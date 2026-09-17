[CmdletBinding()]
param(
    [string]$DestinationPath,
    [string]$ManifestPath,
    [string]$OfflineSourceDirectory,
    [string]$DesktopExecutablePath,
    [string]$DaemonExecutablePath,
    [string]$UpdaterExecutablePath,
    [string]$CoreHostExecutablePath,
    [string]$UpdateRepository,
    [string]$ReleaseAssetPath,
    [switch]$AssertReleaseReady,
    [Parameter(DontShow = $true)]
    [switch]$FixtureContractMode
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# Task 4 may flip this single marker only after the broker and package inventory are complete.
$PrivilegedBrokerReleaseReady = $false
$ReleaseGateMessage = 'Production packaging is disabled until the least-privilege core broker and package inventory are complete (Tasks 2-4).'
$RepositoryRoot = Split-Path -Parent $PSScriptRoot
$MetadataRoot = Join-Path $RepositoryRoot 'packaging\windows-x64'
$DefaultManifestPath = Join-Path $MetadataRoot 'versions.json'
if (-not $ManifestPath) { $ManifestPath = $DefaultManifestPath }
$manifestCandidate = [IO.Path]::GetFullPath($ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($ManifestPath))
$isFixtureInvocation = $false
if ($FixtureContractMode -and $OfflineSourceDirectory -and [IO.File]::Exists($manifestCandidate)) {
    try {
        $temporaryRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') + '\'
        $offlineCandidate = [IO.Path]::GetFullPath($ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OfflineSourceDirectory))
        $manifestItem = Get-Item -LiteralPath $manifestCandidate -Force
        $manifestIsSafeTemporaryFile = $manifestCandidate.StartsWith($temporaryRoot, [StringComparison]::OrdinalIgnoreCase) -and
            $offlineCandidate.StartsWith($temporaryRoot, [StringComparison]::OrdinalIgnoreCase) -and
            -not (($manifestItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) -and
            $manifestItem.Length -gt 0 -and $manifestItem.Length -le 1MB
        if ($manifestIsSafeTemporaryFile) {
            $fixtureManifest = Get-Content -Raw -LiteralPath $manifestCandidate | ConvertFrom-Json
            $isFixtureInvocation = $fixtureManifest.fixtureContract -ceq 'multicore-package-test-v1' -and
                $fixtureManifest.components.xray.version -ceq 'fixture' -and
                $fixtureManifest.components.mihomo.version -ceq 'fixture' -and
                ([string]$fixtureManifest.components.xray.asset.url).StartsWith('https://example.invalid/', [StringComparison]::Ordinal) -and
                ([string]$fixtureManifest.components.mihomo.asset.url).StartsWith('https://example.invalid/', [StringComparison]::Ordinal)
        }
    } catch {
        $isFixtureInvocation = $false
    }
}
if (-not $isFixtureInvocation) {
    if (-not $PrivilegedBrokerReleaseReady) { throw $ReleaseGateMessage }
    if ($AssertReleaseReady) {
        Write-Output 'PASS: privileged broker release gate is open'
        return
    }
}
if ($AssertReleaseReady) { throw 'AssertReleaseReady is valid only for production packaging readiness checks.' }
if ([String]::IsNullOrWhiteSpace($DestinationPath)) { throw 'DestinationPath is required.' }

function Get-FullPath { param([string]$Path) [IO.Path]::GetFullPath($ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Path)) }
function Get-Sha256 { param([string]$Path) (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() }

function Assert-NoReparseAncestor {
    param([string]$Path)
    $full = Get-FullPath $Path
    $root = [IO.Path]::GetPathRoot($full)
    $current = $root
    $relative = $full.Substring($root.Length)
    foreach ($part in ($relative -split '[\\/]')) {
        if (-not $part) { continue }
        $current = Join-Path $current $part
        if (Test-Path -LiteralPath $current) {
            $item = Get-Item -LiteralPath $current -Force
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw "Reparse-point path component is forbidden: $current" }
        }
    }
}

function Assert-File {
    param([string]$Path, [string]$Description)
    Assert-NoReparseAncestor $Path
    if (-not [IO.File]::Exists((Get-FullPath $Path))) { throw "$Description is missing or is not a regular file: $Path" }
}

function Assert-NoPrivateBuildPaths {
    param([string]$Path, [string]$Description)
    $bytes = [IO.File]::ReadAllBytes((Get-FullPath $Path))
    $utf8 = [Text.Encoding]::UTF8.GetString($bytes)
    $utf16 = [Text.Encoding]::Unicode.GetString($bytes)
    $privateRoots = @(
        $RepositoryRoot,
        [Environment]::GetFolderPath([Environment+SpecialFolder]::UserProfile),
        $env:CARGO_HOME,
        $env:RUSTUP_HOME
    ) | Where-Object { $_ } | Select-Object -Unique
    foreach ($root in $privateRoots) {
        if ($utf8.IndexOf($root, [StringComparison]::OrdinalIgnoreCase) -ge 0 -or
            $utf16.IndexOf($root, [StringComparison]::OrdinalIgnoreCase) -ge 0) {
            throw "$Description contains a private absolute build path"
        }
    }
}

