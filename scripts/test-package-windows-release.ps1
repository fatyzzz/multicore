[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$RepositoryRoot = Split-Path -Parent $PSScriptRoot
$Packager = Join-Path $PSScriptRoot 'package-windows-release.ps1'
$PackagerSource = Get-Content -Raw -LiteralPath $Packager
$MetadataRoot = Join-Path $RepositoryRoot 'packaging\windows-x64'

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw "ASSERTION FAILED: $Message" }
}

function Assert-NoStageResidue {
    param([string]$Parent, [string]$Context)
    $residue = @(Get-ChildItem -LiteralPath $Parent -Force -Directory -Filter '.multicore-stage-*' -ErrorAction SilentlyContinue)
    $residueNames = @($residue | ForEach-Object { $_.Name })
    Assert-True ($residue.Count -eq 0) "$Context left sibling staging residue: $($residueNames -join ', ')"
}

function Set-UInt16 {
    param([byte[]]$Bytes, [int]$Offset, [uint16]$Value)
    [BitConverter]::GetBytes($Value).CopyTo($Bytes, $Offset)
}

function Set-UInt32 {
    param([byte[]]$Bytes, [int]$Offset, [uint32]$Value)
    [BitConverter]::GetBytes($Value).CopyTo($Bytes, $Offset)
}

function Set-UInt64 {
    param([byte[]]$Bytes, [int]$Offset, [uint64]$Value)
    [BitConverter]::GetBytes($Value).CopyTo($Bytes, $Offset)
}

function New-TestPe {
    param(
        [string]$Path,
        [uint16]$Machine = 0x8664,
        [switch]$Dll,
        [switch]$Malformed,
        [uint32]$SizeOfHeaders = 0x0200,
        [uint32]$SectionAlignment = 0x1000,
        [uint32]$FileAlignment = 0x0200,
        [uint16]$Subsystem = 3,
        [uint64]$StackReserve = 0x100000,
        [uint64]$StackCommit = 0x1000,
        [uint64]$HeapReserve = 0x100000,
        [uint64]$HeapCommit = 0x1000
    )
    $bytes = New-Object byte[] 1024
    if (-not $Malformed) {
        $bytes[0] = 0x4d; $bytes[1] = 0x5a
        Set-UInt32 $bytes 0x3c 0x80
        $bytes[0x80] = 0x50; $bytes[0x81] = 0x45
        Set-UInt16 $bytes 0x84 $Machine
        Set-UInt16 $bytes 0x86 1
        Set-UInt16 $bytes 0x94 0xf0
        $characteristics = [uint16]0x0022
        if ($Dll) { $characteristics = [uint16]($characteristics -bor 0x2000) }
        Set-UInt16 $bytes 0x96 $characteristics
        Set-UInt16 $bytes 0x98 0x020b
        Set-UInt32 $bytes 0xa8 0x1000
        Set-UInt64 $bytes 0xb0 0x0000000140000000
        Set-UInt32 $bytes 0xb8 $SectionAlignment
        Set-UInt32 $bytes 0xbc $FileAlignment
        Set-UInt32 $bytes 0xd0 0x2000
        Set-UInt32 $bytes 0xd4 $SizeOfHeaders
        Set-UInt16 $bytes 0xdc $Subsystem
        Set-UInt64 $bytes 0xe0 $StackReserve
        Set-UInt64 $bytes 0xe8 $StackCommit
        Set-UInt64 $bytes 0xf0 $HeapReserve
        Set-UInt64 $bytes 0xf8 $HeapCommit
        Set-UInt32 $bytes 0x104 16
        [Text.Encoding]::ASCII.GetBytes('.text').CopyTo($bytes, 0x188)
        Set-UInt32 $bytes 0x190 0x100
        Set-UInt32 $bytes 0x194 0x1000
        Set-UInt32 $bytes 0x198 0x200
        Set-UInt32 $bytes 0x19c 0x200
        Set-UInt32 $bytes 0x1ac 0x60000020
    }
    [IO.File]::WriteAllBytes($Path, $bytes)
}

