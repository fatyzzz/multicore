param(
    [Parameter(Mandatory = $true)]
    [string]$DesktopExecutablePath,

    [Parameter(Mandatory = $true)]
    [string]$CoreHostExecutablePath
)

$ErrorActionPreference = 'Stop'

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

function Assert-NumericManifest(
    [string]$ExecutablePath,
    [string]$RequiredLevel,
    [string]$ForbiddenLevel
) {
    $ExecutablePath = [IO.Path]::GetFullPath($ExecutablePath)
    if (-not (Test-Path -LiteralPath $ExecutablePath -PathType Leaf)) {
        throw "Executable not found: $ExecutablePath"
    }

    $module = [MultiCoreManifestResource]::LoadLibraryExW(
        $ExecutablePath,
        [IntPtr]::Zero,
        0x00000002
    )
    if ($module -eq [IntPtr]::Zero) {
        throw "Could not load executable resources: $([Runtime.InteropServices.Marshal]::GetLastWin32Error())"
    }

    try {
        # Keep both identifiers numeric: name #1 and RT_MANIFEST type 24.
        $resource = [MultiCoreManifestResource]::FindResourceW(
            $module,
            [IntPtr]1,
            [IntPtr]24
        )
        if ($resource -eq [IntPtr]::Zero) {
            throw "Numeric RT_MANIFEST resource #1 is missing"
        }

        $size = [MultiCoreManifestResource]::SizeofResource($module, $resource)
        if ($size -eq 0 -or $size -gt 1MB) {
            throw "Embedded manifest size is invalid: $size"
        }
        $loaded = [MultiCoreManifestResource]::LoadResource($module, $resource)
        $pointer = [MultiCoreManifestResource]::LockResource($loaded)
        if ($loaded -eq [IntPtr]::Zero -or $pointer -eq [IntPtr]::Zero) {
            throw "Embedded manifest could not be loaded"
        }

        $bytes = New-Object byte[] $size
        [Runtime.InteropServices.Marshal]::Copy($pointer, $bytes, 0, $size)
        $manifest = [Text.Encoding]::UTF8.GetString($bytes)
        if (-not $manifest.Contains($RequiredLevel)) {
            throw "Embedded manifest does not contain required execution level"
        }
        if ($manifest.Contains($ForbiddenLevel)) {
            throw "Embedded manifest contains forbidden execution level"
        }
    } finally {
        [void][MultiCoreManifestResource]::FreeLibrary($module)
    }
}

$asInvoker = '<requestedExecutionLevel level="asInvoker" uiAccess="false" />'
$requireAdministrator = '<requestedExecutionLevel level="requireAdministrator" uiAccess="false" />'
Assert-NumericManifest $DesktopExecutablePath $asInvoker $requireAdministrator
Assert-NumericManifest $CoreHostExecutablePath $requireAdministrator $asInvoker

Write-Host "PASS: desktop and core-host embed numeric RT_MANIFEST #1 with least-privilege levels"
