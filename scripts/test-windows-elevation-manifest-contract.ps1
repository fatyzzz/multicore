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
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Checker -DesktopExecutablePath $valid -DaemonExecutablePath $valid -UpdaterExecutablePath $valid -CoreHostExecutablePath $valid -PeHeadersOnly | Out-Null
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
        $output = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Checker -DesktopExecutablePath $fixture -DaemonExecutablePath $fixture -UpdaterExecutablePath $fixture -CoreHostExecutablePath $fixture -PeHeadersOnly 2>&1 | Out-String
        $fixtureExitCode = $LASTEXITCODE
        $ErrorActionPreference = $savedErrorPreference
        Assert-True ($fixtureExitCode -ne 0) "$($case.Name) fixture must fail"
        Assert-True ($output.Contains($case.Expected)) "$($case.Name) failure must identify $($case.Expected)"
    }

    $validXml = Join-Path $root 'valid-manifest.xml'
    [IO.File]::WriteAllText($validXml, '<?xml version="1.0" encoding="UTF-8"?><assembly xmlns="urn:schemas-microsoft-com:asm.v1"><v3:trustInfo xmlns:v3="urn:schemas-microsoft-com:asm.v3"><v3:security><v3:requestedPrivileges><v3:requestedExecutionLevel uiAccess = "false" level = "asInvoker"></v3:requestedExecutionLevel></v3:requestedPrivileges></v3:security></v3:trustInfo></assembly>', [Text.UTF8Encoding]::new($false, $true))
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Checker -DesktopExecutablePath $valid -DaemonExecutablePath $valid -UpdaterExecutablePath $valid -CoreHostExecutablePath $valid -ManifestXmlFixturePath $validXml -ExpectedFixtureLevel asInvoker | Out-Null
    Assert-True ($LASTEXITCODE -eq 0) 'alternate whitespace and attribute order must pass semantic manifest validation'

    foreach ($xmlCase in @(
        @{ Name = 'comment-spoof'; Xml = '<?xml version="1.0" encoding="UTF-8"?><assembly xmlns="urn:schemas-microsoft-com:asm.v1"><!-- <requestedExecutionLevel level="asInvoker" uiAccess="false" /> --><v3:trustInfo xmlns:v3="urn:schemas-microsoft-com:asm.v3"><v3:security><v3:requestedPrivileges><v3:requestedExecutionLevel level="highestAvailable" uiAccess="false" /></v3:requestedPrivileges></v3:security></v3:trustInfo></assembly>'; Expected = "expected 'asInvoker'" },
        @{ Name = 'duplicate'; Xml = '<?xml version="1.0" encoding="UTF-8"?><assembly xmlns="urn:schemas-microsoft-com:asm.v1"><v3:trustInfo xmlns:v3="urn:schemas-microsoft-com:asm.v3"><v3:security><v3:requestedPrivileges><v3:requestedExecutionLevel level="asInvoker" uiAccess="false" /><v3:requestedExecutionLevel level="asInvoker" uiAccess="false" /></v3:requestedPrivileges></v3:security></v3:trustInfo></assembly>'; Expected = 'exactly one' },
        @{ Name = 'malformed-xml'; Xml = '<?xml version="1.0" encoding="UTF-8"?><assembly><broken></assembly>'; Expected = 'valid XML' },
        @{ Name = 'dtd'; Xml = '<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE assembly [<!ENTITY spoof "asInvoker">]><assembly xmlns="urn:schemas-microsoft-com:asm.v1"><v3:trustInfo xmlns:v3="urn:schemas-microsoft-com:asm.v3"><v3:security><v3:requestedPrivileges><v3:requestedExecutionLevel level="&spoof;" uiAccess="false" /></v3:requestedPrivileges></v3:security></v3:trustInfo></assembly>'; Expected = 'valid XML' },
        @{ Name = 'highest-available'; Xml = '<?xml version="1.0" encoding="UTF-8"?><assembly xmlns="urn:schemas-microsoft-com:asm.v1"><v3:trustInfo xmlns:v3="urn:schemas-microsoft-com:asm.v3"><v3:security><v3:requestedPrivileges><v3:requestedExecutionLevel level="highestAvailable" uiAccess="false" /></v3:requestedPrivileges></v3:security></v3:trustInfo></assembly>'; Expected = "expected 'asInvoker'" },
        @{ Name = 'missing'; Xml = '<?xml version="1.0" encoding="UTF-8"?><assembly xmlns="urn:schemas-microsoft-com:asm.v1" />'; Expected = 'exactly one' },
        @{ Name = 'ui-access'; Xml = '<?xml version="1.0" encoding="UTF-8"?><assembly xmlns="urn:schemas-microsoft-com:asm.v1"><v3:trustInfo xmlns:v3="urn:schemas-microsoft-com:asm.v3"><v3:security><v3:requestedPrivileges><v3:requestedExecutionLevel level="asInvoker" uiAccess="true" /></v3:requestedPrivileges></v3:security></v3:trustInfo></assembly>'; Expected = 'uiAccess=false' }
    )) {
        $xmlPath = Join-Path $root ($xmlCase.Name + '.xml')
        [IO.File]::WriteAllText($xmlPath, $xmlCase.Xml, [Text.UTF8Encoding]::new($false, $true))
        $savedErrorPreference = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        $output = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Checker -DesktopExecutablePath $valid -DaemonExecutablePath $valid -UpdaterExecutablePath $valid -CoreHostExecutablePath $valid -ManifestXmlFixturePath $xmlPath -ExpectedFixtureLevel asInvoker 2>&1 | Out-String
        $fixtureExitCode = $LASTEXITCODE
        $ErrorActionPreference = $savedErrorPreference
        Assert-True ($fixtureExitCode -ne 0) "$($xmlCase.Name) XML fixture must fail"
        Assert-True ($output.Contains($xmlCase.Expected)) "$($xmlCase.Name) failure must identify $($xmlCase.Expected)"
    }

    $invalidUtf8 = Join-Path $root 'invalid-utf8.xml'
    [IO.File]::WriteAllBytes($invalidUtf8, [byte[]]@(0x3c, 0x61, 0xc3, 0x28, 0x2f, 0x3e))
    $savedErrorPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $output = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Checker -DesktopExecutablePath $valid -DaemonExecutablePath $valid -UpdaterExecutablePath $valid -CoreHostExecutablePath $valid -ManifestXmlFixturePath $invalidUtf8 -ExpectedFixtureLevel asInvoker 2>&1 | Out-String
    $fixtureExitCode = $LASTEXITCODE
    $ErrorActionPreference = $savedErrorPreference
    Assert-True ($fixtureExitCode -ne 0) 'invalid UTF-8 XML fixture must fail'
    Assert-True ($output.Contains('strict UTF-8')) 'invalid UTF-8 failure must identify strict decoding'
} finally {
    if (Test-Path -LiteralPath $root) { Remove-Item -LiteralPath $root -Recurse -Force }
}

Write-Output 'PASS: elevation manifest PE header contract'
