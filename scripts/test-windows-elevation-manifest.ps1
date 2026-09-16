param(
    [Parameter(Mandatory = $true)]
    [string]$ExecutablePath
)

$ErrorActionPreference = 'Stop'
$ExecutablePath = [IO.Path]::GetFullPath($ExecutablePath)
if (-not (Test-Path -LiteralPath $ExecutablePath -PathType Leaf)) {
    throw "Executable not found: $ExecutablePath"
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

$LoadLibraryAsDataFile = 0x00000002
$ResourceId = [IntPtr]1
$ManifestResourceType = [IntPtr]24
$module = [MultiCoreManifestResource]::LoadLibraryExW(
    $ExecutablePath,
    [IntPtr]::Zero,
    $LoadLibraryAsDataFile
)
if ($module -eq [IntPtr]::Zero) {
    throw "Could not load executable resources: $([Runtime.InteropServices.Marshal]::GetLastWin32Error())"
}

try {
    $resource = [MultiCoreManifestResource]::FindResourceW(
        $module,
        $ResourceId,
        $ManifestResourceType
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
    $required = '<requestedExecutionLevel level="requireAdministrator" uiAccess="false" />'
    if (-not $manifest.Contains($required)) {
        throw "Embedded manifest does not request administrator elevation"
    }
    if ($manifest.Contains('level="asInvoker"')) {
        throw "Embedded release manifest still contains asInvoker"
    }
} finally {
    [void][MultiCoreManifestResource]::FreeLibrary($module)
}

Write-Host "PASS: release PE embeds numeric RT_MANIFEST with requireAdministrator"
