[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Checker = Join-Path $PSScriptRoot 'test-windows-elevation-manifest.ps1'

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw "ASSERTION FAILED: $Message" }
}

function Set-UInt16 {
    param([byte[]]$Bytes, [int]$Offset, [uint16]$Value)
    [BitConverter]::GetBytes($Value).CopyTo($Bytes, $Offset)
}

function Set-UInt32 {
    param([byte[]]$Bytes, [int]$Offset, [uint32]$Value)
    [BitConverter]::GetBytes($Value).CopyTo($Bytes, $Offset)
}

function New-PeHeaderFixture {
    param(
        [string]$Path,
        [uint16]$Machine = 0x8664,
        [uint16]$OptionalMagic = 0x020b,
        [switch]$Dll,
        [switch]$Malformed
    )
    $bytes = New-Object byte[] 512
    if ($Malformed) {
        $bytes[0] = 0x4d; $bytes[1] = 0x5a
        Set-UInt32 $bytes 0x3c ([uint32]::MaxValue - 15)
    } else {
        $bytes[0] = 0x4d; $bytes[1] = 0x5a
        Set-UInt32 $bytes 0x3c 0x80
        $bytes[0x80] = 0x50; $bytes[0x81] = 0x45
        Set-UInt16 $bytes 0x84 $Machine
        Set-UInt16 $bytes 0x86 1
        Set-UInt16 $bytes 0x94 0xf0
        $characteristics = [uint16]0x0002
        if ($Dll) { $characteristics = [uint16]($characteristics -bor 0x2000) }
        Set-UInt16 $bytes 0x96 $characteristics
        Set-UInt16 $bytes 0x98 $OptionalMagic
    }
    [IO.File]::WriteAllBytes($Path, $bytes)
}

$root = Join-Path ([IO.Path]::GetTempPath()) ("multicore-manifest-pe-{0}" -f [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $root | Out-Null
try {
    $valid = Join-Path $root 'valid.exe'; New-PeHeaderFixture $valid
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Checker -DesktopExecutablePath $valid -DaemonExecutablePath $valid -CoreHostExecutablePath $valid -PeHeadersOnly | Out-Null
    Assert-True ($LASTEXITCODE -eq 0) 'synthetic AMD64 PE32+ executable header must pass header-only validation'

    foreach ($case in @(
        @{ Name = 'x86'; Machine = [uint16]0x014c; Magic = [uint16]0x020b; Dll = $false; Malformed = $false; Expected = 'is not AMD64' },
        @{ Name = 'arm64'; Machine = [uint16]0xaa64; Magic = [uint16]0x020b; Dll = $false; Malformed = $false; Expected = 'is not AMD64' },
        @{ Name = 'pe32'; Machine = [uint16]0x8664; Magic = [uint16]0x010b; Dll = $false; Malformed = $false; Expected = 'is not PE32+' },
        @{ Name = 'dll'; Machine = [uint16]0x8664; Magic = [uint16]0x020b; Dll = $true; Malformed = $false; Expected = 'is not a non-DLL executable image' },
        @{ Name = 'malformed'; Machine = [uint16]0x8664; Magic = [uint16]0x020b; Dll = $false; Malformed = $true; Expected = 'invalid PE header' }
    )) {
        $fixture = Join-Path $root ($case.Name + '.exe')
        New-PeHeaderFixture $fixture -Machine $case.Machine -OptionalMagic $case.Magic -Dll:$case.Dll -Malformed:$case.Malformed
        $savedErrorPreference = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        $output = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Checker -DesktopExecutablePath $fixture -DaemonExecutablePath $fixture -CoreHostExecutablePath $fixture -PeHeadersOnly 2>&1 | Out-String
        $fixtureExitCode = $LASTEXITCODE
        $ErrorActionPreference = $savedErrorPreference
        Assert-True ($fixtureExitCode -ne 0) "$($case.Name) fixture must fail"
        Assert-True ($output.Contains($case.Expected)) "$($case.Name) failure must identify $($case.Expected)"
    }
} finally {
    if (Test-Path -LiteralPath $root) { Remove-Item -LiteralPath $root -Recurse -Force }
}

Write-Output 'PASS: elevation manifest PE header contract'
