[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$InstallerPath,
    [Parameter(Mandatory = $true)]
    [string]$PackagePath
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw "FAIL: $Message" }
}

$installer = [IO.Path]::GetFullPath($ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($InstallerPath))
$package = [IO.Path]::GetFullPath($ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($PackagePath))
Assert-True (Test-Path -LiteralPath $installer -PathType Leaf) 'installer must exist'
Assert-True (Test-Path -LiteralPath $package -PathType Container) 'portable package must exist'

$temporaryBase = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
$testRoot = Join-Path $temporaryBase ('multicore-installed-update-smoke-' + [Guid]::NewGuid().ToString('N'))
$installRoot = Join-Path $testRoot 'installation'
$foreignInstallRoot = Join-Path $testRoot 'foreign-installation'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$protocolKey = 'HKCU:\Software\Classes\multicore'
$foreignAutostart = '"C:\Foreign Tool\agent.exe" --background'

try {
    New-Item -ItemType Directory -Path $testRoot | Out-Null
    $install = Start-Process -FilePath $installer `
        -ArgumentList @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/TASKS=""', "/DIR=$installRoot") `
        -Wait -PassThru -WindowStyle Hidden
    Assert-True ($install.ExitCode -eq 0) "installer exited with $($install.ExitCode)"

    $current = Join-Path $installRoot 'current'
    $uninstaller = Join-Path $installRoot 'unins000.exe'
    $uninstallerData = Join-Path $installRoot 'unins000.dat'
    Assert-True (Test-Path -LiteralPath (Join-Path $current 'MultiCore.exe') -PathType Leaf) 'installed app must live under current'
    Assert-True (Test-Path -LiteralPath $uninstaller -PathType Leaf) 'uninstaller executable must remain at install root'
    Assert-True (Test-Path -LiteralPath $uninstallerData -PathType Leaf) 'uninstaller metadata must remain at install root'
    $uninstallerHash = (Get-FileHash -LiteralPath $uninstaller -Algorithm SHA256).Hash
    $uninstallerDataHash = (Get-FileHash -LiteralPath $uninstallerData -Algorithm SHA256).Hash

    $helper = Join-Path $testRoot 'multicore-apply.exe'
    Copy-Item -LiteralPath (Join-Path $current 'runtime\multicore-updater.exe') -Destination $helper
    # Keep updater failure recovery inert: the updater helper exits immediately when launched
    # without an apply request, unlike the real desktop which could start packaged sidecars.
    Copy-Item -LiteralPath $helper -Destination (Join-Path $current 'MultiCore.exe') -Force
    $updatePayload = Join-Path $testRoot 'update-payload'
    Copy-Item -LiteralPath $package -Destination $updatePayload -Recurse
    Copy-Item -LiteralPath (Join-Path $updatePayload 'runtime\multicore-updater.exe') `
        -Destination (Join-Path $updatePayload 'MultiCore.exe') -Force
    $sumPath = Join-Path $updatePayload 'SHA256SUMS.txt'
    $sumLines = Get-ChildItem -LiteralPath $updatePayload -File -Recurse |
        Where-Object { $_.FullName -cne $sumPath } |
        ForEach-Object {
            $relative = $_.FullName.Substring($updatePayload.Length).TrimStart('\', '/').Replace('\', '/')
            $hash = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
            [PSCustomObject]@{ Relative = $relative; Line = "$hash *$relative" }
        } |
        Sort-Object -Property Relative |
        Select-Object -ExpandProperty Line
    [IO.File]::WriteAllLines($sumPath, $sumLines, [Text.UTF8Encoding]::new($false))
    $archive = Join-Path $testRoot 'update.zip'
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archiveStream = [IO.File]::Open($archive, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try {
        $zip = [IO.Compression.ZipArchive]::new($archiveStream, [IO.Compression.ZipArchiveMode]::Create, $false)
        try {
            foreach ($file in Get-ChildItem -LiteralPath $updatePayload -File -Recurse | Sort-Object -Property FullName) {
                $entryName = $file.FullName.Substring($updatePayload.Length).TrimStart('\', '/').Replace('\', '/')
                [IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
                    $zip,
                    $file.FullName,
                    $entryName,
                    [IO.Compression.CompressionLevel]::Optimal) | Out-Null
            }
        } finally {
            $zip.Dispose()
        }
    } finally {
        $archiveStream.Dispose()
    }
    $archiveSize = (Get-Item -LiteralPath $archive).Length
    $archiveHash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()

    $apply = Start-Process -FilePath $helper `
        -ArgumentList @(
            '--wait-pid', '4294967295',
            '--archive', $archive,
            '--target', $current,
            '--size', [string]$archiveSize,
            '--sha256', $archiveHash,
            '--version', '0.1.1') `
        -Wait -PassThru -WindowStyle Hidden
    Assert-True ($apply.ExitCode -eq 0) "packaged updater exited with $($apply.ExitCode)"

    Assert-True (Test-Path -LiteralPath (Join-Path $current 'MultiCore.exe') -PathType Leaf) 'updated payload must be published back to current'
    Assert-True ((Get-FileHash -LiteralPath $uninstaller -Algorithm SHA256).Hash -ceq $uninstallerHash) 'app update must preserve the uninstaller executable'
    Assert-True ((Get-FileHash -LiteralPath $uninstallerData -Algorithm SHA256).Hash -ceq $uninstallerDataHash) 'app update must preserve uninstaller metadata'
    Assert-True (@(Get-ChildItem -LiteralPath $installRoot -Directory -Filter '.multicore-*').Count -eq 0) 'successful updater must clean staging and previous-version directories'

    $protocolCommand = (Get-ItemPropertyValue -LiteralPath (Join-Path $protocolKey 'shell\open\command') -Name '(default)')
    Assert-True ($protocolCommand -ceq ('"' + (Join-Path $current 'MultiCore.exe') + '" "%1"')) 'URL protocol must target current payload'

    $expectedAutostart = '"' + (Join-Path $current 'MultiCore.exe') + '" --background'
    New-Item -Path $runKey -Force | Out-Null
    New-ItemProperty -LiteralPath $runKey -Name 'MultiCore' -PropertyType String -Value $expectedAutostart -Force | Out-Null
    $abandonedStage = Join-Path $installRoot '.multicore-stage-smoke'
    New-Item -ItemType Directory -Path $abandonedStage | Out-Null
    [IO.File]::WriteAllText((Join-Path $abandonedStage 'leftover.txt'), 'staged')

    $uninstall = Start-Process -FilePath $uninstaller `
        -ArgumentList @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART') `
        -Wait -PassThru -WindowStyle Hidden
    Assert-True ($uninstall.ExitCode -eq 0) "uninstaller exited with $($uninstall.ExitCode)"

    Assert-True (-not (Test-Path -LiteralPath $installRoot)) 'uninstall must remove current payload, backup, and install root'
    Assert-True (-not (Test-Path -LiteralPath $protocolKey)) 'uninstall must remove URL protocol registration'
    $remainingAutostart = Get-ItemProperty -LiteralPath $runKey -Name 'MultiCore' -ErrorAction SilentlyContinue
    Assert-True ($null -eq $remainingAutostart) 'uninstall must remove app-owned autostart value even when enabled after install'

    $foreignInstall = Start-Process -FilePath $installer `
        -ArgumentList @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/TASKS=""', "/DIR=$foreignInstallRoot") `
        -Wait -PassThru -WindowStyle Hidden
    Assert-True ($foreignInstall.ExitCode -eq 0) "second installer exited with $($foreignInstall.ExitCode)"
    New-ItemProperty -LiteralPath $runKey -Name 'MultiCore' -PropertyType String -Value $foreignAutostart -Force | Out-Null
    $foreignUninstaller = Join-Path $foreignInstallRoot 'unins000.exe'
    $foreignUninstall = Start-Process -FilePath $foreignUninstaller `
        -ArgumentList @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART') `
        -Wait -PassThru -WindowStyle Hidden
    Assert-True ($foreignUninstall.ExitCode -eq 0) "second uninstaller exited with $($foreignUninstall.ExitCode)"
    $preservedAutostart = Get-ItemPropertyValue -LiteralPath $runKey -Name 'MultiCore'
    Assert-True ($preservedAutostart -ceq $foreignAutostart) 'uninstall must preserve a foreign replacement autostart value'

    Write-Output 'PASS: packaged updater boundary and ownership-safe uninstall cleanup'
} finally {
    $resolvedRoot = [IO.Path]::GetFullPath($testRoot)
    $temporaryPrefix = $temporaryBase.TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
    if ($resolvedRoot.StartsWith($temporaryPrefix, [StringComparison]::OrdinalIgnoreCase) -and
        $resolvedRoot.Contains('multicore-installed-update-smoke-') -and
        (Test-Path -LiteralPath $resolvedRoot)) {
        $leftoverUninstaller = Join-Path $installRoot 'unins000.exe'
        if (Test-Path -LiteralPath $leftoverUninstaller -PathType Leaf) {
            $cleanup = Start-Process -FilePath $leftoverUninstaller `
                -ArgumentList @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART') `
                -Wait -PassThru -WindowStyle Hidden
        }
        $foreignLeftoverUninstaller = Join-Path $foreignInstallRoot 'unins000.exe'
        if (Test-Path -LiteralPath $foreignLeftoverUninstaller -PathType Leaf) {
            $foreignCleanup = Start-Process -FilePath $foreignLeftoverUninstaller `
                -ArgumentList @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART') `
                -Wait -PassThru -WindowStyle Hidden
        }
        if (Test-Path -LiteralPath $resolvedRoot) {
            Remove-Item -LiteralPath $resolvedRoot -Recurse -Force
        }
    }
    $currentAutostart = Get-ItemProperty -LiteralPath $runKey -Name 'MultiCore' -ErrorAction SilentlyContinue
    if ($null -ne $currentAutostart -and $currentAutostart.MultiCore -ceq $foreignAutostart) {
        Remove-ItemProperty -LiteralPath $runKey -Name 'MultiCore' -Force
    }
}
