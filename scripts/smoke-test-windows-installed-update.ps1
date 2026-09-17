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
    $profileId = '11111111-2222-4333-8444-555555555555'
    $profileRoot = Join-Path $mutableRoot (Join-Path 'profiles' $profileId)
    $profileGeneration = Join-Path $profileRoot 'generations\snapshot-00000000000000000001'
    New-Item -ItemType Directory -Path $profileGeneration -Force | Out-Null
    New-Item -ItemType Directory -Path (Join-Path $mutableRoot 'logs') -Force | Out-Null
    $utf8NoBom = [Text.UTF8Encoding]::new($false)
    # This fixture uses the approved multi-sub disk contract. Task 7 must rerun this
    # artifact smoke against the production ProfileStore after that loader lands.
    [IO.File]::WriteAllText(
        (Join-Path $mutableRoot 'preferences.json'),
        "{`n  `"schema_version`": 1,`n  `"restored_bounds`": {`"x`":40,`"y`":60,`"width`":920,`"height`":700},`n  `"maximized`": true,`n  `"visible_page`": `"status`",`n  `"active_profile_hint`": `"$profileId`",`n  `"last_group_by_profile`": {`"$profileId`":`"proxy-group`"},`n  `"selections_by_profile`": {`"$profileId`":{`"proxy-group`":`"node-smoke`"}},`n  `"ambient_background`": true`n}`n",
        $utf8NoBom)
    [IO.File]::WriteAllText(
        (Join-Path $mutableRoot 'profiles\index.json'),
        "{`n  `"schema_version`": 1,`n  `"active_profile_id`": `"$profileId`",`n  `"profiles`": [{`"id`":`"$profileId`",`"created_at_unix`":1757959100}]`n}`n",
        $utf8NoBom)
    [IO.File]::WriteAllText(
        (Join-Path $profileGeneration 'mihomo.yaml'),
        "proxies:`n  - name: node-smoke`n    type: socks5`n    server: 127.0.0.1`n    port: 1080`nproxy-groups:`n  - name: proxy-group`n    type: select`n    proxies:`n      - node-smoke`nrules:`n  - MATCH,proxy-group`n",
        $utf8NoBom)
    [IO.File]::WriteAllText(
        (Join-Path $profileGeneration 'xray.json'),
        "{`n  `"log`": {`"loglevel`":`"warning`"},`n  `"outbounds`": []`n}`n",
        $utf8NoBom)
    [IO.File]::WriteAllText(
        (Join-Path $profileGeneration 'subscription.json'),
        "{`n  `"source_url`": `"https://sentinel.invalid/private-token`",`n  `"info`": {`n    `"source_host`": `"sentinel.invalid`",`n    `"display_name`": `"Smoke Subscription`",`n    `"uploaded_bytes`": null,`n    `"downloaded_bytes`": 1024,`n    `"total_bytes`": 4096,`n    `"expires_at_unix`": null,`n    `"updated_at_unix`": 1757959200,`n    `"refresh_interval_secs`": 3600,`n    `"announcement_text`": null,`n    `"announcement_action_label`": null,`n    `"announcement_tone`": null`n  },`n  `"targets`": {`n    `"home_url`": null,`n    `"support_url`": null,`n    `"announcement_url`": null,`n    `"logo_url`": null`n  }`n}`n",
        $utf8NoBom)
    [IO.File]::WriteAllText(
        (Join-Path $profileRoot 'selections.json'),
        "{`n  `"schema_version`": 1,`n  `"catalog_revision`": 73,`n  `"selections`": {`"proxy-group`": `"node-smoke`"}`n}`n",
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
    Assert-True ($mutableManifestBefore.Count -eq 8) 'mutable data fixture must contain all eight sentinel files'

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
$expectedProfileId = '11111111-2222-4333-8444-555555555555'
$preferences = Get-Content -Raw -LiteralPath (Join-Path $root 'preferences.json') | ConvertFrom-Json
$preferenceKeys = @($preferences.PSObject.Properties.Name | Sort-Object)
Assert-State (($preferenceKeys -join ',') -ceq 'active_profile_hint,ambient_background,last_group_by_profile,maximized,restored_bounds,schema_version,selections_by_profile,visible_page') 'unexpected preferences fields'
Assert-State ($preferences.schema_version -eq 1) 'unexpected preferences schema'
Assert-State ($preferences.schema_version -is [int]) 'preferences schema must be an integer'
$restoredBoundsKeys = @($preferences.restored_bounds.PSObject.Properties.Name | Sort-Object)
Assert-State (($restoredBoundsKeys -join ',') -ceq 'height,width,x,y') 'unexpected restored-bounds fields'
Assert-State ($preferences.restored_bounds.x -eq 40 -and $preferences.restored_bounds.x -is [int]) 'unexpected restored x bound'
Assert-State ($preferences.restored_bounds.y -eq 60 -and $preferences.restored_bounds.y -is [int]) 'unexpected restored y bound'
Assert-State ($preferences.restored_bounds.width -eq 920 -and $preferences.restored_bounds.width -is [int]) 'unexpected restored width'
Assert-State ($preferences.restored_bounds.height -eq 700 -and $preferences.restored_bounds.height -is [int]) 'unexpected restored height'
Assert-State ($preferences.maximized -is [bool] -and $preferences.maximized) 'unexpected maximized preference'
Assert-State ($preferences.visible_page -is [string] -and $preferences.visible_page -ceq 'status') 'unexpected visible page'
Assert-State ($preferences.active_profile_hint -is [string] -and $preferences.active_profile_hint -ceq $expectedProfileId) 'unexpected active profile hint'
Assert-State (@($preferences.last_group_by_profile.PSObject.Properties).Count -eq 1) 'unexpected last-group profile count'
Assert-State ($preferences.last_group_by_profile.$expectedProfileId -is [string] -and $preferences.last_group_by_profile.$expectedProfileId -ceq 'proxy-group') 'unexpected last group ID'
Assert-State (@($preferences.selections_by_profile.PSObject.Properties).Count -eq 1) 'unexpected selection profile count'
Assert-State (@($preferences.selections_by_profile.$expectedProfileId.PSObject.Properties).Count -eq 1) 'unexpected preference selection count'
Assert-State ($preferences.selections_by_profile.$expectedProfileId.'proxy-group' -is [string] -and $preferences.selections_by_profile.$expectedProfileId.'proxy-group' -ceq 'node-smoke') 'unexpected preference selected node ID'
Assert-State ($preferences.ambient_background -is [bool] -and $preferences.ambient_background) 'unexpected ambient background preference'

$index = Get-Content -Raw -LiteralPath (Join-Path $root 'profiles\index.json') | ConvertFrom-Json
Assert-State ((@($index.PSObject.Properties.Name | Sort-Object) -join ',') -ceq 'active_profile_id,profiles,schema_version') 'unexpected profile index fields'
Assert-State ($index.schema_version -eq 1) 'unexpected profiles schema'
Assert-State ($index.schema_version -is [int]) 'profile index schema must be an integer'
Assert-State ($index.active_profile_id -is [string] -and $index.active_profile_id -ceq $expectedProfileId) 'unexpected active profile'
Assert-State ($index.profiles -is [array]) 'ordered profiles must be an array'
Assert-State ($index.profiles.Count -eq 1) 'unexpected ordered profile count'
$profile = $index.profiles[0]
Assert-State ((@($profile.PSObject.Properties.Name | Sort-Object) -join ',') -ceq 'created_at_unix,id') 'unexpected profile entry fields'
Assert-State ($profile.id -is [string] -and $profile.id -ceq $expectedProfileId) 'unexpected ordered profile ID'
Assert-State ($profile.id -cmatch '^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$') 'profile ID must be UUID v4 shaped'
Assert-State ($profile.created_at_unix -is [int] -and $profile.created_at_unix -eq 1757959100) 'unexpected profile creation timestamp'
$profileRoot = Join-Path $root (Join-Path 'profiles' $expectedProfileId)
$generation = Join-Path $profileRoot 'generations\snapshot-00000000000000000001'

$expectedMihomo = "proxies:`n  - name: node-smoke`n    type: socks5`n    server: 127.0.0.1`n    port: 1080`nproxy-groups:`n  - name: proxy-group`n    type: select`n    proxies:`n      - node-smoke`nrules:`n  - MATCH,proxy-group`n"
Assert-State (([IO.File]::ReadAllText((Join-Path $generation 'mihomo.yaml'))) -ceq $expectedMihomo) 'mihomo generation changed'
$xray = Get-Content -Raw -LiteralPath (Join-Path $generation 'xray.json') | ConvertFrom-Json
Assert-State ((@($xray.PSObject.Properties.Name | Sort-Object) -join ',') -ceq 'log,outbounds') 'unexpected Xray fields'
Assert-State ($xray.log.loglevel -is [string] -and $xray.log.loglevel -ceq 'warning') 'unexpected Xray log level'
Assert-State (@($xray.outbounds).Count -eq 0) 'unexpected Xray outbounds'

$subscription = Get-Content -Raw -LiteralPath (Join-Path $generation 'subscription.json') | ConvertFrom-Json
Assert-State ((@($subscription.PSObject.Properties.Name | Sort-Object) -join ',') -ceq 'info,source_url,targets') 'unexpected subscription fields'
Assert-State ($subscription.source_url -is [string] -and $subscription.source_url -ceq 'https://sentinel.invalid/private-token') 'subscription sentinel changed'
Assert-State ((@($subscription.info.PSObject.Properties.Name | Sort-Object) -join ',') -ceq 'announcement_action_label,announcement_text,announcement_tone,display_name,downloaded_bytes,expires_at_unix,refresh_interval_secs,source_host,total_bytes,updated_at_unix,uploaded_bytes') 'unexpected subscription info fields'
Assert-State ($subscription.info.source_host -is [string] -and $subscription.info.source_host -ceq 'sentinel.invalid') 'subscription host changed'
Assert-State ($subscription.info.display_name -is [string] -and $subscription.info.display_name -ceq 'Smoke Subscription') 'subscription display name changed'
Assert-State ($null -eq $subscription.info.uploaded_bytes) 'subscription uploaded bytes changed'
Assert-State ($subscription.info.downloaded_bytes -is [int] -and $subscription.info.downloaded_bytes -eq 1024) 'subscription downloaded bytes changed'
Assert-State ($subscription.info.total_bytes -is [int] -and $subscription.info.total_bytes -eq 4096) 'subscription total bytes changed'
Assert-State ($null -eq $subscription.info.expires_at_unix) 'subscription expiry changed'
Assert-State ($subscription.info.updated_at_unix -is [int] -and $subscription.info.updated_at_unix -eq 1757959200) 'subscription timestamp changed'
Assert-State ($subscription.info.refresh_interval_secs -is [int] -and $subscription.info.refresh_interval_secs -eq 3600) 'subscription refresh interval changed'
Assert-State ($null -eq $subscription.info.announcement_text) 'subscription announcement text changed'
Assert-State ($null -eq $subscription.info.announcement_action_label) 'subscription announcement action changed'
Assert-State ($null -eq $subscription.info.announcement_tone) 'subscription announcement tone changed'
Assert-State ((@($subscription.targets.PSObject.Properties.Name | Sort-Object) -join ',') -ceq 'announcement_url,home_url,logo_url,support_url') 'unexpected subscription target fields'
Assert-State ($null -eq $subscription.targets.home_url) 'subscription home URL changed'
Assert-State ($null -eq $subscription.targets.support_url) 'subscription support URL changed'
Assert-State ($null -eq $subscription.targets.announcement_url) 'subscription announcement URL changed'
Assert-State ($null -eq $subscription.targets.logo_url) 'subscription logo URL changed'

$selections = Get-Content -Raw -LiteralPath (Join-Path $profileRoot 'selections.json') | ConvertFrom-Json
Assert-State ((@($selections.PSObject.Properties.Name | Sort-Object) -join ',') -ceq 'catalog_revision,schema_version,selections') 'unexpected selections fields'
Assert-State ($selections.schema_version -eq 1) 'unexpected selections schema'
Assert-State ($selections.schema_version -is [int]) 'selections schema must be an integer'
Assert-State ($selections.catalog_revision -eq 73 -and $selections.catalog_revision -is [int]) 'unexpected catalog revision'
Assert-State (@($selections.selections.PSObject.Properties).Count -eq 1) 'unexpected persisted selection count'
Assert-State ($selections.selections.'proxy-group' -is [string] -and $selections.selections.'proxy-group' -ceq 'node-smoke') 'selected node changed'
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
