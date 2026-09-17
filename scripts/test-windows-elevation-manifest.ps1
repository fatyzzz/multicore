param(
    [Parameter(Mandatory = $true)]
    [string]$DesktopExecutablePath,

    [Parameter(Mandatory = $true)]
    [string]$DaemonExecutablePath,

    [Parameter(Mandatory = $true)]
    [string]$CoreHostExecutablePath,

    [switch]$PeHeadersOnly
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Assert-Amd64PeExecutable {
    param([string]$ExecutablePath, [string]$Description)

    $fullPath = [IO.Path]::GetFullPath($ExecutablePath)
    if (-not (Test-Path -LiteralPath $fullPath -PathType Leaf)) {
        throw "$Description was not found"
    }
    $bytes = [IO.File]::ReadAllBytes($fullPath)
    if ($bytes.Length -lt 256 -or $bytes[0] -ne 0x4d -or $bytes[1] -ne 0x5a) {
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

# Validate every image before LoadLibraryExW performs any resource inspection.
Assert-Amd64PeExecutable $DesktopExecutablePath 'Desktop executable'
Assert-Amd64PeExecutable $DaemonExecutablePath 'Daemon executable'
Assert-Amd64PeExecutable $CoreHostExecutablePath 'Core-host executable'

if ($PeHeadersOnly) {
    Write-Host 'PASS: all inputs are AMD64 PE32+ non-DLL executables'
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
    param([string]$ExecutablePath, [string]$RequiredLevel, [string]$ForbiddenLevel)

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
        $manifest = [Text.Encoding]::UTF8.GetString($bytes)
        if (-not $manifest.Contains($RequiredLevel)) {
            throw 'Embedded manifest does not contain required execution level'
        }
        if ($manifest.Contains($ForbiddenLevel)) {
            throw 'Embedded manifest contains forbidden execution level'
        }
    } finally {
        [void][MultiCoreManifestResource]::FreeLibrary($module)
    }
}

$asInvoker = '<requestedExecutionLevel level="asInvoker" uiAccess="false" />'
$requireAdministrator = '<requestedExecutionLevel level="requireAdministrator" uiAccess="false" />'
Assert-NumericManifest $DesktopExecutablePath $asInvoker $requireAdministrator
Assert-NumericManifest $DaemonExecutablePath $asInvoker $requireAdministrator
Assert-NumericManifest $CoreHostExecutablePath $requireAdministrator $asInvoker

Write-Host 'PASS: AMD64 desktop, daemon, and core-host embed numeric RT_MANIFEST #1 with least-privilege levels'
