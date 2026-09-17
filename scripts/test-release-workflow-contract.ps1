[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$root = Split-Path -Parent $PSScriptRoot
$workflowPath = Join-Path $root '.github\workflows\release.yml'

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw "FAIL: $Message" }
}

Assert-True (Test-Path -LiteralPath $workflowPath -PathType Leaf) 'release workflow must exist'
$workflow = Get-Content -Raw -LiteralPath $workflowPath

foreach ($token in @(
    'tags:',
    "- 'v*'",
    'permissions:',
    'contents: write',
    'runs-on: windows-latest',
    'cargo test --workspace --all-targets --locked',
    'cargo clippy --workspace --all-targets --locked -- -D warnings',
    '-AssertReleaseReady',
    '-UpdateRepository fatyzzz/multicore',
    'multicore-windows-x64.zip',
    'MultiCore-Setup-x64.exe',
    'test-windows-installer-contract.ps1',
    '-DesktopExecutablePath dist/release/multicore-windows-x64/MultiCore.exe',
    '-DaemonExecutablePath dist/release/multicore-windows-x64/runtime/multicore-daemon.exe',
    '-UpdaterExecutablePath dist/release/multicore-windows-x64/runtime/multicore-updater.exe',
    '-CoreHostExecutablePath dist/release/multicore-windows-x64/runtime/multicore-core-host.exe',
    'build-windows-installer.ps1',
    'smoke-test-windows-installed-update.ps1',
    'Xray-core-source.zip',
    'mihomo-source.zip',
    'gh release create',
    '--draft',
    'gh release edit',
    '--draft=false'
)) {
    Assert-True ($workflow.Contains($token)) "release workflow is missing: $token"
}

Assert-True (-not $workflow.Contains('pull_request_target')) 'release workflow must not run in pull_request_target'
Assert-True (-not $workflow.Contains('MULTICORE_DAEMON_TOKEN')) 'release workflow must not embed runtime secrets'

Write-Output 'PASS: GitHub release workflow contract'
