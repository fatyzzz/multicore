[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$RepositoryRoot = Split-Path -Parent $PSScriptRoot
$DefinitionPath = Join-Path $RepositoryRoot 'packaging\windows-x64\installer.iss'
$BuilderPath = Join-Path $PSScriptRoot 'build-windows-installer.ps1'

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw "FAIL: $Message" }
}

Assert-True (Test-Path -LiteralPath $DefinitionPath -PathType Leaf) 'installer definition must exist'
Assert-True (Test-Path -LiteralPath $BuilderPath -PathType Leaf) 'installer builder must exist'

$definition = Get-Content -Raw -LiteralPath $DefinitionPath
$builder = Get-Content -Raw -LiteralPath $BuilderPath

Assert-True (-not $definition.Contains('Verb: "runas"')) 'installer must not elevate desktop postinstall launch'

foreach ($token in @(
    'DefaultDirName={localappdata}\Programs\MultiCore',
    'PrivilegesRequired=lowest',
    'ArchitecturesAllowed=x64compatible',
    'OutputBaseFilename=MultiCore-Setup-x64',
    'CloseApplications=yes',
    'UninstallDisplayIcon={app}\current\MultiCore.exe',
    'DestDir: "{app}\current"',
    'Name: "desktopicon"',
    'Name: "autostart"',
    'Software\Microsoft\Windows\CurrentVersion\Run',
    '""{app}\current\MultiCore.exe"" --background',
    'Software\Classes\multicore',
    'ValueName: "URL Protocol"',
    'ValueData: "{app}\current\MultiCore.exe,0"',
    '""{app}\current\MultiCore.exe"" ""%1""',
    'Filename: "{app}\current\MultiCore.exe"',
    'WorkingDir: "{app}\current"',
    'Flags: postinstall shellexec skipifsilent',
    'Type: filesandordirs; Name: "{app}\.multicore-previous-*"',
    'Type: filesandordirs; Name: "{app}\.multicore-failed-*"',
    'Type: filesandordirs; Name: "{app}\.multicore-stage-*"',
    'RegQueryStringValue(',
    "CompareText(AutostartValue, ExpectedAutostartValue) = 0",
    "RegDeleteValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Run', 'MultiCore')"
)) {
    Assert-True ($definition.Contains($token)) "installer definition is missing: $token"
}

foreach ($forbiddenToken in @(
    'DestDir: "{app}"',
    'UninstallDisplayIcon={app}\MultiCore.exe',
    'Filename: "{app}\MultiCore.exe"',
    'ValueData: "{app}\MultiCore.exe,0"',
    '""{app}\MultiCore.exe"" --background',
    '""{app}\MultiCore.exe"" ""%1""',
    'uninsdeletevalue',
    'Flags: nowait postinstall skipifsilent'
)) {
    Assert-True (-not $definition.Contains($forbiddenToken)) "installer definition still uses the root payload path: $forbiddenToken"
}

foreach ($token in @(
    '[switch]$ValidateOnly',
    'SHA256SUMS.txt',
    'MultiCore-Setup-x64.exe',
    'ISCC.exe',
    '/DPayloadDir=',
    '/DOutputDir=',
    '/DAppVersion='
)) {
    Assert-True ($builder.Contains($token)) "installer builder is missing: $token"
}

$testRoot = Join-Path ([IO.Path]::GetTempPath()) ('multicore-installer-contract-' + [Guid]::NewGuid().ToString('N'))
$payload = Join-Path $testRoot 'payload'
try {
    foreach ($directory in @($payload, (Join-Path $payload 'cores'), (Join-Path $payload 'licenses'), (Join-Path $payload 'runtime'))) {
        New-Item -ItemType Directory -Path $directory | Out-Null
    }
    $files = @(
        'MultiCore.exe',
        'README.md',
        'THIRD_PARTY_NOTICES.md',
        'cores\mihomo.exe',
        'cores\xray.exe',
        'licenses\Xray-core-MPL-2.0.txt',
        'licenses\mihomo-GPL-3.0.txt',
        'runtime\multicore-daemon.exe',
        'runtime\multicore-updater.exe',
        'runtime\multicore-core-host.exe',
        'versions.json'
    )
    $lines = foreach ($relative in $files) {
        $path = Join-Path $payload $relative
        [IO.File]::WriteAllText($path, "fixture:$relative", [Text.UTF8Encoding]::new($false))
        $hash = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
        "$hash *$($relative.Replace('\', '/'))"
    }
    [IO.File]::WriteAllLines((Join-Path $payload 'SHA256SUMS.txt'), $lines, [Text.UTF8Encoding]::new($false))

    & $BuilderPath -PackagePath $payload -AppVersion '0.1.0' -ValidateOnly

    [IO.File]::WriteAllText((Join-Path $payload 'README.md'), 'tampered', [Text.UTF8Encoding]::new($false))
    $failed = $false
    try { & $BuilderPath -PackagePath $payload -AppVersion '0.1.0' -ValidateOnly } catch { $failed = $true }
    Assert-True $failed 'tampered portable payload must fail installer validation'

    Write-Output 'PASS: Windows installer contract'
} finally {
    $resolvedRoot = [IO.Path]::GetFullPath($testRoot)
    $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
    if ($resolvedRoot.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -and (Test-Path -LiteralPath $resolvedRoot)) {
        Remove-Item -LiteralPath $resolvedRoot -Recurse -Force
    }
}
