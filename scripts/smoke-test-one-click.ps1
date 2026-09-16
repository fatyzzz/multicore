[CmdletBinding()]
param(
    [string]$PackagePath,
    [string]$DesktopExecutableName = 'MultiCore.exe',
    [string]$DiagnosticErrorPath,
    [switch]$CrashCleanup
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Text;

public static class MultiCoreSmokeNative {
    public delegate bool EnumWindowsCallback(IntPtr window, IntPtr state);
    [DllImport("user32.dll")]
    public static extern bool EnumWindows(EnumWindowsCallback callback, IntPtr state);
    [DllImport("user32.dll")]
    public static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);
    [DllImport("user32.dll")]
    public static extern bool IsWindowVisible(IntPtr window);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern int GetWindowText(IntPtr window, StringBuilder text, int maximum);
    [DllImport("user32.dll")]
    public static extern bool PostMessage(IntPtr window, uint message, IntPtr wParam, IntPtr lParam);
}
'@

if (-not $PackagePath) {
    $PackagePath = Join-Path (Split-Path -Parent $PSScriptRoot) 'dist\multicore-windows-x64'
}

function Get-ExactProcess {
    param([string]$ExecutablePath)
    $expected = [IO.Path]::GetFullPath($ExecutablePath)
    @(Get-CimInstance Win32_Process | Where-Object {
        $_.ExecutablePath -and
        [IO.Path]::GetFullPath($_.ExecutablePath).Equals($expected, [StringComparison]::OrdinalIgnoreCase)
    })
}

function Get-MultiCoreWindow {
    param([int]$ProcessId)
    $matches = New-Object 'System.Collections.Generic.List[System.IntPtr]'
    $callback = [MultiCoreSmokeNative+EnumWindowsCallback] {
        param([IntPtr]$window, [IntPtr]$state)
        [uint32]$owner = 0
        [void][MultiCoreSmokeNative]::GetWindowThreadProcessId($window, [ref]$owner)
        if ($owner -eq $ProcessId -and [MultiCoreSmokeNative]::IsWindowVisible($window)) {
            $title = New-Object Text.StringBuilder 256
            [void][MultiCoreSmokeNative]::GetWindowText($window, $title, $title.Capacity)
            if ($title.ToString() -ceq 'MultiCore') { $matches.Add($window) }
        }
        return $true
    }
    [void][MultiCoreSmokeNative]::EnumWindows($callback, [IntPtr]::Zero)
    @($matches)
}

