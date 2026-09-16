//! Verified release metadata and portable-bundle updates for MultiCore.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const WINDOWS_ASSET_NAME: &str = "multicore-windows-x64.zip";
pub const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_EXPANDED_BYTES: u64 = 1024 * 1024 * 1024;
pub const MAX_ARCHIVE_ENTRIES: usize = 256;

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("invalid update repository")]
    InvalidRepository,
    #[error("invalid stable release version")]
    InvalidVersion,
    #[error("invalid GitHub release response: {0}")]
    InvalidRelease(&'static str),
    #[error("invalid release JSON")]
    InvalidReleaseJson(#[from] serde_json::Error),
    #[error("update I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid update archive: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("invalid update bundle: {0}")]
    InvalidBundle(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Repository {
    owner: String,
    name: String,
}

impl Repository {
    pub fn parse(value: &str) -> Result<Self, UpdateError> {
        let mut parts = value.split('/');
        let owner = parts.next().unwrap_or_default();
        let name = parts.next().unwrap_or_default();
        if parts.next().is_some()
            || !valid_repository_segment(owner)
            || !valid_repository_segment(name)
        {
            return Err(UpdateError::InvalidRepository);
        }
        Ok(Self {
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn latest_release_api_url(&self) -> String {
        format!(
            "https://api.github.com/repos/{}/{}/releases/latest",
            self.owner, self.name
        )
    }
}

fn valid_repository_segment(value: &str) -> bool {
    value.len() <= 100
        && !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    pub fn parse(value: &str) -> Result<Self, UpdateError> {
        let value = value.strip_prefix('v').unwrap_or(value);
        let mut parts = value.split('.');
        let major = parse_version_part(parts.next())?;
        let minor = parse_version_part(parts.next())?;
        let patch = parse_version_part(parts.next())?;
        if parts.next().is_some() {
            return Err(UpdateError::InvalidVersion);
        }
        Ok(Self {
            major,
            minor,
            patch,
        })
    }
}

fn parse_version_part(value: Option<&str>) -> Result<u64, UpdateError> {
    let value = value.ok_or(UpdateError::InvalidVersion)?;
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(UpdateError::InvalidVersion);
    }
    value.parse().map_err(|_| UpdateError::InvalidVersion)
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReleaseDecision {
    Current,
    UpdateAvailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseAsset {
    download_url: String,
    size: u64,
    sha256_hex: String,
}

impl ReleaseAsset {
    pub fn download_url(&self) -> &str {
        &self.download_url
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn sha256_hex(&self) -> &str {
        &self.sha256_hex
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitHubRelease {
    version: Version,
    page_url: String,
    asset: ReleaseAsset,
}

impl GitHubRelease {
    pub fn parse(body: &[u8]) -> Result<Self, UpdateError> {
        let raw: RawRelease = serde_json::from_slice(body)?;
        let version = Version::parse(&raw.tag_name)?;
        if !raw.html_url.starts_with("https://github.com/") {
            return Err(UpdateError::InvalidRelease("unexpected release page URL"));
        }
        let mut matching = raw
            .assets
            .into_iter()
            .filter(|asset| asset.name == WINDOWS_ASSET_NAME);
        let raw_asset = matching
            .next()
            .ok_or(UpdateError::InvalidRelease("release asset is missing"))?;
        if matching.next().is_some() {
            return Err(UpdateError::InvalidRelease("release asset is duplicated"));
        }
        if raw_asset.size == 0 || raw_asset.size > MAX_ARCHIVE_BYTES {
            return Err(UpdateError::InvalidRelease("release asset size is invalid"));
        }
        if !raw_asset
            .browser_download_url
            .starts_with("https://github.com/")
            || !raw_asset
                .browser_download_url
                .ends_with(&format!("/{WINDOWS_ASSET_NAME}"))
        {
            return Err(UpdateError::InvalidRelease("unexpected asset URL"));
        }
        let digest = raw_asset
            .digest
            .strip_prefix("sha256:")
            .ok_or(UpdateError::InvalidRelease("SHA-256 digest is missing"))?;
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(UpdateError::InvalidRelease("SHA-256 digest is invalid"));
        }

        Ok(Self {
            version,
            page_url: raw.html_url,
            asset: ReleaseAsset {
                download_url: raw_asset.browser_download_url,
                size: raw_asset.size,
                sha256_hex: digest.to_owned(),
            },
        })
    }

    pub fn parse_for_repository(repository: &Repository, body: &[u8]) -> Result<Self, UpdateError> {
        let release = Self::parse(body)?;
        let base = format!(
            "https://github.com/{}/{}/releases/",
            repository.owner(),
            repository.name()
        )
        .to_ascii_lowercase();
        if !release.page_url.to_ascii_lowercase().starts_with(&base)
            || !release
                .asset
                .download_url
                .to_ascii_lowercase()
                .starts_with(&base)
        {
            return Err(UpdateError::InvalidRelease(
                "release URLs do not match the pinned repository",
            ));
        }
        Ok(release)
    }

    pub fn version(&self) -> Version {
        self.version
    }

    pub fn page_url(&self) -> &str {
        &self.page_url
    }

    pub fn asset(&self) -> &ReleaseAsset {
        &self.asset
    }

    pub fn decision(&self, current: &Version) -> ReleaseDecision {
        if self.version > *current {
            ReleaseDecision::UpdateAvailable
        } else {
            ReleaseDecision::Current
        }
    }
}

#[derive(Deserialize)]
struct RawRelease {
    tag_name: String,
    html_url: String,
    assets: Vec<RawAsset>,
}

#[derive(Deserialize)]
struct RawAsset {
    name: String,
    browser_download_url: String,
    size: u64,
    digest: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BundleInventory {
    files: usize,
    expanded_bytes: u64,
}

impl BundleInventory {
    pub fn files(self) -> usize {
        self.files
    }

    pub fn expanded_bytes(self) -> u64 {
        self.expanded_bytes
    }
}

pub fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn sha256_file(path: &Path) -> Result<String, UpdateError> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

pub fn verify_archive_file(
    path: &Path,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<(), UpdateError> {
    let actual_size = fs::metadata(path)?.len();
    if actual_size != expected_size {
        return Err(UpdateError::InvalidBundle(format!(
            "archive size mismatch: expected {expected_size}, got {actual_size}"
        )));
    }
    if !valid_sha256(expected_sha256) || sha256_file(path)? != expected_sha256 {
        return Err(UpdateError::InvalidBundle(
            "archive checksum mismatch".into(),
        ));
    }
    Ok(())
}

pub fn extract_verified_bundle(
    archive_path: &Path,
    destination: &Path,
) -> Result<BundleInventory, UpdateError> {
    if destination.exists() {
        return Err(UpdateError::InvalidBundle(
            "extraction destination already exists".into(),
        ));
    }
    fs::create_dir(destination)?;
    let result = extract_verified_bundle_inner(archive_path, destination);
    if result.is_err() {
        let _ = fs::remove_dir_all(destination);
    }
    result
}

fn extract_verified_bundle_inner(
    archive_path: &Path,
    destination: &Path,
) -> Result<BundleInventory, UpdateError> {
    let archive_file = File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(archive_file)?;
    if archive.is_empty() || archive.len() > MAX_ARCHIVE_ENTRIES {
        return Err(UpdateError::InvalidBundle(
            "archive entry count is invalid".into(),
        ));
    }

    let mut names = BTreeSet::new();
    let mut regular_files = BTreeSet::new();
    let mut expanded_bytes = 0_u64;
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        let name = safe_archive_name(entry.name())?;
        let collision_key = name.to_ascii_lowercase();
        if !names.insert(collision_key) {
            return Err(UpdateError::InvalidBundle("duplicate archive path".into()));
        }
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            return Err(UpdateError::InvalidBundle(
                "archive links are not allowed".into(),
            ));
        }
        if !entry.is_dir() {
            expanded_bytes = expanded_bytes.checked_add(entry.size()).ok_or_else(|| {
                UpdateError::InvalidBundle("expanded archive size overflow".into())
            })?;
            if expanded_bytes > MAX_EXPANDED_BYTES {
                return Err(UpdateError::InvalidBundle(
                    "expanded archive is too large".into(),
                ));
            }
            regular_files.insert(name);
        }
    }

    if !regular_files.contains("SHA256SUMS.txt") {
        return Err(UpdateError::InvalidBundle(
            "SHA256SUMS.txt is missing".into(),
        ));
    }
    let checksums = {
        let mut entry = archive.by_name("SHA256SUMS.txt")?;
        if entry.size() > 64 * 1024 {
            return Err(UpdateError::InvalidBundle(
                "checksum inventory is too large".into(),
            ));
        }
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut bytes)?;
        parse_checksums(&bytes)?
    };

    let payload_files = regular_files
        .iter()
        .filter(|name| name.as_str() != "SHA256SUMS.txt")
        .cloned()
        .collect::<BTreeSet<_>>();
    let listed_files = checksums.keys().cloned().collect::<BTreeSet<_>>();
    if payload_files != listed_files {
        return Err(UpdateError::InvalidBundle(
            "checksum inventory does not match archive files".into(),
        ));
    }

    for required in REQUIRED_BUNDLE_FILES {
        if !payload_files.contains(*required) {
            return Err(UpdateError::InvalidBundle(format!(
                "required bundle file is missing: {required}"
            )));
        }
    }

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let name = safe_archive_name(entry.name())?;
        let output_path = destination.join(Path::new(&name));
        if entry.is_dir() {
            fs::create_dir_all(&output_path)?;
            continue;
        }
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output = File::create(&output_path)?;
        let mut digest = Sha256::new();
        let mut written = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = entry.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            written = written
                .checked_add(read as u64)
                .ok_or_else(|| UpdateError::InvalidBundle("file size overflow".into()))?;
            if written > entry.size() || written > MAX_EXPANDED_BYTES {
                return Err(UpdateError::InvalidBundle(
                    "archive entry exceeded its declared size".into(),
                ));
            }
            digest.update(&buffer[..read]);
            output.write_all(&buffer[..read])?;
        }
        output.flush()?;
        if written != entry.size() {
            return Err(UpdateError::InvalidBundle(
                "archive entry size mismatch".into(),
            ));
        }
        let actual = format!("{:x}", digest.finalize());
        if name != "SHA256SUMS.txt" && checksums.get(&name) != Some(&actual) {
            return Err(UpdateError::InvalidBundle(format!(
                "checksum mismatch: {name}"
            )));
        }
    }

    for executable in REQUIRED_PE_FILES {
        validate_pe_x64(&destination.join(executable))?;
    }

    Ok(BundleInventory {
        files: regular_files.len(),
        expanded_bytes,
    })
}

const REQUIRED_BUNDLE_FILES: &[&str] = &[
    "MultiCore.exe",
    "README.md",
    "THIRD_PARTY_NOTICES.md",
    "cores/mihomo.exe",
    "cores/xray.exe",
    "licenses/Xray-core-MPL-2.0.txt",
    "licenses/mihomo-GPL-3.0.txt",
    "runtime/multicore-daemon.exe",
    "runtime/multicore-updater.exe",
    "versions.json",
];

const REQUIRED_PE_FILES: &[&str] = &[
    "MultiCore.exe",
    "cores/mihomo.exe",
    "cores/xray.exe",
    "runtime/multicore-daemon.exe",
    "runtime/multicore-updater.exe",
];

fn safe_archive_name(raw: &str) -> Result<String, UpdateError> {
    if raw.is_empty()
        || raw.starts_with('/')
        || raw.starts_with('\\')
        || raw.contains('\\')
        || raw.contains(':')
        || raw.bytes().any(|byte| byte == 0 || byte < 0x20)
    {
        return Err(UpdateError::InvalidBundle("unsafe archive path".into()));
    }
    let is_dir = raw.ends_with('/');
    let trimmed = raw.trim_end_matches('/');
    let path = Path::new(trimmed);
    if trimmed.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(UpdateError::InvalidBundle("unsafe archive path".into()));
    }
    let mut normalized = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if is_dir {
        normalized.push('/');
    }
    Ok(normalized)
}

fn parse_checksums(bytes: &[u8]) -> Result<BTreeMap<String, String>, UpdateError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| UpdateError::InvalidBundle("checksum inventory is not UTF-8".into()))?;
    let mut checksums = BTreeMap::new();
    for line in text.lines() {
        let (digest, name) = line
            .split_once(" *")
            .ok_or_else(|| UpdateError::InvalidBundle("invalid checksum line".into()))?;
        let name = safe_archive_name(name)?;
        if name.ends_with('/') || name == "SHA256SUMS.txt" || !valid_sha256(digest) {
            return Err(UpdateError::InvalidBundle(
                "invalid checksum inventory entry".into(),
            ));
        }
        if checksums.insert(name, digest.to_owned()).is_some() {
            return Err(UpdateError::InvalidBundle(
                "duplicate checksum inventory entry".into(),
            ));
        }
    }
    if checksums.is_empty() {
        return Err(UpdateError::InvalidBundle(
            "checksum inventory is empty".into(),
        ));
    }
    Ok(checksums)
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn validate_pe_x64(path: &Path) -> Result<(), UpdateError> {
    let mut file = File::open(path)?;
    let mut header = [0_u8; 64];
    file.read_exact(&mut header)?;
    if &header[0..2] != b"MZ" {
        return Err(UpdateError::InvalidBundle(format!(
            "required executable is not PE: {}",
            path.display()
        )));
    }
    let pe_offset = u32::from_le_bytes(header[0x3c..0x40].try_into().expect("fixed slice"));
    if pe_offset > 16 * 1024 * 1024 {
        return Err(UpdateError::InvalidBundle("invalid PE offset".into()));
    }
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::Start(pe_offset as u64))?;
    let mut signature_and_machine = [0_u8; 6];
    file.read_exact(&mut signature_and_machine)?;
    if &signature_and_machine[..4] != b"PE\0\0"
        || u16::from_le_bytes(signature_and_machine[4..6].try_into().expect("fixed slice"))
            != 0x8664
    {
        return Err(UpdateError::InvalidBundle(
            "required executable is not PE32+ AMD64".into(),
        ));
    }
    Ok(())
}

pub fn transactional_swap(target: &Path, staged: &Path, backup: &Path) -> Result<(), UpdateError> {
    validate_swap_paths(target, staged, backup)?;
    transactional_swap_with(target, staged, backup, |from, to| fs::rename(from, to))?;
    Ok(())
}

fn validate_swap_paths(target: &Path, staged: &Path, backup: &Path) -> Result<(), UpdateError> {
    if !target.is_absolute() || !staged.is_absolute() || !backup.is_absolute() {
        return Err(UpdateError::InvalidBundle(
            "update directories must be absolute".into(),
        ));
    }
    let parent = target
        .parent()
        .filter(|parent| parent.parent().is_some())
        .ok_or_else(|| UpdateError::InvalidBundle("unsafe install directory".into()))?;
    if staged.parent() != Some(parent)
        || backup.parent() != Some(parent)
        || target == staged
        || target == backup
        || staged == backup
        || backup.exists()
    {
        return Err(UpdateError::InvalidBundle(
            "update directories are not isolated siblings".into(),
        ));
    }
    for directory in [target, staged] {
        if !directory.is_dir()
            || !directory.join("MultiCore.exe").is_file()
            || !directory.join("SHA256SUMS.txt").is_file()
        {
            return Err(UpdateError::InvalidBundle(format!(
                "invalid portable directory: {}",
                directory.display()
            )));
        }
    }
    Ok(())
}

fn transactional_swap_with(
    target: &Path,
    staged: &Path,
    backup: &Path,
    mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    rename(target, backup)?;
    if let Err(publish_error) = rename(staged, target) {
        if let Err(rollback_error) = rename(backup, target) {
            return Err(std::io::Error::other(format!(
                "publication failed ({publish_error}); rollback failed ({rollback_error})"
            )));
        }
        return Err(publish_error);
    }
    Ok(())
}

#[cfg(test)]
mod publication_tests {
    use super::transactional_swap_with;
    use std::fs;
    use std::io;
    use tempfile::TempDir;

    #[test]
    fn failed_publication_restores_previous_install_directory() {
        let temp = TempDir::new().unwrap();
        let target = temp.path().join("MultiCore");
        let staged = temp.path().join("MultiCore.new");
        let backup = temp.path().join("MultiCore.previous");
        fs::create_dir(&target).unwrap();
        fs::create_dir(&staged).unwrap();
        fs::write(target.join("old.txt"), b"old").unwrap();

        let mut calls = 0;
        let result = transactional_swap_with(&target, &staged, &backup, |from, to| {
            calls += 1;
            if calls == 2 {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "fixture"));
            }
            fs::rename(from, to)
        });

        assert!(result.is_err());
        assert_eq!(fs::read(target.join("old.txt")).unwrap(), b"old");
        assert!(!backup.exists());
        assert!(staged.exists());
    }
}
