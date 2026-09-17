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

function ConvertTo-NativeQuotedArgument {
    param([Parameter(Mandatory = $true)][string]$Value)

    Assert-True (-not $Value.Contains('"')) 'native process argument must not contain a quote'
    return '"' + $Value + '"'
}

function Assert-NoCredentialLeak {
    param([Parameter(Mandatory = $true)][string[]]$Paths)

    $maximumDiagnosticBytes = 1MB
    foreach ($path in $Paths) {
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { continue }
        $diagnostic = Get-Item -LiteralPath $path
        Assert-True ($diagnostic.Length -le $maximumDiagnosticBytes) `
            "diagnostic output exceeds the bounded read limit: $($diagnostic.FullName)"
        $contents = [IO.File]::ReadAllText($diagnostic.FullName)
        Assert-True (-not $contents.Contains('private-token')) `
            "diagnostic output contains the subscription credential marker: $($diagnostic.Name)"
        Assert-True (-not $contents.Contains('https://sentinel.invalid/private-token')) `
            "diagnostic output contains the credential-bearing subscription URL: $($diagnostic.Name)"
    }
}

function Get-FileHashManifest {
    param([Parameter(Mandatory = $true)][string]$Root)

    $resolvedRoot = [IO.Path]::GetFullPath($Root).TrimEnd('\', '/')
    return @(Get-ChildItem -LiteralPath $resolvedRoot -File -Recurse |
        ForEach-Object {
            $relative = $_.FullName.Substring($resolvedRoot.Length).TrimStart('\', '/').Replace('\', '/')
            [PSCustomObject]@{
                RelativePath = $relative
                Sha256 = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
            }
        } |
        Sort-Object -Property RelativePath)
}

function Assert-FileHashManifestEqual {
    param(
        [Parameter(Mandatory = $true)][object[]]$Expected,
        [Parameter(Mandatory = $true)][object[]]$Actual
    )

    Assert-True ($Actual.Count -eq $Expected.Count) "mutable data file count changed from $($Expected.Count) to $($Actual.Count)"
    for ($index = 0; $index -lt $Expected.Count; $index++) {
        Assert-True ($Actual[$index].RelativePath -ceq $Expected[$index].RelativePath) `
            "mutable data file set changed at '$($Expected[$index].RelativePath)'"
        Assert-True ($Actual[$index].Sha256 -ceq $Expected[$index].Sha256) `
            "mutable data file changed at '$($Expected[$index].RelativePath)'"
    }
}

$installer = [IO.Path]::GetFullPath($ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($InstallerPath))
$package = [IO.Path]::GetFullPath($ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($PackagePath))
Assert-True (Test-Path -LiteralPath $installer -PathType Leaf) 'installer must exist'
Assert-True (Test-Path -LiteralPath $package -PathType Container) 'portable package must exist'

$temporaryBase = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
$testRoot = Join-Path $temporaryBase ('multicore-installed-update-smoke ' + [Guid]::NewGuid().ToString('N'))
$installRoot = Join-Path $testRoot 'installation'
$foreignInstallRoot = Join-Path $testRoot 'foreign-installation'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$protocolKey = 'HKCU:\Software\Classes\multicore'
$foreignAutostart = '"C:\Foreign Tool\agent.exe" --background'

try {
    New-Item -ItemType Directory -Path $testRoot | Out-Null
    $install = Start-Process -FilePath $installer `
        -ArgumentList @(
            '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/TASKS=""',
            ('/DIR=' + (ConvertTo-NativeQuotedArgument $installRoot))) `
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

    $testLocalAppData = Join-Path $testRoot 'local-app-data'
    $mutableRoot = Join-Path $testLocalAppData 'MultiCore'
    $profileGeneration = Join-Path $mutableRoot 'profiles\profile-smoke\snapshot-00000000000000000001'
    New-Item -ItemType Directory -Path $profileGeneration -Force | Out-Null
    New-Item -ItemType Directory -Path (Join-Path $mutableRoot 'logs') -Force | Out-Null
    $utf8NoBom = [Text.UTF8Encoding]::new($false)
    [IO.File]::WriteAllText(
        (Join-Path $mutableRoot 'preferences.json'),
        "{`n  `"schema_version`": 1,`n  `"launch_on_startup`": false,`n  `"theme`": `"system`"`n}`n",
        $utf8NoBom)
    [IO.File]::WriteAllText(
        (Join-Path $mutableRoot 'profiles\index.json'),
        "{`n  `"schema_version`": 1,`n  `"active_profile_id`": `"profile-smoke`",`n  `"profiles`": [{`"id`":`"profile-smoke`",`"current_generation`":1}]`n}`n",
        $utf8NoBom)
    [IO.File]::WriteAllText(
        (Join-Path $profileGeneration 'subscription.json'),
        "{`n  `"source_url`": `"https://sentinel.invalid/private-token`",`n  `"info`": {`"source_host`":`"sentinel.invalid`",`"updated_at_unix`":1757959200},`n  `"targets`": {}`n}`n",
        $utf8NoBom)
    [IO.File]::WriteAllText(
        (Join-Path $profileGeneration 'selections.json'),
        "{`n  `"schema_version`": 1,`n  `"revision`": 1,`n  `"selections`": {`"proxy-group`": `"node-smoke`"}`n}`n",
        $utf8NoBom)
    [IO.File]::WriteAllText(
        (Join-Path $mutableRoot 'device-identity'),
        "multicore-smoke-device-00000001`n",
        $utf8NoBom)
    [IO.File]::WriteAllText(
        (Join-Path $mutableRoot 'logs\latest-core.log'),
        "2025-09-15T12:00:00Z INFO smoke sentinel log line`n",
        $utf8NoBom)
    $mutableManifestBefore = Get-FileHashManifest -Root $mutableRoot
    Assert-True ($mutableManifestBefore.Count -eq 6) 'mutable data fixture must contain all six sentinel files'

    $updaterStdout = Join-Path $testRoot 'updater-stdout.log'
    $updaterStderr = Join-Path $testRoot 'updater-stderr.log'
    $previousLocalAppData = [Environment]::GetEnvironmentVariable('LOCALAPPDATA', 'Process')
    try {
        [Environment]::SetEnvironmentVariable('LOCALAPPDATA', $testLocalAppData, 'Process')
        $apply = Start-Process -FilePath $helper `
            -ArgumentList @(
                '--wait-pid', '4294967295',
                '--archive', (ConvertTo-NativeQuotedArgument $archive),
                '--target', (ConvertTo-NativeQuotedArgument $current),
                '--size', [string]$archiveSize,
                '--sha256', $archiveHash,
                '--version', '0.1.1') `
            -RedirectStandardOutput $updaterStdout `
            -RedirectStandardError $updaterStderr `
            -Wait -PassThru -WindowStyle Hidden
    } finally {
        [Environment]::SetEnvironmentVariable('LOCALAPPDATA', $previousLocalAppData, 'Process')
    }
    Assert-True ($apply.ExitCode -eq 0) "packaged updater exited with $($apply.ExitCode)"

    $mutableManifestAfter = Get-FileHashManifest -Root $mutableRoot
    Assert-FileHashManifestEqual -Expected $mutableManifestBefore -Actual $mutableManifestAfter

    $stateReader = Join-Path $testRoot 'read persisted state.ps1'
    $stateSignal = Join-Path $testRoot 'state-read.ok'
    $stateReaderSource = @'
