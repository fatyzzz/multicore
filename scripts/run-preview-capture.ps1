param(
    [string]$OutputDirectory = "artifacts/screenshots",
    [int]$Port = 18787,
    [ValidateSet("empty", "ready", "connected", "populated-catalog", "error", "selection-pending", "diagnostics", "settings")]
    [string[]]$OnlyScenario
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent (Split-Path -Parent $PSCommandPath)
$fixturePath = [System.IO.Path]::GetFullPath((Join-Path $root "scripts/preview-fixture.ps1"))
$capturePath = [System.IO.Path]::GetFullPath((Join-Path $root "scripts/capture-preview.ps1"))
$desktopPath = [System.IO.Path]::GetFullPath((Join-Path $root "target/release/multicore-desktop.exe"))
$outputPath = [System.IO.Path]::GetFullPath((Join-Path $root $OutputDirectory))
$allowedOutputRoot = [System.IO.Path]::GetFullPath((Join-Path $root "artifacts/screenshots"))
$allowedOutputPrefix = $allowedOutputRoot.TrimEnd([System.IO.Path]::DirectorySeparatorChar) + [System.IO.Path]::DirectorySeparatorChar
$authorizationValue = "preview-local-only"

if (-not (Test-Path -LiteralPath $desktopPath -PathType Leaf)) {
    throw "Release desktop executable not found: $desktopPath"
}
if ($outputPath -ne $allowedOutputRoot -and -not $outputPath.StartsWith($allowedOutputPrefix, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "OutputDirectory must resolve inside $allowedOutputRoot"
}

# A marker argument is ignored by the desktop but lets a later preview run
# distinguish this script's stale desktop from a user-launched release binary.
$previewMarker = "--quiet-signal-preview"
$stalePreviewProcesses = @(Get-CimInstance Win32_Process | Where-Object {
    ($_.ExecutablePath -ieq $desktopPath -and $_.CommandLine -like "*$previewMarker*") -or
    ($_.Name -ieq "powershell.exe" -and $_.CommandLine -like "*$fixturePath*" -and $_.CommandLine -like "*-Port $Port*")
})
foreach ($stale in $stalePreviewProcesses) {
    Stop-Process -Id $stale.ProcessId -Force -ErrorAction Stop
    Wait-Process -Id $stale.ProcessId -Timeout 5 -ErrorAction SilentlyContinue
}

$listener = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue
if ($null -ne $listener) {
    $owners = $listener | Select-Object -ExpandProperty OwningProcess -Unique
    $details = foreach ($owner in $owners) {
        Get-CimInstance Win32_Process -Filter "ProcessId = $owner" | Select-Object ProcessId, ExecutablePath, CommandLine
    }
    throw "Preview port $Port is already occupied. Refusing to terminate an unowned process: $($details | Format-List | Out-String)"
}

$beforeRelated = @(Get-Process -Name "multicore-desktop", "multicore-daemon", "mihomo", "xray" -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Id)
$scenarios = @(
    @{ Name = "empty"; State = "empty"; Width = 700; Height = 660; SelectionDelay = 0; SelectRoute = $false; OpenDiagnostics = $false; OpenSettings = $false },
    @{ Name = "ready"; State = "ready"; Width = 820; Height = 760; SelectionDelay = 0; SelectRoute = $false; OpenDiagnostics = $false; OpenSettings = $false },
    @{ Name = "connected"; State = "connected"; Width = 900; Height = 760; SelectionDelay = 0; SelectRoute = $false; OpenDiagnostics = $false; OpenSettings = $false },
    @{ Name = "populated-catalog"; State = "ready"; Width = 900; Height = 760; SelectionDelay = 0; SelectRoute = $false; OpenDiagnostics = $false; OpenSettings = $false },
    @{ Name = "error"; State = "error"; Width = 700; Height = 660; SelectionDelay = 0; SelectRoute = $false; OpenDiagnostics = $false; OpenSettings = $false },
    @{ Name = "selection-pending"; State = "connected"; Width = 820; Height = 760; SelectionDelay = 5000; SelectRoute = $true; OpenDiagnostics = $false; OpenSettings = $false },
    @{ Name = "diagnostics"; State = "connected"; Width = 900; Height = 760; SelectionDelay = 0; SelectRoute = $false; OpenDiagnostics = $true; OpenSettings = $false },
    @{ Name = "settings"; State = "ready"; Width = 820; Height = 760; SelectionDelay = 0; SelectRoute = $false; OpenDiagnostics = $false; OpenSettings = $true }
)
if ($OnlyScenario.Count -gt 0) {
    $scenarios = @($scenarios | Where-Object { $_.Name -in $OnlyScenario })
}
$captureFileNames = @($scenarios | ForEach-Object { "preview-$($_.Name)-$($_.Width)x$($_.Height).png" })

New-Item -ItemType Directory -Force -Path $outputPath | Out-Null
foreach ($captureFileName in $captureFileNames) {
    $captureFile = Join-Path $outputPath $captureFileName
    if (Test-Path -LiteralPath $captureFile -PathType Leaf) {
        Remove-Item -LiteralPath $captureFile -Force
    }
}

foreach ($scenario in $scenarios) {
    $fixture = $null
    $desktop = $null
    try {
        $fixtureInfo = [System.Diagnostics.ProcessStartInfo]::new()
        $fixtureInfo.FileName = "powershell.exe"
        $fixtureInfo.Arguments = "-NoProfile -ExecutionPolicy Bypass -File `"$fixturePath`" -Port $Port -InitialState $($scenario.State) -SelectionDelayMilliseconds $($scenario.SelectionDelay) -AuthorizationValue $authorizationValue"
        $fixtureInfo.UseShellExecute = $false
        $fixtureInfo.CreateNoWindow = $true
        $fixtureInfo.WindowStyle = [System.Diagnostics.ProcessWindowStyle]::Hidden

        $fixture = [System.Diagnostics.Process]::Start($fixtureInfo)
        Start-Sleep -Milliseconds 500
        if ($fixture.HasExited) {
            throw "Fixture exited before the $($scenario.Name) capture."
        }

        $desktopInfo = [System.Diagnostics.ProcessStartInfo]::new()
        $desktopInfo.FileName = $desktopPath
        $desktopInfo.Arguments = $previewMarker
        $desktopInfo.UseShellExecute = $false
        $desktopInfo.EnvironmentVariables["MULTICORE_DAEMON_URL"] = "http://127.0.0.1:$Port"
        $desktopInfo.EnvironmentVariables["MULTICORE_DAEMON_TOKEN"] = $authorizationValue
        $desktop = [System.Diagnostics.Process]::Start($desktopInfo)

        $captureArgs = @(
            "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", $capturePath,
            "-ProcessId", $desktop.Id,
            "-OutputPath", (Join-Path $outputPath "preview-$($scenario.Name)-$($scenario.Width)x$($scenario.Height).png"),
            "-Width", $scenario.Width,
            "-Height", $scenario.Height,
            "-ExpectedState", $scenario.Name
        )
        if ($scenario.SelectRoute) { $captureArgs += "-SelectSecondRoute" }
        if ($scenario.OpenDiagnostics) { $captureArgs += "-OpenDiagnostics" }
        if ($scenario.OpenSettings) { $captureArgs += "-OpenSettings" }
        & powershell.exe @captureArgs
        if ($LASTEXITCODE -ne 0) {
            throw "Capture helper failed for $($scenario.Name)."
        }
    } finally {
        if ($null -ne $desktop -and -not $desktop.HasExited) {
            [void]$desktop.CloseMainWindow()
            if (-not $desktop.WaitForExit(3000)) {
                $desktop.Kill()
                $desktop.WaitForExit()
            }
        }
        if ($null -ne $fixture -and -not $fixture.HasExited) {
            $fixture.Kill()
            $fixture.WaitForExit()
        }
    }
}

$listenerAfter = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue
if ($null -ne $listenerAfter) {
    throw "Preview fixture still owns port $Port after cleanup."
}
$afterRelated = @(Get-Process -Name "multicore-desktop", "multicore-daemon", "mihomo", "xray" -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Id)
$newRelated = @($afterRelated | Where-Object { $_ -notin $beforeRelated })
if ($newRelated.Count -gt 0) {
    throw "Preview run left related process IDs active: $($newRelated -join ', ')"
}
$staleAfter = @(Get-CimInstance Win32_Process | Where-Object {
    ($_.ExecutablePath -ieq $desktopPath -and $_.CommandLine -like "*$previewMarker*") -or
    ($_.Name -ieq "powershell.exe" -and $_.CommandLine -like "*$fixturePath*" -and $_.CommandLine -like "*-Port $Port*")
})
if ($staleAfter.Count -gt 0) {
    throw "Preview run left fixture/desktop processes active: $($staleAfter.ProcessId -join ', ')"
}
Write-Output "Preview capture cleanup verified: fixture port free and no related process from this run remains."