function New-TestZip {
    param([string]$Path, [object[]]$Entries)
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $stream = [IO.File]::Open($Path, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try {
        $archive = New-Object IO.Compression.ZipArchive($stream, [IO.Compression.ZipArchiveMode]::Create, $true)
        try {
            foreach ($item in $Entries) {
                $entry = $archive.CreateEntry([string]$item.Name, [IO.Compression.CompressionLevel]::NoCompression)
                if ($null -ne $item.Attributes) { $entry.ExternalAttributes = [int]$item.Attributes }
                $output = $entry.Open()
                try { $output.Write([byte[]]$item.Data, 0, ([byte[]]$item.Data).Length) } finally { $output.Dispose() }
            }
        } finally { $archive.Dispose() }
    } finally { $stream.Dispose() }
}

function Get-Sha256 { param([string]$Path) (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() }

function Write-TestManifest {
    param(
        [string]$Path,
        [string]$InputDirectory,
        [string]$XrayExecutableHash,
        [string]$MihomoExecutableHash,
        [int64]$MaximumExpandedBytes = 1048576,
        [string]$XrayArchiveHash,
        [string]$MihomoArchiveHash
    )
    if (-not $XrayArchiveHash) { $XrayArchiveHash = Get-Sha256 (Join-Path $InputDirectory 'xray-fixture.zip') }
    if (-not $MihomoArchiveHash) { $MihomoArchiveHash = Get-Sha256 (Join-Path $InputDirectory 'mihomo-fixture.zip') }
    $manifest = [ordered]@{
        schemaVersion = 1
        fixtureContract = 'multicore-package-test-v1'
        platform = 'windows-x64'
        build = [ordered]@{ rustToolchain = '1.98.1'; desktopPackage = 'multicore-desktop'; daemonPackage = 'multicore-daemon'; updaterPackage = 'multicore-updater'; coreHostPackage = 'multicore-core-host' }
        components = [ordered]@{
            xray = [ordered]@{
                project = 'XTLS/Xray-core'; version = 'fixture'; commit = ('1' * 40); license = 'MPL-2.0'
                asset = [ordered]@{ name = 'xray-fixture.zip'; url = 'https://example.invalid/xray-fixture.zip'; sha256 = $XrayArchiveHash; entry = 'xray.exe'; executableSha256 = $XrayExecutableHash; maximumExpandedBytes = $MaximumExpandedBytes }
                licenseFile = [ordered]@{ name = 'Xray-core-MPL-2.0.txt'; url = 'https://example.invalid/LICENSE'; sha256 = (Get-Sha256 (Join-Path $InputDirectory 'Xray-core-MPL-2.0.txt')) }
                source = [ordered]@{ url = 'https://example.invalid/source'; sha256 = ('2' * 64) }
            }
            mihomo = [ordered]@{
                project = 'MetaCubeX/mihomo'; version = 'fixture'; commit = ('3' * 40); license = 'GPL-3.0-only'
                asset = [ordered]@{ name = 'mihomo-fixture.zip'; url = 'https://example.invalid/mihomo-fixture.zip'; sha256 = $MihomoArchiveHash; entry = 'mihomo.exe'; executableSha256 = $MihomoExecutableHash; maximumExpandedBytes = $MaximumExpandedBytes }
                licenseFile = [ordered]@{ name = 'mihomo-GPL-3.0.txt'; url = 'https://example.invalid/LICENSE'; sha256 = (Get-Sha256 (Join-Path $InputDirectory 'mihomo-GPL-3.0.txt')) }
                source = [ordered]@{ url = 'https://example.invalid/source'; sha256 = ('4' * 64) }
            }
        }
    }
    $manifest | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $Path -Encoding UTF8
}

function Invoke-Package {
    param([string]$Destination, [string]$InputDirectory, [string]$Manifest, [string]$Desktop, [string]$Daemon, [string]$CoreHost, [string]$ReleaseAsset)
    $parameters = @{
        DestinationPath = $Destination
        OfflineSourceDirectory = $InputDirectory
        ManifestPath = $Manifest
        DesktopExecutablePath = $Desktop
        DaemonExecutablePath = $Daemon
        CoreHostExecutablePath = $CoreHost
        UpdaterExecutablePath = $Updater
        FixtureContractMode = $true
    }
    if ($ReleaseAsset) { $parameters.ReleaseAssetPath = $ReleaseAsset }
    & $Packager @parameters
}

function Assert-PackageFailure {
    param([string]$Name, [scriptblock]$Arrange)
    $caseRoot = Join-Path $TestRoot $Name
    New-Item -ItemType Directory -Path $caseRoot | Out-Null
    $input = Join-Path $caseRoot 'input'; New-Item -ItemType Directory -Path $input | Out-Null
    Get-ChildItem -LiteralPath $BaseInput | Copy-Item -Destination $input -Recurse
    $manifest = Join-Path $caseRoot 'versions.json'
    & $Arrange $input $manifest
    $destination = Join-Path $caseRoot 'output'
    $failed = $false
    try { Invoke-Package $destination $input $manifest $Desktop $Daemon $CoreHost | Out-Null } catch { $failed = $true }
    Assert-True $failed "$Name must fail"
    Assert-True (-not (Test-Path -LiteralPath $destination)) "$Name must not publish a partial destination"
    Assert-NoStageResidue $caseRoot $Name
}

Assert-True (Test-Path -LiteralPath $Packager) 'packager script must exist'
Assert-True ($PackagerSource.Contains('$PrivilegedBrokerReleaseReady = $true')) 'production release gate marker must be true after broker Tasks 2-4 are complete'
Assert-True ($PackagerSource.Contains('$AssertReleaseReady')) 'packager must expose the explicit CI readiness check'
Assert-True ($PackagerSource.Contains("fixtureContract -ceq 'multicore-package-test-v1'")) 'fixture authorization must require the synthetic manifest marker'
Assert-True ($PackagerSource.IndexOf('$PrivilegedBrokerReleaseReady = $true', [StringComparison]::Ordinal) -lt $PackagerSource.IndexOf('function Invoke-PinnedRustBuild', [StringComparison]::Ordinal)) 'release gate must be evaluated before Rust builds or downloads'
foreach ($required in @('README.md', 'THIRD_PARTY_NOTICES.md', 'licenses\Xray-core-MPL-2.0.txt', 'licenses\mihomo-GPL-3.0.txt')) {
    Assert-True (Test-Path -LiteralPath (Join-Path $MetadataRoot $required)) "metadata file $required must exist"
}

$TestRoot = Join-Path ([IO.Path]::GetTempPath()) ("multicore-package-test-{0}" -f [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $TestRoot | Out-Null
try {
    $Desktop = Join-Path $TestRoot 'multicore-desktop.exe'; New-TestPe $Desktop
    $Daemon = Join-Path $TestRoot 'multicore-daemon.exe'; New-TestPe $Daemon
    $CoreHost = Join-Path $TestRoot 'multicore-core-host.exe'; New-TestPe $CoreHost
    $Updater = Join-Path $TestRoot 'multicore-apply.exe'; New-TestPe $Updater
    $xray = Join-Path $TestRoot 'xray.exe'; New-TestPe $xray
    $mihomo = Join-Path $TestRoot 'mihomo.exe'; New-TestPe $mihomo
    $xrayBytes = [IO.File]::ReadAllBytes($xray); $mihomoBytes = [IO.File]::ReadAllBytes($mihomo)
    $BaseInput = Join-Path $TestRoot 'base-input'; New-Item -ItemType Directory -Path $BaseInput | Out-Null
    New-TestZip (Join-Path $BaseInput 'xray-fixture.zip') @(@{ Name = 'xray.exe'; Data = $xrayBytes; Attributes = $null })
    New-TestZip (Join-Path $BaseInput 'mihomo-fixture.zip') @(@{ Name = 'mihomo.exe'; Data = $mihomoBytes; Attributes = $null })
    Copy-Item -LiteralPath (Join-Path $MetadataRoot 'licenses\Xray-core-MPL-2.0.txt') -Destination $BaseInput
    Copy-Item -LiteralPath (Join-Path $MetadataRoot 'licenses\mihomo-GPL-3.0.txt') -Destination $BaseInput
    $xrayHash = Get-Sha256 $xray; $mihomoHash = Get-Sha256 $mihomo
    $manifest = Join-Path $TestRoot 'versions.json'
    Write-TestManifest $manifest $BaseInput $xrayHash $mihomoHash

    $releaseReadyOutput = (& $Packager -AssertReleaseReady | Out-String).Trim()
    Assert-True ($releaseReadyOutput -ceq 'PASS: privileged broker release gate is open') 'production release gate must report ready after broker Tasks 2-4 are complete'

    $productionOfflineProbeError = $null
    try {
        & $Packager -DestinationPath (Join-Path $TestRoot 'production-offline-probe') -OfflineSourceDirectory $BaseInput -DesktopExecutablePath $Desktop -DaemonExecutablePath $Daemon -UpdaterExecutablePath $Updater | Out-Null
    } catch {
        $productionOfflineProbeError = $_.Exception.Message
    }
    Assert-True ($productionOfflineProbeError -ceq 'Production packaging builds all Rust executables internally; executable overrides are fixture-only') 'offline inputs must not allow executable overrides with the production manifest'

    $out1 = Join-Path $TestRoot 'package-one'; $out2 = Join-Path $TestRoot 'package-two'
    $releaseAsset = Join-Path $TestRoot 'multicore-windows-x64.zip'
    Invoke-Package $out1 $BaseInput $manifest $Desktop $Daemon $CoreHost $releaseAsset | Out-Null
    Invoke-Package $out2 $BaseInput $manifest $Desktop $Daemon $CoreHost | Out-Null
    [string[]]$expected = @('MultiCore.exe', 'README.md', 'SHA256SUMS.txt', 'THIRD_PARTY_NOTICES.md', 'cores/mihomo.exe', 'cores/xray.exe', 'licenses/Xray-core-MPL-2.0.txt', 'licenses/mihomo-GPL-3.0.txt', 'runtime/multicore-core-host.exe', 'runtime/multicore-daemon.exe', 'runtime/multicore-updater.exe', 'versions.json')
    $actualItems = @(Get-ChildItem -LiteralPath $out1 -Force -Recurse)
    foreach ($item in $actualItems) { Assert-True (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) "published inventory must not contain reparse entries: $($item.FullName)" }
    [string[]]$actual = @($actualItems | Where-Object { -not $_.PSIsContainer } | ForEach-Object { $_.FullName.Substring($out1.Length + 1).Replace('\', '/') })
    [Array]::Sort($actual, [StringComparer]::Ordinal)
    Assert-True (($actual -join '|') -ceq ($expected -join '|')) 'published inventory must be exact'
    [string[]]$expectedDirectories = @('cores', 'licenses', 'runtime')
    [string[]]$actualDirectories = @($actualItems | Where-Object { $_.PSIsContainer } | ForEach-Object { $_.FullName.Substring($out1.Length + 1).Replace('\', '/') })
    [Array]::Sort($actualDirectories, [StringComparer]::Ordinal)
    Assert-True (($actualDirectories -join '|') -ceq ($expectedDirectories -join '|')) 'published directory inventory must reject extra empty directories'
    Assert-True ((Get-Sha256 (Join-Path $out1 'MultiCore.exe')) -eq (Get-Sha256 $Desktop)) 'desktop executable must be renamed without modification'
    Assert-True ((Get-Sha256 (Join-Path $out1 'runtime\multicore-daemon.exe')) -eq (Get-Sha256 $Daemon)) 'daemon hash must match'
    Assert-True ((Get-Sha256 (Join-Path $out1 'runtime\multicore-updater.exe')) -eq (Get-Sha256 $Updater)) 'updater hash must match'
    Assert-True ((Get-Sha256 (Join-Path $out1 'runtime\multicore-core-host.exe')) -eq (Get-Sha256 $CoreHost)) 'core-host hash must match'
    Assert-True ((Get-Sha256 (Join-Path $out1 'cores\xray.exe')) -eq $xrayHash) 'Xray hash must match'
    Assert-True ((Get-Sha256 (Join-Path $out1 'cores\mihomo.exe')) -eq $mihomoHash) 'Mihomo hash must match'
    foreach ($relative in $expected) {
        Assert-True ((Get-Sha256 (Join-Path $out1 $relative)) -eq (Get-Sha256 (Join-Path $out2 $relative))) "deterministic content mismatch: $relative"
    }
    $sumLines = Get-Content -LiteralPath (Join-Path $out1 'SHA256SUMS.txt')
    Assert-True (-not ($sumLines -match 'SHA256SUMS.txt')) 'SHA256SUMS must exclude itself'
    Assert-True ($sumLines.Count -eq 11) 'SHA256SUMS must cover every other file'
    Assert-True (-not ($sumLines -match [regex]::Escape($TestRoot))) 'SHA256SUMS must not contain host paths'
    $sumPaths = New-Object System.Collections.Generic.List[string]
    foreach ($line in $sumLines) {
        Assert-True ($line -cmatch '^([0-9a-f]{64}) \*(.+)$') "invalid SHA256SUMS line: $line"
        $relative = $Matches[2]
        $sumPaths.Add($relative)
        Assert-True ((Get-Sha256 (Join-Path $out1 $relative)) -ceq $Matches[1]) "SHA256SUMS digest mismatch: $relative"
    }
    [string[]]$expectedSumPaths = @('MultiCore.exe', 'README.md', 'THIRD_PARTY_NOTICES.md', 'cores/mihomo.exe', 'cores/xray.exe', 'licenses/Xray-core-MPL-2.0.txt', 'licenses/mihomo-GPL-3.0.txt', 'runtime/multicore-core-host.exe', 'runtime/multicore-daemon.exe', 'runtime/multicore-updater.exe', 'versions.json')
    Assert-True (($sumPaths -join '|') -ceq ($expectedSumPaths -join '|')) 'SHA256SUMS paths must be complete and ordered'

    Assert-True (Test-Path -LiteralPath $releaseAsset -PathType Leaf) 'release ZIP asset must be created when requested'
    $zip = [IO.Compression.ZipFile]::OpenRead($releaseAsset)
    try {
        [string[]]$zipNames = @($zip.Entries | ForEach-Object { $_.FullName })
        [Array]::Sort($zipNames, [StringComparer]::Ordinal)
        Assert-True (($zipNames -join '|') -ceq ($expected -join '|')) 'release ZIP inventory must exactly match the portable package'
        foreach ($entry in $zip.Entries) {
            $stream = $entry.Open()
            try {
                $sha = [Security.Cryptography.SHA256]::Create()
                try { $entryHash = ([BitConverter]::ToString($sha.ComputeHash($stream))).Replace('-', '').ToLowerInvariant() } finally { $sha.Dispose() }
            } finally { $stream.Dispose() }
            Assert-True ($entryHash -ceq (Get-Sha256 (Join-Path $out1 $entry.FullName.Replace('/', '\')))) "release ZIP content mismatch: $($entry.FullName)"
        }
    } finally { $zip.Dispose() }

    $customOnlineDestination = Join-Path $TestRoot 'custom-online-output'
    $failed = $false
    try { & $Packager -DestinationPath $customOnlineDestination -ManifestPath $manifest -DesktopExecutablePath $Desktop -DaemonExecutablePath $Daemon -UpdaterExecutablePath $Updater -CoreHostExecutablePath $CoreHost | Out-Null } catch { $failed = $true }
    Assert-True $failed 'online mode must reject every custom manifest before network access'
    Assert-True (-not (Test-Path -LiteralPath $customOnlineDestination)) 'custom online manifest rejection must not publish'
    Assert-NoStageResidue $TestRoot 'custom online manifest rejection'

    $malformedDesktop = Join-Path $TestRoot 'malformed-desktop.exe'; New-TestPe $malformedDesktop -Malformed
    $malformedDesktopDestination = Join-Path $TestRoot 'malformed-desktop-output'
    $failed = $false
    try { Invoke-Package $malformedDesktopDestination $BaseInput $manifest $malformedDesktop $Daemon $CoreHost | Out-Null } catch { $failed = $true }
    Assert-True $failed 'malformed staged desktop executable must be rejected'
    Assert-True (-not (Test-Path -LiteralPath $malformedDesktopDestination)) 'malformed staged desktop failure must not publish'
    Assert-NoStageResidue $TestRoot 'malformed staged desktop rejection'

    Assert-PackageFailure 'bad-archive-hash' { param($i, $m) Write-TestManifest $m $i $xrayHash $mihomoHash -XrayArchiveHash ('0' * 64) }
    Assert-PackageFailure 'bad-executable-hash' { param($i, $m) Write-TestManifest $m $i ('0' * 64) $mihomoHash }

    $unsafeNames = @('../evil', '/absolute', '\\server\share\evil', '\\?\C:\evil', 'C:\evil', 'xray.exe:stream', 'dir.\evil', 'CON\evil')
    for ($n = 0; $n -lt $unsafeNames.Count; $n++) {
        $unsafe = $unsafeNames[$n]
        Assert-PackageFailure "unsafe-name-$n" {
            param($i, $m)
            Remove-Item -LiteralPath (Join-Path $i 'xray-fixture.zip')
            New-TestZip (Join-Path $i 'xray-fixture.zip') @(@{ Name = 'xray.exe'; Data = $xrayBytes; Attributes = $null }, @{ Name = $unsafe; Data = [byte[]](1); Attributes = $null })
            Write-TestManifest $m $i $xrayHash $mihomoHash
        }
    }
    Assert-PackageFailure 'duplicate-entry' {
        param($i, $m)
        Remove-Item -LiteralPath (Join-Path $i 'xray-fixture.zip')
        New-TestZip (Join-Path $i 'xray-fixture.zip') @(@{ Name = 'xray.exe'; Data = $xrayBytes; Attributes = $null }, @{ Name = 'XRAY.EXE'; Data = $xrayBytes; Attributes = $null })
        Write-TestManifest $m $i $xrayHash $mihomoHash
    }
    Assert-PackageFailure 'symlink-entry' {
        param($i, $m)
        Remove-Item -LiteralPath (Join-Path $i 'xray-fixture.zip')
        New-TestZip (Join-Path $i 'xray-fixture.zip') @(@{ Name = 'xray.exe'; Data = $xrayBytes; Attributes = ([int64]0xA1FF0000) })
        Write-TestManifest $m $i $xrayHash $mihomoHash
    }
    Assert-PackageFailure 'oversized-entry' { param($i, $m) Write-TestManifest $m $i $xrayHash $mihomoHash -MaximumExpandedBytes 512 }

    foreach ($peCase in @(@('malformed-pe', 0x8664, $false, $true), @('x86-pe', 0x014c, $false, $false), @('arm64-pe', 0xaa64, $false, $false), @('dll-pe', 0x8664, $true, $false))) {
        $caseName = $peCase[0]; $machine = [uint16]$peCase[1]; $isDll = [bool]$peCase[2]; $malformed = [bool]$peCase[3]
        Assert-PackageFailure $caseName {
            param($i, $m)
            $bad = Join-Path $TestRoot "$caseName.exe"
            New-TestPe $bad -Machine $machine -Dll:$isDll -Malformed:$malformed
            $badBytes = [IO.File]::ReadAllBytes($bad)
            Remove-Item -LiteralPath (Join-Path $i 'xray-fixture.zip')
            New-TestZip (Join-Path $i 'xray-fixture.zip') @(@{ Name = 'xray.exe'; Data = $badBytes; Attributes = $null })
            Write-TestManifest $m $i (Get-Sha256 $bad) $mihomoHash
        }
    }
    Assert-PackageFailure 'headers-too-small-pe' {
        param($i, $m)
        $bad = Join-Path $TestRoot 'headers-too-small.exe'; New-TestPe $bad -SizeOfHeaders 1
        Remove-Item -LiteralPath (Join-Path $i 'xray-fixture.zip')
        New-TestZip (Join-Path $i 'xray-fixture.zip') @(@{ Name = 'xray.exe'; Data = [IO.File]::ReadAllBytes($bad); Attributes = $null })
        Write-TestManifest $m $i (Get-Sha256 $bad) $mihomoHash
    }
    Assert-PackageFailure 'bad-file-alignment-pe' {
        param($i, $m)
        $bad = Join-Path $TestRoot 'bad-file-alignment.exe'; New-TestPe $bad -FileAlignment 0x0300
        Remove-Item -LiteralPath (Join-Path $i 'xray-fixture.zip')
        New-TestZip (Join-Path $i 'xray-fixture.zip') @(@{ Name = 'xray.exe'; Data = [IO.File]::ReadAllBytes($bad); Attributes = $null })
        Write-TestManifest $m $i (Get-Sha256 $bad) $mihomoHash
    }
    Assert-PackageFailure 'bad-section-alignment-pe' {
        param($i, $m)
        $bad = Join-Path $TestRoot 'bad-section-alignment.exe'; New-TestPe $bad -SectionAlignment 0x0800
        Remove-Item -LiteralPath (Join-Path $i 'xray-fixture.zip')
        New-TestZip (Join-Path $i 'xray-fixture.zip') @(@{ Name = 'xray.exe'; Data = [IO.File]::ReadAllBytes($bad); Attributes = $null })
        Write-TestManifest $m $i (Get-Sha256 $bad) $mihomoHash
    }
    Assert-PackageFailure 'unsupported-subsystem-pe' {
        param($i, $m)
        $bad = Join-Path $TestRoot 'unsupported-subsystem.exe'; New-TestPe $bad -Subsystem 7
        Remove-Item -LiteralPath (Join-Path $i 'xray-fixture.zip')
        New-TestZip (Join-Path $i 'xray-fixture.zip') @(@{ Name = 'xray.exe'; Data = [IO.File]::ReadAllBytes($bad); Attributes = $null })
        Write-TestManifest $m $i (Get-Sha256 $bad) $mihomoHash
    }
    Assert-PackageFailure 'bad-loader-bounds-pe' {
        param($i, $m)
        $bad = Join-Path $TestRoot 'bad-loader-bounds.exe'; New-TestPe $bad -StackReserve 0x1000 -StackCommit 0x2000
        Remove-Item -LiteralPath (Join-Path $i 'xray-fixture.zip')
        New-TestZip (Join-Path $i 'xray-fixture.zip') @(@{ Name = 'xray.exe'; Data = [IO.File]::ReadAllBytes($bad); Attributes = $null })
        Write-TestManifest $m $i (Get-Sha256 $bad) $mihomoHash
    }

    $existing = Join-Path $TestRoot 'existing-output'; New-Item -ItemType Directory -Path $existing | Out-Null
    Set-Content -LiteralPath (Join-Path $existing 'keep.txt') -Value 'untouched' -NoNewline
    $failed = $false
    try { Invoke-Package $existing $BaseInput $manifest $Desktop $Daemon $CoreHost | Out-Null } catch { $failed = $true }
    Assert-True $failed 'existing destination must be rejected'
    Assert-True ((Get-Content -Raw -LiteralPath (Join-Path $existing 'keep.txt')) -ceq 'untouched') 'existing destination must be untouched'
    Assert-NoStageResidue $TestRoot 'existing destination rejection'

    $junctionTarget = Join-Path $TestRoot 'junction-target'; New-Item -ItemType Directory -Path $junctionTarget | Out-Null
    $junction = Join-Path $TestRoot 'junction-parent'
    cmd.exe /d /c "mklink /J `"$junction`" `"$junctionTarget`"" | Out-Null
    Assert-True (Test-Path -LiteralPath $junction) 'junction fixture must be created'
    $junctionDestination = Join-Path $junction 'output'
    $failed = $false
    try { Invoke-Package $junctionDestination $BaseInput $manifest $Desktop $Daemon $CoreHost | Out-Null } catch { $failed = $true }
    Assert-True $failed 'reparse-point destination ancestor must be rejected'
    Assert-True (-not (Test-Path -LiteralPath $junctionDestination)) 'reparse ancestor failure must not publish'
    Assert-NoStageResidue $junctionTarget 'destination reparse ancestor rejection'

    $inputJunctionTarget = Join-Path $TestRoot 'input-junction-target'; New-Item -ItemType Directory -Path $inputJunctionTarget | Out-Null
    Copy-Item -LiteralPath $Desktop -Destination (Join-Path $inputJunctionTarget 'desktop.exe')
    $inputJunction = Join-Path $TestRoot 'input-junction'
    cmd.exe /d /c "mklink /J `"$inputJunction`" `"$inputJunctionTarget`"" | Out-Null
    Assert-True (Test-Path -LiteralPath $inputJunction) 'input junction fixture must be created'
    $inputJunctionDestination = Join-Path $TestRoot 'input-junction-output'
    $failed = $false
    try { & $Packager -DestinationPath $inputJunctionDestination -OfflineSourceDirectory $BaseInput -ManifestPath $manifest -DesktopExecutablePath (Join-Path $inputJunction 'desktop.exe') -DaemonExecutablePath $Daemon -UpdaterExecutablePath $Updater -CoreHostExecutablePath $CoreHost -FixtureContractMode | Out-Null } catch { $failed = $true }
    Assert-True $failed 'reparse-point input ancestor must be rejected'
    Assert-True (-not (Test-Path -LiteralPath $inputJunctionDestination)) 'input reparse ancestor failure must not publish'
    Assert-NoStageResidue $TestRoot 'input reparse ancestor rejection'

    Write-Output 'PASS: Windows release package contract and failure atomicity'
} finally {
    if (Test-Path -LiteralPath $TestRoot) { Remove-Item -LiteralPath $TestRoot -Recurse -Force }
}