[CmdletBinding()]
param([Parameter(Mandatory = $true)][string]$SignalPath)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Assert-State {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw $Message }
}

$root = Join-Path ([Environment]::GetEnvironmentVariable('LOCALAPPDATA', 'Process')) 'MultiCore'
$preferences = Get-Content -Raw -LiteralPath (Join-Path $root 'preferences.json') | ConvertFrom-Json
Assert-State ($preferences.schema_version -eq 1) 'unexpected preferences schema'
Assert-State ($preferences.launch_on_startup -eq $false) 'unexpected startup preference'
Assert-State ($preferences.theme -ceq 'system') 'unexpected theme preference'

$index = Get-Content -Raw -LiteralPath (Join-Path $root 'profiles\index.json') | ConvertFrom-Json
Assert-State ($index.schema_version -eq 1) 'unexpected profiles schema'
Assert-State ($index.active_profile_id -ceq 'profile-smoke') 'unexpected active profile'
$activeProfile = @($index.profiles | Where-Object { $_.id -ceq $index.active_profile_id })
Assert-State ($activeProfile.Count -eq 1) 'active profile entry must be unique'
Assert-State ([long]$activeProfile[0].current_generation -eq 1) 'unexpected active generation'
$generationName = 'snapshot-{0:D20}' -f [long]$activeProfile[0].current_generation
$generation = Join-Path $root (Join-Path ('profiles\' + $index.active_profile_id) $generationName)

$subscription = Get-Content -Raw -LiteralPath (Join-Path $generation 'subscription.json') | ConvertFrom-Json
Assert-State ($subscription.source_url -ceq 'https://sentinel.invalid/private-token') 'subscription sentinel changed'
Assert-State ($subscription.info.source_host -ceq 'sentinel.invalid') 'subscription host changed'
Assert-State ([long]$subscription.info.updated_at_unix -eq 1757959200) 'subscription timestamp changed'
$selections = Get-Content -Raw -LiteralPath (Join-Path $generation 'selections.json') | ConvertFrom-Json
Assert-State ($selections.schema_version -eq 1) 'unexpected selections schema'
Assert-State ([long]$selections.revision -eq 1) 'unexpected selections revision'
Assert-State ($selections.selections.'proxy-group' -ceq 'node-smoke') 'selected node changed'
Assert-State (([IO.File]::ReadAllText((Join-Path $root 'device-identity'))) -ceq "multicore-smoke-device-00000001`n") 'device identity changed'
Assert-State (([IO.File]::ReadAllText((Join-Path $root 'logs\latest-core.log'))) -ceq "2025-09-15T12:00:00Z INFO smoke sentinel log line`n") 'core log changed'

$signalTemporary = $SignalPath + '.tmp'
[IO.File]::WriteAllText($signalTemporary, 'multicore-state-readable-v1', [Text.UTF8Encoding]::new($false))
[IO.File]::Move($signalTemporary, $SignalPath)
'@
    [IO.File]::WriteAllText($stateReader, $stateReaderSource, $utf8NoBom)
    $stateReaderStdout = Join-Path $testRoot 'state-reader-stdout.log'
    $stateReaderStderr = Join-Path $testRoot 'state-reader-stderr.log'
    $previousLocalAppData = [Environment]::GetEnvironmentVariable('LOCALAPPDATA', 'Process')
    try {
        [Environment]::SetEnvironmentVariable('LOCALAPPDATA', $testLocalAppData, 'Process')
        $stateRead = Start-Process -FilePath 'powershell.exe' `
            -ArgumentList @(
                '-NoProfile', '-ExecutionPolicy', 'Bypass',
                '-File', (ConvertTo-NativeQuotedArgument $stateReader),
                '-SignalPath', (ConvertTo-NativeQuotedArgument $stateSignal)) `
            -RedirectStandardOutput $stateReaderStdout `
            -RedirectStandardError $stateReaderStderr `
            -Wait -PassThru -WindowStyle Hidden
    } finally {
        [Environment]::SetEnvironmentVariable('LOCALAPPDATA', $previousLocalAppData, 'Process')
    }
    Assert-True ($stateRead.ExitCode -eq 0) "persisted state reader exited with $($stateRead.ExitCode)"
    Assert-True (Test-Path -LiteralPath $stateSignal -PathType Leaf) 'persisted state reader must emit a success signal'
    Assert-True ((Get-Item -LiteralPath $stateSignal).Length -le 64) 'persisted state reader signal must remain bounded'
    Assert-True ([IO.File]::ReadAllText($stateSignal) -ceq 'multicore-state-readable-v1') 'persisted state reader signal must be exact'

    Assert-NoCredentialLeak -Paths @(
        $updaterStdout,
        $updaterStderr,
        (Join-Path $testRoot 'updater.log'),
        (Join-Path $testRoot 'result.json'),
        $stateReaderStdout,
        $stateReaderStderr)

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
        -ArgumentList @(
            '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/TASKS=""',
            ('/DIR=' + (ConvertTo-NativeQuotedArgument $foreignInstallRoot))) `
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

    Write-Output 'PASS: packaged updater preserves mutable data without credential leakage and ownership-safe uninstall cleanup'
} finally {
    $resolvedRoot = [IO.Path]::GetFullPath($testRoot)
    $temporaryPrefix = $temporaryBase.TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
    $resolvedRootName = [IO.Path]::GetFileName($resolvedRoot.TrimEnd('\', '/'))
    if ($resolvedRoot.StartsWith($temporaryPrefix, [StringComparison]::OrdinalIgnoreCase) -and
        $resolvedRootName -match '^multicore-installed-update-smoke [0-9a-f]{32}$' -and
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