function Invoke-PinnedRustBuild {
    param([object]$Build, [string]$TargetDirectory, [string]$UpdateRepository)
    $hadRustFlags = Test-Path Env:RUSTFLAGS
    $savedRustFlags = $env:RUSTFLAGS
    $hadEncodedFlags = Test-Path Env:CARGO_ENCODED_RUSTFLAGS
    $savedEncodedFlags = $env:CARGO_ENCODED_RUSTFLAGS
    $hadCFlags = Test-Path Env:CFLAGS
    $savedCFlags = $env:CFLAGS
    $hadCxxFlags = Test-Path Env:CXXFLAGS
    $savedCxxFlags = $env:CXXFLAGS
    $hadTargetDirectory = Test-Path Env:CARGO_TARGET_DIR
    $savedTargetDirectory = $env:CARGO_TARGET_DIR
    $hadUpdateRepository = Test-Path Env:MULTICORE_UPDATE_REPOSITORY
    $savedUpdateRepository = $env:MULTICORE_UPDATE_REPOSITORY
    Push-Location $RepositoryRoot
    try {
        $cargoVersion = (& cargo -V 2>&1 | Out-String).Trim()
        if ($LASTEXITCODE -ne 0 -or $cargoVersion -cnotmatch '^cargo 1\.98\.1 \(') {
            throw "Packaging requires Cargo 1.98.1; observed: $cargoVersion"
        }
        $rustcVersion = (& rustc -Vv 2>&1 | Out-String).Trim()
        if ($LASTEXITCODE -ne 0 -or $rustcVersion -cnotmatch '(?m)^release: 1\.98\.1\r?$') {
            throw "Packaging requires rustc 1.98.1"
        }

        Remove-Item Env:RUSTFLAGS -ErrorAction SilentlyContinue
        $userProfile = [Environment]::GetFolderPath([Environment+SpecialFolder]::UserProfile)
        if ([String]::IsNullOrWhiteSpace($userProfile)) { $userProfile = $env:USERPROFILE }
        if ([String]::IsNullOrWhiteSpace($userProfile) -or -not [IO.Path]::IsPathRooted($userProfile)) {
            throw 'Packaging requires an absolute builder user-profile path for deterministic native path remapping'
        }
        $userProfile = [IO.Path]::GetFullPath($userProfile).TrimEnd([IO.Path]::DirectorySeparatorChar)
        $encodedFlags = @(
            "--remap-path-prefix=$RepositoryRoot=multicore-src",
            "--remap-path-prefix=$userProfile=builder-home",
            '-Clink-arg=/PDBALTPATH:multicore.pdb'
        )
        $env:CARGO_ENCODED_RUSTFLAGS = $encodedFlags -join [char]0x1f
        $nativePathFlags = "/experimental:deterministic /pathmap:$userProfile=builder-home /d1trimfile:$userProfile\"
        $env:CFLAGS = $nativePathFlags
        $env:CXXFLAGS = $nativePathFlags
        $env:CARGO_TARGET_DIR = $TargetDirectory
        if ($UpdateRepository) {
            if ($UpdateRepository -cnotmatch '^[A-Za-z0-9][A-Za-z0-9._-]{0,99}/[A-Za-z0-9][A-Za-z0-9._-]{0,99}$') {
                throw 'UpdateRepository must use owner/repository syntax'
            }
            $env:MULTICORE_UPDATE_REPOSITORY = $UpdateRepository
        } else {
            Remove-Item Env:MULTICORE_UPDATE_REPOSITORY -ErrorAction SilentlyContinue
        }
        & cargo build --locked --release -p $Build.desktopPackage -p $Build.daemonPackage -p $Build.updaterPackage -p $Build.coreHostPackage
        if ($LASTEXITCODE -ne 0) { throw 'Pinned Rust release build failed' }
    } finally {
        Pop-Location
        if ($hadRustFlags) { $env:RUSTFLAGS = $savedRustFlags } else { Remove-Item Env:RUSTFLAGS -ErrorAction SilentlyContinue }
        if ($hadEncodedFlags) { $env:CARGO_ENCODED_RUSTFLAGS = $savedEncodedFlags } else { Remove-Item Env:CARGO_ENCODED_RUSTFLAGS -ErrorAction SilentlyContinue }
        if ($hadCFlags) { $env:CFLAGS = $savedCFlags } else { Remove-Item Env:CFLAGS -ErrorAction SilentlyContinue }
        if ($hadCxxFlags) { $env:CXXFLAGS = $savedCxxFlags } else { Remove-Item Env:CXXFLAGS -ErrorAction SilentlyContinue }
        if ($hadTargetDirectory) { $env:CARGO_TARGET_DIR = $savedTargetDirectory } else { Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue }
        if ($hadUpdateRepository) { $env:MULTICORE_UPDATE_REPOSITORY = $savedUpdateRepository } else { Remove-Item Env:MULTICORE_UPDATE_REPOSITORY -ErrorAction SilentlyContinue }
    }
}