$package = (Resolve-Path -LiteralPath $PackagePath).Path
$desktopExe = [IO.Path]::GetFullPath((Join-Path $package $DesktopExecutableName))
$daemonExe = [IO.Path]::GetFullPath((Join-Path $package 'runtime\multicore-daemon.exe'))
$xrayExe = [IO.Path]::GetFullPath((Join-Path $package 'cores\xray.exe'))
$mihomoExe = [IO.Path]::GetFullPath((Join-Path $package 'cores\mihomo.exe'))
foreach ($file in @($desktopExe, $daemonExe, $xrayExe, $mihomoExe)) {
    if (-not [IO.File]::Exists($file)) { throw "Package executable is missing: $file" }
    if ((Get-Item -LiteralPath $file -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw "Package executable is a reparse point: $file"
    }
}

$smokeRoot = Join-Path ([IO.Path]::GetTempPath()) ('multicore-task4-smoke-' + [Guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($smokeRoot) | Out-Null
$hadLocal = Test-Path Env:LOCALAPPDATA
$savedLocal = $env:LOCALAPPDATA
$hadUrl = Test-Path Env:MULTICORE_DAEMON_URL
$savedUrl = $env:MULTICORE_DAEMON_URL
$hadToken = Test-Path Env:MULTICORE_DAEMON_TOKEN
$savedToken = $env:MULTICORE_DAEMON_TOKEN
$hadController = Test-Path Env:MULTICORE_MIHOMO_CONTROLLER_ADDR
$savedController = $env:MULTICORE_MIHOMO_CONTROLLER_ADDR
$hadSmokeClose = Test-Path Env:MULTICORE_SMOKE_EXIT_ON_CLOSE
$savedSmokeClose = $env:MULTICORE_SMOKE_EXIT_ON_CLOSE
$desktop = $null
$daemonPid = $null
$succeeded = $false

try {
    $env:LOCALAPPDATA = $smokeRoot
    Remove-Item Env:MULTICORE_DAEMON_URL -ErrorAction SilentlyContinue
    Remove-Item Env:MULTICORE_DAEMON_TOKEN -ErrorAction SilentlyContinue
    $env:MULTICORE_MIHOMO_CONTROLLER_ADDR = '192.0.2.1:19090'
    $env:MULTICORE_SMOKE_EXIT_ON_CLOSE = '1'

    $startArguments = @{
        FilePath = $desktopExe
        WorkingDirectory = $package
        PassThru = $true
    }
    if ($DiagnosticErrorPath) {
        $startArguments.RedirectStandardError = [IO.Path]::GetFullPath($DiagnosticErrorPath)
    }
    $desktop = Start-Process @startArguments
    $windowDeadline = [DateTime]::UtcNow.AddSeconds(20)
    do {
        Start-Sleep -Milliseconds 100
        $desktop.Refresh()
        $windows = @(Get-MultiCoreWindow $desktop.Id)
    } until ($desktop.HasExited -or $windows.Count -eq 1 -or [DateTime]::UtcNow -ge $windowDeadline)
    if ($desktop.HasExited -or $windows.Count -ne 1) {
        throw 'GUI did not appear after the authenticated bootstrap deadline'
    }

    $childDeadline = [DateTime]::UtcNow.AddSeconds(10)
    do {
        $children = @(Get-CimInstance Win32_Process | Where-Object {
            $_.ParentProcessId -eq $desktop.Id -and
            $_.ExecutablePath -and
            [IO.Path]::GetFullPath($_.ExecutablePath).Equals($daemonExe, [StringComparison]::OrdinalIgnoreCase)
        })
        if ($children.Count -ne 1) { Start-Sleep -Milliseconds 100 }
    } until ($children.Count -eq 1 -or [DateTime]::UtcNow -ge $childDeadline)
    if ($children.Count -ne 1) { throw "Expected one exact daemon child; observed $($children.Count)" }
    $daemonPid = [int]$children[0].ProcessId

    $listenerDeadline = [DateTime]::UtcNow.AddSeconds(10)
    do {
        $listeners = @(Get-NetTCPConnection -State Listen -OwningProcess $daemonPid -ErrorAction SilentlyContinue |
            Where-Object { $_.LocalAddress -eq '127.0.0.1' })
        if ($listeners.Count -ne 1) { Start-Sleep -Milliseconds 100 }
    } until ($listeners.Count -eq 1 -or [DateTime]::UtcNow -ge $listenerDeadline)
    if ($listeners.Count -ne 1) { throw "Expected one daemon loopback listener; observed $($listeners.Count)" }

    $status = $null
    try {
        $response = Invoke-WebRequest -UseBasicParsing -TimeoutSec 3 -Uri ("http://127.0.0.1:{0}/v1/status" -f $listeners[0].LocalPort)
        $status = [int]$response.StatusCode
    } catch {
        if ($_.Exception.Response) {
            $status = [int]$_.Exception.Response.StatusCode
        } else {
            throw
        }
    }
    if ($status -ne 401) { throw "Tokenless daemon status returned $status instead of 401" }

    $coreLog = Join-Path $smokeRoot 'MultiCore\logs\latest-core.log'
    if (-not [IO.File]::Exists($coreLog)) { throw 'Daemon did not create the bounded latest-core.log diagnostics sink' }
    if ((Get-Item -LiteralPath $coreLog).Length -gt 1048576) { throw 'latest-core.log exceeded its one-mebibyte bound' }

    if ($CrashCleanup) {
        Stop-Process -Id $desktop.Id -Force
        if (-not $desktop.WaitForExit(5000)) { throw 'Desktop process could not be terminated by exact PID' }
        $shutdown = 'forced-crash-check'
    } else {
        if (-not [MultiCoreSmokeNative]::PostMessage($windows[0], 0x0010, [IntPtr]::Zero, [IntPtr]::Zero)) {
            throw 'Desktop rejected the normal close request'
        }
        if (-not $desktop.WaitForExit(10000)) { throw 'Desktop did not exit after the normal close request' }
        $shutdown = 'graceful'
    }
    $leakDeadline = [DateTime]::UtcNow.AddSeconds(5)
    do {
        $leaks = @()
        foreach ($path in @($desktopExe, $daemonExe, $xrayExe, $mihomoExe)) {
            $leaks += @(Get-ExactProcess $path)
        }
        if ($leaks.Count -ne 0) { Start-Sleep -Milliseconds 100 }
    } until ($leaks.Count -eq 0 -or [DateTime]::UtcNow -ge $leakDeadline)
    if ($leaks.Count -ne 0) {
        $leaks | Select-Object ProcessId, ParentProcessId, ExecutablePath
        throw 'An exact-package process remained after desktop shutdown'
    }

    $succeeded = $true
    [pscustomobject]@{
        DesktopPid = $desktop.Id
        DaemonPid = $daemonPid
        ApiPort = $listeners[0].LocalPort
        TokenlessStatus = $status
        CoreLog = 'created'
        DesktopShutdown = $shutdown
        Cleanup = 'verified'
    } | Format-List
} finally {
    if ($desktop -and -not $desktop.HasExited) {
        Stop-Process -Id $desktop.Id -Force -ErrorAction SilentlyContinue
        [void]$desktop.WaitForExit(5000)
    }
    if ($daemonPid) {
        $exactDaemon = @(Get-ExactProcess $daemonExe | Where-Object { $_.ProcessId -eq $daemonPid })
        if ($exactDaemon.Count -eq 1) {
            Stop-Process -Id $daemonPid -Force -ErrorAction SilentlyContinue
        }
    }

    if ($hadLocal) { $env:LOCALAPPDATA = $savedLocal } else { Remove-Item Env:LOCALAPPDATA -ErrorAction SilentlyContinue }
    if ($hadUrl) { $env:MULTICORE_DAEMON_URL = $savedUrl } else { Remove-Item Env:MULTICORE_DAEMON_URL -ErrorAction SilentlyContinue }
    if ($hadToken) { $env:MULTICORE_DAEMON_TOKEN = $savedToken } else { Remove-Item Env:MULTICORE_DAEMON_TOKEN -ErrorAction SilentlyContinue }
    if ($hadController) { $env:MULTICORE_MIHOMO_CONTROLLER_ADDR = $savedController } else { Remove-Item Env:MULTICORE_MIHOMO_CONTROLLER_ADDR -ErrorAction SilentlyContinue }
    if ($hadSmokeClose) { $env:MULTICORE_SMOKE_EXIT_ON_CLOSE = $savedSmokeClose } else { Remove-Item Env:MULTICORE_SMOKE_EXIT_ON_CLOSE -ErrorAction SilentlyContinue }

    if ($succeeded) {
        $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') + '\'
        $smokeFull = [IO.Path]::GetFullPath($smokeRoot)
        if ($smokeFull.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -and
            [IO.Path]::GetFileName($smokeFull).StartsWith('multicore-task4-smoke-', [StringComparison]::Ordinal)) {
            Remove-Item -LiteralPath $smokeFull -Recurse -Force
        }
    }
}
