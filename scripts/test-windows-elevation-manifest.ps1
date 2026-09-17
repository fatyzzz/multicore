param(
    [Parameter(Mandatory = $true)]
    [string]$DesktopExecutablePath,

    [Parameter(Mandatory = $true)]
    [string]$DaemonExecutablePath,

    [Parameter(Mandatory = $true)]
    [string]$UpdaterExecutablePath,

    [Parameter(Mandatory = $true)]
    [string]$CoreHostExecutablePath,

    [switch]$PeHeadersOnly,

    [Parameter(DontShow = $true)]
    [string]$ManifestXmlFixturePath,

    [Parameter(DontShow = $true)]
    [ValidateSet('asInvoker', 'requireAdministrator')]
    [string]$ExpectedFixtureLevel = 'asInvoker'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Assert-Amd64PeExecutable {
    param([string]$ExecutablePath, [string]$Description)

    $fullPath = [IO.Path]::GetFullPath($ExecutablePath)
    if (-not (Test-Path -LiteralPath $fullPath -PathType Leaf)) {
        throw "$Description was not found"
    }
    $file = Get-Item -LiteralPath $fullPath -Force
    if ($file.Length -lt 256 -or $file.Length -gt 256MB) {
        throw "$Description exceeds the bounded PE size or is truncated"
    }
    $bytes = [IO.File]::ReadAllBytes($fullPath)
    if ($bytes[0] -ne 0x4d -or $bytes[1] -ne 0x5a) {
        throw "$Description is not a coherent PE executable"
    }
    [uint64]$pe = [BitConverter]::ToUInt32($bytes, 0x3c)
    if ($pe -lt 0x40 -or $pe -gt ([uint64]$bytes.Length - 24)) {
        throw "$Description has an invalid PE header"
    }
    $peOffset = [int]$pe
    if ($bytes[$peOffset] -ne 0x50 -or $bytes[$peOffset + 1] -ne 0x45 -or
        $bytes[$peOffset + 2] -ne 0 -or $bytes[$peOffset + 3] -ne 0) {
        throw "$Description has an invalid PE header"
    }

    $coff = $peOffset + 4
    $machine = [BitConverter]::ToUInt16($bytes, $coff)
    $optionalSize = [BitConverter]::ToUInt16($bytes, $coff + 16)
    $characteristics = [BitConverter]::ToUInt16($bytes, $coff + 18)
    $optional = $coff + 20
    if ($machine -ne 0x8664) { throw "$Description is not AMD64" }
    if ($optionalSize -lt 2 -or ([uint64]$optional + $optionalSize) -gt $bytes.Length) {
        throw "$Description has a truncated optional header"
    }
    if (($characteristics -band 0x0002) -eq 0 -or ($characteristics -band 0x2000) -ne 0) {
        throw "$Description is not a non-DLL executable image"
    }
    if ([BitConverter]::ToUInt16($bytes, $optional) -ne 0x020b) {
        throw "$Description is not PE32+"
    }
}

function Assert-SemanticExecutionManifest {
    param(
        [byte[]]$Bytes,
        [string]$ExpectedLevel,
        [string]$Description
    )

    if ($Bytes.Length -eq 0 -or $Bytes.Length -gt 1MB) {
        throw "$Description manifest size is invalid"
    }
    try {
        $strictUtf8 = [Text.UTF8Encoding]::new($false, $true)
        $xmlText = $strictUtf8.GetString($Bytes)
    } catch {
        throw "$Description manifest is not strict UTF-8"
    }
    if ($xmlText -match '^\s*<\?xml[^>]*\bencoding\s*=\s*["'']([^"'']+)["'']' -and
        $Matches[1] -notin @('UTF-8', 'utf-8', 'UTF8', 'utf8')) {
        throw "$Description manifest declares a non-UTF-8 encoding"
    }

    try {
        $settings = [Xml.XmlReaderSettings]::new()
        $settings.DtdProcessing = [Xml.DtdProcessing]::Prohibit
        $settings.XmlResolver = $null
        $settings.MaxCharactersInDocument = 1MB
        $reader = [Xml.XmlReader]::Create([IO.StringReader]::new($xmlText), $settings)
        try {
            $document = [Xml.XmlDocument]::new()
            $document.XmlResolver = $null
            $document.Load($reader)
        } finally {
            $reader.Dispose()
        }
    } catch {
        throw "$Description manifest is not valid XML"
    }

    $namespaces = [Xml.XmlNamespaceManager]::new($document.NameTable)
    $namespaces.AddNamespace('asmv1', 'urn:schemas-microsoft-com:asm.v1')
    $namespaces.AddNamespace('asmv3', 'urn:schemas-microsoft-com:asm.v3')
    $levels = $document.SelectNodes('/asmv1:assembly/asmv3:trustInfo/asmv3:security/asmv3:requestedPrivileges/asmv3:requestedExecutionLevel', $namespaces)
    if ($levels.Count -ne 1) {
        throw "$Description manifest must contain exactly one namespaced requestedExecutionLevel"
    }
    $level = $levels[0].GetAttribute('level')
    if ($level -cne $ExpectedLevel) {
        throw "$Description manifest execution level '$level' does not match expected '$ExpectedLevel'"
    }
    if ($levels[0].GetAttribute('uiAccess') -cne 'false') {
        throw "$Description manifest must set uiAccess=false"
    }
}

# Validate every image before LoadLibraryExW performs any resource inspection.
Assert-Amd64PeExecutable $DesktopExecutablePath 'Desktop executable'
Assert-Amd64PeExecutable $DaemonExecutablePath 'Daemon executable'
Assert-Amd64PeExecutable $UpdaterExecutablePath 'Updater executable'
Assert-Amd64PeExecutable $CoreHostExecutablePath 'Core-host executable'

if ($PeHeadersOnly) {
    Write-Host 'PASS: all inputs are AMD64 PE32+ non-DLL executables'
    return
}

if ($ManifestXmlFixturePath) {
    $fixtureFullPath = [IO.Path]::GetFullPath($ManifestXmlFixturePath)
    if (-not (Test-Path -LiteralPath $fixtureFullPath -PathType Leaf)) {
        throw 'Manifest XML fixture was not found'
    }
    $fixture = Get-Item -LiteralPath $fixtureFullPath -Force
    if ($fixture.Length -eq 0 -or $fixture.Length -gt 1MB) {
        throw 'Manifest XML fixture size is invalid'
    }
    Assert-SemanticExecutionManifest ([IO.File]::ReadAllBytes($fixtureFullPath)) $ExpectedFixtureLevel 'Fixture'
    Write-Host 'PASS: semantic execution manifest fixture'
    return
}

Add-Type @'
using System;
using System.Runtime.InteropServices;

public static class MultiCoreManifestResource
{
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern IntPtr LoadLibraryExW(string file, IntPtr reserved, uint flags);

    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern IntPtr FindResourceW(IntPtr module, IntPtr name, IntPtr type);

    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern uint SizeofResource(IntPtr module, IntPtr resource);

    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern IntPtr LoadResource(IntPtr module, IntPtr resource);

    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern IntPtr LockResource(IntPtr resourceData);

    [DllImport("kernel32.dll")]
    public static extern bool FreeLibrary(IntPtr module);
}
'@

function Assert-NumericManifest {
    param([string]$ExecutablePath, [string]$RequiredLevel)

    $module = [MultiCoreManifestResource]::LoadLibraryExW(
        [IO.Path]::GetFullPath($ExecutablePath),
        [IntPtr]::Zero,
        0x00000002
    )
    if ($module -eq [IntPtr]::Zero) {
        throw "Could not load executable resources: $([Runtime.InteropServices.Marshal]::GetLastWin32Error())"
    }

    try {
        $resource = [MultiCoreManifestResource]::FindResourceW($module, [IntPtr]1, [IntPtr]24)
        if ($resource -eq [IntPtr]::Zero) { throw 'Numeric RT_MANIFEST resource #1 is missing' }
        $size = [MultiCoreManifestResource]::SizeofResource($module, $resource)
        if ($size -eq 0 -or $size -gt 1MB) { throw "Embedded manifest size is invalid: $size" }
        $loaded = [MultiCoreManifestResource]::LoadResource($module, $resource)
        $pointer = [MultiCoreManifestResource]::LockResource($loaded)
        if ($loaded -eq [IntPtr]::Zero -or $pointer -eq [IntPtr]::Zero) {
            throw 'Embedded manifest could not be loaded'
        }

        $bytes = New-Object byte[] $size
        [Runtime.InteropServices.Marshal]::Copy($pointer, $bytes, 0, $size)
        Assert-SemanticExecutionManifest $bytes $RequiredLevel 'Embedded'
    } finally {
        [void][MultiCoreManifestResource]::FreeLibrary($module)
    }
}

$asInvoker = 'asInvoker'
$requireAdministrator = 'requireAdministrator'
Assert-NumericManifest $DesktopExecutablePath $asInvoker
Assert-NumericManifest $DaemonExecutablePath $asInvoker
Assert-NumericManifest $UpdaterExecutablePath $asInvoker
Assert-NumericManifest $CoreHostExecutablePath $requireAdministrator

Write-Host 'PASS: four AMD64 executables embed semantic numeric RT_MANIFEST #1 with least-privilege levels'