function Copy-CreateNew {
    param([string]$Source, [string]$Destination)
    Assert-File $Source 'Input file'
    $input = [IO.File]::Open((Get-FullPath $Source), [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        $output = [IO.File]::Open((Get-FullPath $Destination), [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
        try { $input.CopyTo($output) } finally { $output.Dispose() }
    } finally { $input.Dispose() }
}

function New-ReleaseAsset {
    param([string]$PackageDirectory, [string]$AssetPath)
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $assetFull = Get-FullPath $AssetPath
    if ([IO.Path]::GetFileName($assetFull) -cne 'multicore-windows-x64.zip') { throw 'ReleaseAssetPath filename must be multicore-windows-x64.zip' }
    if (Test-Path -LiteralPath $assetFull) { throw "Release asset already exists: $assetFull" }
    $assetParent = Split-Path -Parent $assetFull
    if (-not (Test-Path -LiteralPath $assetParent)) { New-Item -ItemType Directory -Path $assetParent | Out-Null }
    Assert-NoReparseAncestor $assetParent
    $temporary = Join-Path $assetParent ('.multicore-release-{0}.zip' -f [Guid]::NewGuid().ToString('N'))
    try {
        $stream = [IO.File]::Open($temporary, [IO.FileMode]::CreateNew, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
        try {
            $archive = New-Object IO.Compression.ZipArchive($stream, [IO.Compression.ZipArchiveMode]::Create, $true)
            try {
                $inventory = Get-StageInventory $PackageDirectory
                foreach ($relative in $inventory.Files) {
                    $entryName = $relative.Replace('\', '/')
                    $entry = $archive.CreateEntry($entryName, [IO.Compression.CompressionLevel]::Optimal)
                    $entry.LastWriteTime = [DateTimeOffset]::new(1980, 1, 1, 0, 0, 0, [TimeSpan]::Zero)
                    $input = [IO.File]::Open((Join-Path $PackageDirectory $relative), [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
                    try {
                        $output = $entry.Open()
                        try { $input.CopyTo($output) } finally { $output.Dispose() }
                    } finally { $input.Dispose() }
                }
            } finally { $archive.Dispose() }
        } finally { $stream.Dispose() }
        [IO.File]::Move($temporary, $assetFull)
        Write-Output "Created GitHub release asset: $assetFull"
    } finally {
        if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Force }
    }
}

function Assert-PinnedHttpsUrl {
    param([string]$Url)
    $uri = [Uri]$Url
    if (-not $uri.IsAbsoluteUri -or $uri.Scheme -cne 'https' -or $uri.UserInfo -or $uri.Fragment) { throw "Only absolute pinned HTTPS URLs are permitted: $Url" }
    if ($uri.Host -cnotin @('github.com', 'raw.githubusercontent.com')) { throw "Unapproved download host: $($uri.Host)" }
}

function Get-InputFile {
    param([string]$Name, [string]$Url, [string]$Destination)
    if ($OfflineSourceDirectory) {
        $source = Join-Path (Get-FullPath $OfflineSourceDirectory) $Name
        Copy-CreateNew $source $Destination
    } else {
        Assert-PinnedHttpsUrl $Url
        if (Test-Path -LiteralPath $Destination) { throw "Refusing to overwrite staging input: $Destination" }
        Invoke-WebRequest -UseBasicParsing -Uri $Url -OutFile $Destination
    }
}

function Assert-Hash {
    param([string]$Path, [string]$Expected, [string]$Description)
    if ($Expected -cnotmatch '^[0-9a-f]{64}$') { throw "Invalid pinned SHA-256 for $Description" }
    $actual = Get-Sha256 $Path
    if ($actual -cne $Expected) { throw "SHA-256 mismatch for $Description (expected $Expected, got $actual)" }
}

function Assert-SafeArchiveName {
    param([string]$Name)
    if ([string]::IsNullOrWhiteSpace($Name) -or $Name.IndexOf([char]0) -ge 0) { throw 'ZIP contains an empty or NUL-bearing entry name' }
    if ($Name.StartsWith('/') -or $Name.StartsWith('\') -or $Name -match '^[A-Za-z]:' -or $Name.Contains(':')) { throw "ZIP contains an absolute, device, or ADS path: $Name" }
    $normalized = $Name.Replace('\', '/')
    $isDirectory = $normalized.EndsWith('/')
    $parts = $normalized.TrimEnd('/').Split('/')
    foreach ($part in $parts) {
        if (-not $part -or $part -eq '.' -or $part -eq '..' -or $part.EndsWith('.') -or $part.EndsWith(' ')) { throw "ZIP contains an unsafe path segment: $Name" }
        $baseName = $part.Split('.')[0]
        if ($baseName -match '^(?i:CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])$') { throw "ZIP contains a reserved Windows path: $Name" }
    }
    if (-not $isDirectory -and $normalized.EndsWith('/')) { throw "ZIP contains an invalid entry: $Name" }
    return $normalized
}

function Assert-SafeLeafName {
    param([string]$Name, [string]$Description)
    $normalized = Assert-SafeArchiveName $Name
    if ($normalized.Contains('/') -or $normalized -cne $Name) { throw "$Description must be one safe filename, not a path: $Name" }
}

function Assert-ExactProductionPin {
    param([string]$Name, [object]$Component)
    $pins = @{
        xray = @(
            'XTLS/Xray-core', 'v26.9.9', '52a412d9e2f5c2a5142b1b4e2ab3771dacb8b120', 'MPL-2.0',
            'Xray-windows-64.zip', 'https://github.com/XTLS/Xray-core/releases/download/v26.9.9/Xray-windows-64.zip',
            '244deaba2098c2964e49bba90df3707777e5f5f428a82d2f29604015f24beec2', 'xray.exe',
            '0d0fc0ea2b05641acb78c01fc36ad694e7b029861b2d5eb93da0e3e9fda9a98f', '268435456',
            'Xray-core-MPL-2.0.txt', 'https://raw.githubusercontent.com/XTLS/Xray-core/52a412d9e2f5c2a5142b1b4e2ab3771dacb8b120/LICENSE',
            '1f256ecad192880510e84ad60474eab7589218784b9a50bc7ceee34c2b91f1d5',
            'https://github.com/XTLS/Xray-core/archive/52a412d9e2f5c2a5142b1b4e2ab3771dacb8b120.zip',
            '0159e934d908cd176fed51dc61209e546046351928af685b143cad2ebe704831'
        )
        mihomo = @(
            'MetaCubeX/mihomo', 'v1.19.30', 'ac017cdd246ce8bd547653d927e7bf77d7ee73d5', 'GPL-3.0-only',
            'mihomo-windows-amd64-compatible-v1.19.30.zip', 'https://github.com/MetaCubeX/mihomo/releases/download/v1.19.30/mihomo-windows-amd64-compatible-v1.19.30.zip',
            '289fde5e29d37a5b3326480590d8b3551c5bf7f8737290355c19bce74d57a563', 'mihomo-windows-amd64-compatible.exe',
            '6ac25fcb26afe8e1bea24b6e6e80805bf884a33232d12e2d78dfa0b6c529ac14', '268435456',
            'mihomo-GPL-3.0.txt', 'https://raw.githubusercontent.com/MetaCubeX/mihomo/ac017cdd246ce8bd547653d927e7bf77d7ee73d5/LICENSE',
            '3972dc9744f6499f0f9b2dbf76696f2ae7ad8af9b23dde66d6af86c9dfb36986',
            'https://github.com/MetaCubeX/mihomo/archive/ac017cdd246ce8bd547653d927e7bf77d7ee73d5.zip',
            'd26880078fae7755c7ee9ce718ef6de5806d76c8d289237ec926b6a2f35d69be'
        )
    }
    $actual = @(
        [string]$Component.project, [string]$Component.version, [string]$Component.commit, [string]$Component.license,
        [string]$Component.asset.name, [string]$Component.asset.url, [string]$Component.asset.sha256, [string]$Component.asset.entry,
        [string]$Component.asset.executableSha256, [string]$Component.asset.maximumExpandedBytes,
        [string]$Component.licenseFile.name, [string]$Component.licenseFile.url, [string]$Component.licenseFile.sha256,
        [string]$Component.source.url, [string]$Component.source.sha256
    )
    $expected = $pins[$Name]
    if ($actual.Count -ne $expected.Count) { throw "Incomplete built-in pin tuple for $Name" }
    for ($index = 0; $index -lt $expected.Count; $index++) {
        if ($actual[$index] -cne $expected[$index]) { throw "Production manifest does not match the built-in immutable $Name pin tuple" }
    }
}

function Expand-ExactZipEntry {
    param([string]$ArchivePath, [string]$ExpectedEntry, [int64]$MaximumBytes, [string]$Destination)
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    if ($MaximumBytes -le 0 -or $MaximumBytes -gt 1073741824) { throw 'Expanded-entry bound is invalid' }
    $expectedNormalized = Assert-SafeArchiveName $ExpectedEntry
    $stream = [IO.File]::OpenRead((Get-FullPath $ArchivePath))
    try {
        $archive = New-Object IO.Compression.ZipArchive($stream, [IO.Compression.ZipArchiveMode]::Read, $false)
        try {
            $seen = @{}
            $matches = New-Object System.Collections.Generic.List[object]
            foreach ($entry in $archive.Entries) {
                $normalized = Assert-SafeArchiveName $entry.FullName
                $key = $normalized.ToUpperInvariant()
                if ($seen.ContainsKey($key)) { throw "ZIP contains duplicate/aliased entries: $($entry.FullName)" }
                $seen[$key] = $true
                $attributes = [BitConverter]::ToUInt32([BitConverter]::GetBytes([int]$entry.ExternalAttributes), 0)
                $unixType = ($attributes -shr 16) -band 0xf000
                if ($unixType -eq 0xa000 -or ($attributes -band 0x400) -ne 0) { throw "ZIP contains a link/reparse entry: $($entry.FullName)" }
                if ($entry.Length -gt $MaximumBytes) { throw "ZIP entry exceeds the configured expanded-size bound: $($entry.FullName)" }
                if ($normalized -ceq $expectedNormalized) { $matches.Add($entry) }
            }
            if ($matches.Count -ne 1) { throw "ZIP must contain exactly one exact entry named $ExpectedEntry" }
            $entryStream = $matches[0].Open()
            try {
                $output = [IO.File]::Open((Get-FullPath $Destination), [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
                try {
                    $buffer = New-Object byte[] 65536
                    [int64]$total = 0
                    while (($read = $entryStream.Read($buffer, 0, $buffer.Length)) -gt 0) {
                        $total += $read
                        if ($total -gt $MaximumBytes) { throw 'Expanded ZIP entry exceeded its configured bound while streaming' }
                        $output.Write($buffer, 0, $read)
                    }
                } finally { $output.Dispose() }
            } finally { $entryStream.Dispose() }
        } finally { $archive.Dispose() }
    } finally { $stream.Dispose() }
}

function Read-UInt16 { param([byte[]]$Bytes, [int]$Offset) [BitConverter]::ToUInt16($Bytes, $Offset) }
function Read-UInt32 { param([byte[]]$Bytes, [int]$Offset) [BitConverter]::ToUInt32($Bytes, $Offset) }
function Read-UInt64 { param([byte[]]$Bytes, [int]$Offset) [BitConverter]::ToUInt64($Bytes, $Offset) }

function Assert-Amd64PeExecutable {
    param([string]$Path, [string]$Description)
    Assert-File $Path $Description
    $bytes = [IO.File]::ReadAllBytes((Get-FullPath $Path))
    if ($bytes.Length -lt 512 -or $bytes[0] -ne 0x4d -or $bytes[1] -ne 0x5a) { throw "$Description is not a coherent PE executable" }
    $pe = [int](Read-UInt32 $bytes 0x3c)
    if ($pe -lt 0x40 -or $pe -gt ($bytes.Length - 24) -or $bytes[$pe] -ne 0x50 -or $bytes[$pe + 1] -ne 0x45 -or $bytes[$pe + 2] -ne 0 -or $bytes[$pe + 3] -ne 0) { throw "$Description has an invalid PE header" }
    $coff = $pe + 4
    $machine = Read-UInt16 $bytes $coff
    $sections = Read-UInt16 $bytes ($coff + 2)
    $optionalSize = Read-UInt16 $bytes ($coff + 16)
    $characteristics = Read-UInt16 $bytes ($coff + 18)
    $optional = $coff + 20
    if ($machine -ne 0x8664) { throw "$Description is not AMD64" }
    if ($sections -lt 1 -or $sections -gt 96 -or $optionalSize -lt 0xf0 -or ($optional + $optionalSize) -gt $bytes.Length) { throw "$Description has incoherent PE/COFF dimensions" }
    if (($characteristics -band 0x0002) -eq 0 -or ($characteristics -band 0x2000) -ne 0) { throw "$Description is not a non-DLL executable image" }
    if ((Read-UInt16 $bytes $optional) -ne 0x020b) { throw "$Description is not PE32+" }
    $entryPoint = Read-UInt32 $bytes ($optional + 16)
    $sectionAlignment = Read-UInt32 $bytes ($optional + 32)
    $fileAlignment = Read-UInt32 $bytes ($optional + 36)
    $sectionTable = $optional + $optionalSize
    [uint64]$sectionTableEnd = $sectionTable + 40 * $sections
    if ($sectionTableEnd -gt $bytes.Length) { throw "$Description has a truncated section table" }
    $sizeOfImage = Read-UInt32 $bytes ($optional + 56)
    $sizeOfHeaders = Read-UInt32 $bytes ($optional + 60)
    $subsystem = Read-UInt16 $bytes ($optional + 68)
    [uint64]$stackReserve = Read-UInt64 $bytes ($optional + 72)
    [uint64]$stackCommit = Read-UInt64 $bytes ($optional + 80)
    [uint64]$heapReserve = Read-UInt64 $bytes ($optional + 88)
    [uint64]$heapCommit = Read-UInt64 $bytes ($optional + 96)
    $directoryCount = Read-UInt32 $bytes ($optional + 108)
    if ($directoryCount -gt 16 -or (112 + (8 * $directoryCount)) -gt $optionalSize) { throw "$Description has incoherent PE32+ data directories" }
    if ($entryPoint -eq 0 -or $entryPoint -ge $sizeOfImage -or $sizeOfImage -eq 0 -or $sizeOfHeaders -lt $sectionTableEnd -or $sizeOfHeaders -gt $bytes.Length) { throw "$Description has incoherent entry/image/header sizes" }
    if ($subsystem -notin @(2, 3)) { throw "$Description does not target the supported Windows GUI/CUI subsystems" }
    if ($stackReserve -eq 0 -or $stackCommit -eq 0 -or $stackCommit -gt $stackReserve -or $heapReserve -eq 0 -or $heapCommit -eq 0 -or $heapCommit -gt $heapReserve) { throw "$Description has incoherent loader stack/heap bounds" }
    if ($fileAlignment -lt 512 -or $fileAlignment -gt 65536 -or (($fileAlignment -band ($fileAlignment - 1)) -ne 0)) { throw "$Description has an invalid PE file alignment" }
    if ($sectionAlignment -eq 0 -or (($sectionAlignment -band ($sectionAlignment - 1)) -ne 0) -or $sectionAlignment -lt $fileAlignment) { throw "$Description has an invalid PE section alignment" }
    if ($sectionAlignment -lt 4096 -and $sectionAlignment -ne $fileAlignment) { throw "$Description uses unsupported sub-page PE alignment" }
    if (($sizeOfHeaders % $fileAlignment) -ne 0 -or ($sizeOfImage % $sectionAlignment) -ne 0) { throw "$Description has unaligned PE header/image sizes" }
    $entryInExecutableSection = $false
    for ($index = 0; $index -lt $sections; $index++) {
        $section = $sectionTable + (40 * $index)
        [uint64]$virtualSize = Read-UInt32 $bytes ($section + 8)
        [uint64]$virtualAddress = Read-UInt32 $bytes ($section + 12)
        [uint64]$rawSize = Read-UInt32 $bytes ($section + 16)
        [uint64]$rawPointer = Read-UInt32 $bytes ($section + 20)
        [uint32]$sectionCharacteristics = Read-UInt32 $bytes ($section + 36)
        if (($virtualAddress % $sectionAlignment) -ne 0 -or ($rawSize % $fileAlignment) -ne 0 -or ($rawSize -gt 0 -and ($rawPointer % $fileAlignment) -ne 0)) { throw "$Description has an unaligned section" }
        if ($rawSize -gt 0 -and ($rawPointer -lt $sizeOfHeaders -or ($rawPointer + $rawSize) -gt $bytes.Length)) { throw "$Description has a section outside the file" }
        $mappedSize = [Math]::Max($virtualSize, $rawSize)
        if ($mappedSize -gt 0 -and ($virtualAddress + $mappedSize) -gt $sizeOfImage) { throw "$Description has a section outside the image" }
        if (($sectionCharacteristics -band 0x20000000) -ne 0 -and $entryPoint -ge $virtualAddress -and $entryPoint -lt ($virtualAddress + $mappedSize)) { $entryInExecutableSection = $true }
    }
    if (-not $entryInExecutableSection) { throw "$Description entry point is not in an executable section" }
}

function Get-StageInventory {
    param([string]$Root)
    $rootFull = Get-FullPath $Root
    $pending = New-Object 'System.Collections.Generic.Queue[string]'
    $pending.Enqueue($rootFull)
    $files = New-Object System.Collections.Generic.List[string]
    $directories = New-Object System.Collections.Generic.List[string]
    while ($pending.Count -gt 0) {
        $current = $pending.Dequeue()
        foreach ($item in @(Get-ChildItem -LiteralPath $current -Force)) {
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw "Staging inventory contains a reparse entry: $($item.FullName)" }
            $relative = $item.FullName.Substring($rootFull.Length + 1).Replace('\', '/')
            if ($item.PSIsContainer) {
                $directories.Add($relative)
                $pending.Enqueue($item.FullName)
            } else {
                $files.Add($relative)
            }
        }
    }
    [string[]]$fileArray = $files.ToArray()
    [string[]]$directoryArray = $directories.ToArray()
    [Array]::Sort($fileArray, [StringComparer]::Ordinal)
    [Array]::Sort($directoryArray, [StringComparer]::Ordinal)
    [PSCustomObject]@{ Files = $fileArray; Directories = $directoryArray }
}

$manifestFull = Get-FullPath $ManifestPath
Assert-File $manifestFull 'Version manifest'
$defaultManifestFull = Get-FullPath $DefaultManifestPath
$isProductionManifest = $manifestFull.Equals($defaultManifestFull, [StringComparison]::OrdinalIgnoreCase)
if (-not $OfflineSourceDirectory -and -not $isProductionManifest) {
    throw 'A custom manifest is permitted only with OfflineSourceDirectory fixture mode'
}

$destination = Get-FullPath $DestinationPath
Assert-NoReparseAncestor $destination
if (Test-Path -LiteralPath $destination) { throw "Destination already exists and will not be modified: $destination" }
$parent = Split-Path -Parent $destination
if (-not (Test-Path -LiteralPath $parent)) { New-Item -ItemType Directory -Path $parent | Out-Null }
Assert-NoReparseAncestor $parent

$releaseAsset = $null
if ($ReleaseAssetPath) {
    $releaseAsset = Get-FullPath $ReleaseAssetPath
    $destinationPrefix = $destination.TrimEnd('\') + '\'
    if ($releaseAsset.StartsWith($destinationPrefix, [StringComparison]::OrdinalIgnoreCase)) { throw 'Release asset cannot be inside the portable package directory' }
    if (Test-Path -LiteralPath $releaseAsset) { throw "Release asset already exists: $releaseAsset" }
}

if ($OfflineSourceDirectory) { Assert-NoReparseAncestor $OfflineSourceDirectory }

$stage = Join-Path $parent ('.multicore-stage-{0}-{1}' -f ([IO.Path]::GetFileName($destination)), [Guid]::NewGuid().ToString('N'))
if (Test-Path -LiteralPath $stage) { throw 'Unexpected staging collision' }
New-Item -ItemType Directory -Path $stage | Out-Null
$published = $false
try {
    foreach ($directory in @('cores', 'runtime', 'licenses', '.inputs')) { New-Item -ItemType Directory -Path (Join-Path $stage $directory) | Out-Null }
    $stagedManifest = Join-Path $stage 'versions.json'
    Copy-CreateNew $manifestFull $stagedManifest
    $manifest = Get-Content -Raw -LiteralPath $stagedManifest | ConvertFrom-Json
    if ($manifest.schemaVersion -ne 1 -or $manifest.platform -cne 'windows-x64') { throw 'Unsupported Windows package manifest' }
    if ($manifest.build.rustToolchain -cne '1.98.1' -or $manifest.build.desktopPackage -cne 'multicore-desktop' -or $manifest.build.daemonPackage -cne 'multicore-daemon' -or $manifest.build.updaterPackage -cne 'multicore-updater' -or $manifest.build.coreHostPackage -cne 'multicore-core-host') { throw 'Unexpected build provenance in manifest' }
    foreach ($componentName in @('xray', 'mihomo')) {
        $component = $manifest.components.$componentName
        if ($component.commit -cnotmatch '^[0-9a-f]{40}$') { throw "$componentName must use a full lowercase commit pin" }
        foreach ($hash in @($component.asset.sha256, $component.asset.executableSha256, $component.licenseFile.sha256, $component.source.sha256)) {
            if ($hash -cnotmatch '^[0-9a-f]{64}$') { throw "$componentName contains an invalid SHA-256 pin" }
        }
        Assert-SafeLeafName $component.asset.name "$componentName archive name"
        Assert-SafeLeafName $component.licenseFile.name "$componentName license name"
        [void](Assert-SafeArchiveName $component.asset.entry)
        if (-not $OfflineSourceDirectory) {
            Assert-ExactProductionPin $componentName $component
            Assert-PinnedHttpsUrl $component.asset.url
            Assert-PinnedHttpsUrl $component.licenseFile.url
            Assert-PinnedHttpsUrl $component.source.url
            if (-not $component.licenseFile.url.Contains($component.commit) -or -not $component.source.url.Contains($component.commit)) { throw "$componentName license/source URLs are not pinned to its full commit" }
        }
    }

    if ($isProductionManifest) {
        if ($DesktopExecutablePath -or $DaemonExecutablePath -or $UpdaterExecutablePath -or $CoreHostExecutablePath) {
            throw 'Production packaging builds all Rust executables internally; executable overrides are fixture-only'
        }
        $rustTarget = Join-Path $stage '.rust-target'
        Invoke-PinnedRustBuild $manifest.build $rustTarget $UpdateRepository
        $DesktopExecutablePath = Join-Path $rustTarget 'release\multicore-desktop.exe'
        $DaemonExecutablePath = Join-Path $rustTarget 'release\multicore-daemon.exe'
        $UpdaterExecutablePath = Join-Path $rustTarget 'release\multicore-apply.exe'
        $CoreHostExecutablePath = Join-Path $rustTarget 'release\multicore-core-host.exe'
    } elseif (-not $DesktopExecutablePath -or -not $DaemonExecutablePath -or -not $UpdaterExecutablePath -or -not $CoreHostExecutablePath) {
        throw 'Fixture packaging requires all executable paths'
    }
    $stagedDesktop = Join-Path $stage 'MultiCore.exe'
    $stagedDaemon = Join-Path $stage 'runtime\multicore-daemon.exe'
    $stagedUpdater = Join-Path $stage 'runtime\multicore-updater.exe'
    $stagedCoreHost = Join-Path $stage 'runtime\multicore-core-host.exe'
    Copy-CreateNew $DesktopExecutablePath $stagedDesktop
    Copy-CreateNew $DaemonExecutablePath $stagedDaemon
    Copy-CreateNew $UpdaterExecutablePath $stagedUpdater
    Copy-CreateNew $CoreHostExecutablePath $stagedCoreHost
    Assert-Amd64PeExecutable $stagedDesktop 'Staged desktop executable'
    Assert-Amd64PeExecutable $stagedDaemon 'Staged daemon executable'
    Assert-Amd64PeExecutable $stagedUpdater 'Staged updater executable'
    Assert-Amd64PeExecutable $stagedCoreHost 'Staged core-host executable'
    if ($isProductionManifest) {
        Assert-NoPrivateBuildPaths $stagedDesktop 'Staged desktop executable'
        Assert-NoPrivateBuildPaths $stagedDaemon 'Staged daemon executable'
        Assert-NoPrivateBuildPaths $stagedUpdater 'Staged updater executable'
        Assert-NoPrivateBuildPaths $stagedCoreHost 'Staged core-host executable'
        $rustTargetFull = Get-FullPath $rustTarget
        if ((Split-Path -Parent $rustTargetFull) -cne (Get-FullPath $stage) -or (Split-Path -Leaf $rustTargetFull) -cne '.rust-target') {
            throw 'Refusing unsafe isolated Rust target cleanup'
        }
        Remove-Item -LiteralPath $rustTargetFull -Recurse -Force
    }

    foreach ($componentName in @('xray', 'mihomo')) {
        $component = $manifest.components.$componentName
        $archive = Join-Path $stage ('.inputs\' + $component.asset.name)
        Get-InputFile $component.asset.name $component.asset.url $archive
        Assert-Hash $archive $component.asset.sha256 "$componentName archive"
        $coreTarget = Join-Path $stage ("cores\$componentName.exe")
        Expand-ExactZipEntry $archive $component.asset.entry ([int64]$component.asset.maximumExpandedBytes) $coreTarget
        Assert-Hash $coreTarget $component.asset.executableSha256 "$componentName executable"
        Assert-Amd64PeExecutable $coreTarget "$componentName executable"

        $licenseTarget = Join-Path $stage ('licenses\' + $component.licenseFile.name)
        Get-InputFile $component.licenseFile.name $component.licenseFile.url $licenseTarget
        Assert-Hash $licenseTarget $component.licenseFile.sha256 "$componentName license"
    }

    Copy-CreateNew (Join-Path $MetadataRoot 'README.md') (Join-Path $stage 'README.md')
    Copy-CreateNew (Join-Path $MetadataRoot 'THIRD_PARTY_NOTICES.md') (Join-Path $stage 'THIRD_PARTY_NOTICES.md')

    $inputs = Get-FullPath (Join-Path $stage '.inputs')
    if ((Split-Path -Parent $inputs) -cne (Get-FullPath $stage) -or (Split-Path -Leaf $inputs) -cne '.inputs') { throw 'Refusing unsafe staging-input cleanup' }
    Remove-Item -LiteralPath $inputs -Recurse -Force
    $inventory = Get-StageInventory $stage
    [string[]]$directories = $inventory.Directories
    [string[]]$expectedDirectories = @('cores', 'licenses', 'runtime')
    if (($directories -join '|') -cne ($expectedDirectories -join '|')) { throw "Staged directory inventory is not exact: $($directories -join ', ')" }
    [string[]]$files = $inventory.Files
    [string[]]$expected = @('MultiCore.exe', 'README.md', 'THIRD_PARTY_NOTICES.md', 'cores/mihomo.exe', 'cores/xray.exe', 'licenses/Xray-core-MPL-2.0.txt', 'licenses/mihomo-GPL-3.0.txt', 'runtime/multicore-core-host.exe', 'runtime/multicore-daemon.exe', 'runtime/multicore-updater.exe', 'versions.json')
    if (($files -join '|') -cne ($expected -join '|')) { throw "Staged file inventory is not exact: $($files -join ', ')" }
    $lines = foreach ($relative in $files) { '{0} *{1}' -f (Get-Sha256 (Join-Path $stage $relative)), $relative }
    $sumPath = Join-Path $stage 'SHA256SUMS.txt'
    $encoded = (New-Object Text.UTF8Encoding($false)).GetBytes(($lines -join "`n") + "`n")
    $sumStream = [IO.File]::Open($sumPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try { $sumStream.Write($encoded, 0, $encoded.Length) } finally { $sumStream.Dispose() }

    $finalInventory = Get-StageInventory $stage
    [string[]]$finalFiles = $finalInventory.Files
    [string[]]$expectedFinalFiles = @('MultiCore.exe', 'README.md', 'SHA256SUMS.txt', 'THIRD_PARTY_NOTICES.md', 'cores/mihomo.exe', 'cores/xray.exe', 'licenses/Xray-core-MPL-2.0.txt', 'licenses/mihomo-GPL-3.0.txt', 'runtime/multicore-core-host.exe', 'runtime/multicore-daemon.exe', 'runtime/multicore-updater.exe', 'versions.json')
    if (($finalFiles -join '|') -cne ($expectedFinalFiles -join '|')) { throw "Final file inventory is not exact: $($finalFiles -join ', ')" }
    [string[]]$finalDirectories = $finalInventory.Directories
    if (($finalDirectories -join '|') -cne ($expectedDirectories -join '|')) { throw "Final directory inventory is not exact: $($finalDirectories -join ', ')" }

    if (Test-Path -LiteralPath $destination) { throw 'Destination appeared during staging; refusing publication' }
    [IO.Directory]::Move($stage, $destination)
    try {
        if ($releaseAsset) { New-ReleaseAsset $destination $releaseAsset }
    } catch {
        $destinationFull = Get-FullPath $destination
        $parentPrefix = (Get-FullPath $parent).TrimEnd('\') + '\'
        if (-not $destinationFull.StartsWith($parentPrefix, [StringComparison]::OrdinalIgnoreCase)) { throw 'Refusing unsafe failed-publication cleanup' }
        if (Test-Path -LiteralPath $destinationFull) { Remove-Item -LiteralPath $destinationFull -Recurse -Force }
        throw
    }
    $published = $true
    Write-Output "Created local preview package: $destination"
} finally {
    if (-not $published -and (Test-Path -LiteralPath $stage)) {
        $stageFull = Get-FullPath $stage
        $parentPrefix = (Get-FullPath $parent).TrimEnd('\') + '\'
        if (-not $stageFull.StartsWith($parentPrefix, [StringComparison]::OrdinalIgnoreCase) -or -not ([IO.Path]::GetFileName($stageFull)).StartsWith('.multicore-stage-', [StringComparison]::Ordinal)) { throw 'Refusing unsafe staging cleanup' }
        Remove-Item -LiteralPath $stageFull -Recurse -Force
    }
}
