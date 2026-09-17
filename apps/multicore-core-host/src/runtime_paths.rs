use multicore_core::RuntimePaths;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    os::windows::{ffi::OsStringExt, fs::OpenOptionsExt, io::AsRawHandle},
    path::{Path, PathBuf},
};
use windows_sys::Win32::{
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
        GetFileInformationByHandle,
    },
    System::Com::CoTaskMemFree,
    UI::Shell::{FOLDERID_LocalAppData, KF_FLAG_DEFAULT, SHGetKnownFolderPath},
};

#[derive(Debug)]
pub(crate) enum ResolveError {
    InvalidGeneration,
    Layout,
    Reparse,
    Escape,
    Missing,
    InvalidPe,
    Io,
}

pub(crate) struct ResolvedRuntime {
    pub paths: RuntimePaths,
    guards: Vec<GuardedPath>,
}

struct GuardedPath {
    path: PathBuf,
    handle: File,
    identity: FileIdentity,
    directory: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct FileIdentity {
    volume: u32,
    index: u64,
}

impl ResolvedRuntime {
    pub(crate) fn verify_unchanged(&self) -> Result<(), ResolveError> {
        for guard in &self.guards {
            let reopened = open_guard(&guard.path, guard.directory)?;
            if reopened.identity != guard.identity {
                return Err(ResolveError::Escape);
            }
            let _ = guard.handle.as_raw_handle();
        }
        Ok(())
    }
}

pub(crate) fn resolve_current(generation: u64) -> Result<ResolvedRuntime, ResolveError> {
    let data = local_app_data()?.join("MultiCore");
    let executable = std::env::current_exe().map_err(|_| ResolveError::Io)?;
    resolve_from(&data, &executable, generation)
}

fn local_app_data() -> Result<PathBuf, ResolveError> {
    let mut raw = std::ptr::null_mut();
    if unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_LocalAppData,
            KF_FLAG_DEFAULT as u32,
            std::ptr::null_mut(),
            &mut raw,
        )
    } < 0
    {
        return Err(ResolveError::Io);
    }
    let mut length = 0;
    while unsafe { *raw.add(length) } != 0 {
        length += 1;
    }
    let result = PathBuf::from(std::ffi::OsString::from_wide(unsafe {
        std::slice::from_raw_parts(raw, length)
    }));
    unsafe { CoTaskMemFree(raw.cast()) };
    Ok(result)
}

pub(crate) fn resolve_from(
    data_root: &Path,
    host_executable: &Path,
    generation: u64,
) -> Result<ResolvedRuntime, ResolveError> {
    if generation == 0 {
        return Err(ResolveError::InvalidGeneration);
    }
    let runtime_dir = host_executable.parent().ok_or(ResolveError::Layout)?;
    if runtime_dir.file_name().and_then(|v| v.to_str()) != Some("runtime")
        || host_executable.file_name().and_then(|v| v.to_str()) != Some("multicore-core-host.exe")
    {
        return Err(ResolveError::Layout);
    }
    let package_lexical = runtime_dir.parent().ok_or(ResolveError::Layout)?;
    let cores = package_lexical.join("cores");
    let runtime_root = data_root.join("runtime");
    let generation_dir = runtime_root.join(format!("runtime-{generation:020}"));
    if generation_dir.file_name().and_then(|v| v.to_str())
        != Some(format!("runtime-{generation:020}").as_str())
    {
        return Err(ResolveError::InvalidGeneration);
    }
    let xray = cores.join("xray.exe");
    let mihomo = cores.join("mihomo.exe");
    let xray_config = generation_dir.join("xray.json");
    let mihomo_config = generation_dir.join("mihomo.yaml");
    let mut guards = Vec::new();
    for directory in [
        data_root,
        &runtime_root,
        &generation_dir,
        package_lexical,
        runtime_dir,
        &cores,
    ] {
        guard_lexical_chain(directory, true, &mut guards)?;
    }
    for file in [
        host_executable,
        &xray,
        &mihomo,
        &xray_config,
        &mihomo_config,
    ] {
        guard_lexical_chain(file, false, &mut guards)?;
    }

    let host = fs::canonicalize(host_executable).map_err(|_| ResolveError::Missing)?;
    let package = fs::canonicalize(package_lexical).map_err(|_| ResolveError::Missing)?;
    let canonical_data = fs::canonicalize(data_root).map_err(|_| ResolveError::Missing)?;
    let canonical_generation =
        fs::canonicalize(&generation_dir).map_err(|_| ResolveError::Missing)?;
    canonical_contained(&package, &host)?;
    canonical_contained(&canonical_data, &canonical_generation)?;
    for binary in [&xray, &mihomo] {
        canonical_contained(
            &package,
            &fs::canonicalize(binary).map_err(|_| ResolveError::Missing)?,
        )?;
    }
    for config in [&xray_config, &mihomo_config] {
        canonical_contained(
            &canonical_generation,
            &fs::canonicalize(config).map_err(|_| ResolveError::Missing)?,
        )?;
    }
    for binary in [&xray, &mihomo] {
        let guard = guards
            .iter_mut()
            .rev()
            .find(|guard| guard.path == *binary)
            .ok_or(ResolveError::Missing)?;
        validate_amd64_pe(&mut guard.handle)?;
    }
    Ok(ResolvedRuntime {
        paths: RuntimePaths {
            xray_binary: fs::canonicalize(xray).map_err(|_| ResolveError::Missing)?,
            mihomo_binary: fs::canonicalize(mihomo).map_err(|_| ResolveError::Missing)?,
            xray_config: fs::canonicalize(xray_config).map_err(|_| ResolveError::Missing)?,
            mihomo_config: fs::canonicalize(mihomo_config).map_err(|_| ResolveError::Missing)?,
        },
        guards,
    })
}

fn canonical_contained(root: &Path, target: &Path) -> Result<(), ResolveError> {
    let root = fs::canonicalize(root).map_err(|_| ResolveError::Missing)?;
    let target = fs::canonicalize(target).map_err(|_| ResolveError::Missing)?;
    if !target.starts_with(&root) {
        return Err(ResolveError::Escape);
    }
    Ok(())
}

fn guard_lexical_chain(
    path: &Path,
    target_directory: bool,
    guards: &mut Vec<GuardedPath>,
) -> Result<(), ResolveError> {
    let mut ancestors = path.ancestors().collect::<Vec<_>>();
    ancestors.reverse();
    for (index, ancestor) in ancestors.into_iter().enumerate() {
        if guards.iter().any(|guard| guard.path == ancestor) {
            continue;
        }
        let final_component = ancestor == path;
        let directory = if final_component {
            target_directory
        } else {
            true
        };
        if index == 0 && ancestor.parent().is_none() {
            continue;
        }
        guards.push(open_guard(ancestor, directory)?);
    }
    Ok(())
}

fn open_guard(path: &Path, directory: bool) -> Result<GuardedPath, ResolveError> {
    let handle = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(
            FILE_FLAG_OPEN_REPARSE_POINT
                | if directory {
                    FILE_FLAG_BACKUP_SEMANTICS
                } else {
                    0
                },
        )
        .open(path)
        .map_err(|_| ResolveError::Missing)?;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(handle.as_raw_handle(), &mut info) } == 0 {
        return Err(ResolveError::Io);
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(ResolveError::Reparse);
    }
    let actual_directory = info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    if actual_directory != directory {
        return Err(ResolveError::Layout);
    }
    Ok(GuardedPath {
        path: path.to_path_buf(),
        identity: FileIdentity {
            volume: info.dwVolumeSerialNumber,
            index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        },
        handle,
        directory,
    })
}

fn validate_amd64_pe(file: &mut File) -> Result<(), ResolveError> {
    let mut dos = [0u8; 64];
    file.read_exact(&mut dos)
        .map_err(|_| ResolveError::InvalidPe)?;
    if &dos[..2] != b"MZ" {
        return Err(ResolveError::InvalidPe);
    }
    let offset = u32::from_le_bytes(dos[0x3c..0x40].try_into().unwrap()) as u64;
    if !(64..=16 * 1024 * 1024).contains(&offset) {
        return Err(ResolveError::InvalidPe);
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(|_| ResolveError::InvalidPe)?;
    let mut header = [0u8; 26];
    file.read_exact(&mut header)
        .map_err(|_| ResolveError::InvalidPe)?;
    let machine = u16::from_le_bytes([header[4], header[5]]);
    let characteristics = u16::from_le_bytes([header[22], header[23]]);
    let magic = u16::from_le_bytes([header[24], header[25]]);
    if &header[..4] != b"PE\0\0"
        || machine != 0x8664
        || magic != 0x20b
        || characteristics & 0x0002 == 0
        || characteristics & 0x2000 != 0
    {
        return Err(ResolveError::InvalidPe);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "multicore-host-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn pe(path: &Path, machine: u16, dll: bool) {
        let mut bytes = vec![0u8; 0x100];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        bytes[0x80..0x84].copy_from_slice(b"PE\0\0");
        bytes[0x84..0x86].copy_from_slice(&machine.to_le_bytes());
        let flags = 0x0002u16 | if dll { 0x2000 } else { 0 };
        bytes[0x96..0x98].copy_from_slice(&flags.to_le_bytes());
        bytes[0x98..0x9a].copy_from_slice(&0x20bu16.to_le_bytes());
        fs::write(path, bytes).unwrap();
    }
    fn fixture() -> (TestDir, PathBuf, PathBuf) {
        let root = TestDir::new();
        let package = root.path().join("Program Files").join("MultiCore");
        fs::create_dir_all(package.join("runtime")).unwrap();
        fs::create_dir_all(package.join("cores")).unwrap();
        let host = package.join("runtime/multicore-core-host.exe");
        fs::write(&host, b"host").unwrap();
        pe(&package.join("cores/xray.exe"), 0x8664, false);
        pe(&package.join("cores/mihomo.exe"), 0x8664, false);
        let data = root.path().join("Local/MultiCore");
        fs::create_dir_all(data.join("runtime/runtime-00000000000000000007")).unwrap();
        fs::write(
            data.join("runtime/runtime-00000000000000000007/xray.json"),
            b"{}",
        )
        .unwrap();
        fs::write(
            data.join("runtime/runtime-00000000000000000007/mihomo.yaml"),
            b"tun: {}",
        )
        .unwrap();
        (root, data, host)
    }
    #[test]
    fn accepts_exact_layout() {
        let (_r, d, h) = fixture();
        assert!(resolve_from(&d, &h, 7).is_ok());
    }
    #[test]
    fn rejects_zero_missing_and_wrong_pe() {
        let (_r, d, h) = fixture();
        assert!(matches!(
            resolve_from(&d, &h, 0),
            Err(ResolveError::InvalidGeneration)
        ));
        assert!(matches!(
            resolve_from(&d, &h, 8),
            Err(ResolveError::Missing)
        ));
        pe(
            &h.parent().unwrap().parent().unwrap().join("cores/xray.exe"),
            0x014c,
            false,
        );
        assert!(matches!(
            resolve_from(&d, &h, 7),
            Err(ResolveError::InvalidPe)
        ));
    }
    #[test]
    fn rejects_dll_core_and_wrong_host_layout() {
        let (_r, d, h) = fixture();
        pe(
            &h.parent().unwrap().parent().unwrap().join("cores/xray.exe"),
            0x8664,
            true,
        );
        assert!(matches!(
            resolve_from(&d, &h, 7),
            Err(ResolveError::InvalidPe)
        ));
        assert!(matches!(
            resolve_from(&d, &h.with_file_name("other.exe"), 7),
            Err(ResolveError::Missing | ResolveError::Layout)
        ));
    }
    #[test]
    fn rejects_escape_and_reparse_components() {
        let (root, data, host) = fixture();
        assert!(matches!(
            canonical_contained(&data, &host),
            Err(ResolveError::Escape)
        ));
        let target = root.path().join("real-directory");
        let link = root.path().join("linked-directory");
        fs::create_dir(&target).unwrap();
        match std::os::windows::fs::symlink_dir(&target, &link) {
            Ok(()) => assert!(matches!(
                open_guard(&link, true),
                Err(ResolveError::Reparse)
            )),
            Err(error)
                if error.kind() == std::io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(1314) => {}
            Err(error) => panic!("could not create reparse test fixture: {error}"),
        }
    }

    fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        PathBuf::from(name)
    }

    fn replace_with_symlink(path: &Path, directory: bool) -> Result<(), std::io::Error> {
        let real = append_suffix(path, ".real");
        fs::rename(path, &real)?;
        let result = if directory {
            std::os::windows::fs::symlink_dir(&real, path)
        } else {
            std::os::windows::fs::symlink_file(&real, path)
        };
        if result.is_err() {
            let _ = fs::rename(&real, path);
        }
        result
    }

    #[test]
    fn full_resolver_rejects_reparse_at_every_trusted_layer() {
        for target in ["data", "runtime", "generation", "cores", "binary", "config"] {
            let (_root, data, host) = fixture();
            let package = host.parent().unwrap().parent().unwrap();
            let (path, directory) = match target {
                "data" => (data.clone(), true),
                "runtime" => (data.join("runtime"), true),
                "generation" => (data.join("runtime/runtime-00000000000000000007"), true),
                "cores" => (package.join("cores"), true),
                "binary" => (package.join("cores/xray.exe"), false),
                "config" => (
                    data.join("runtime/runtime-00000000000000000007/xray.json"),
                    false,
                ),
                _ => unreachable!(),
            };
            match replace_with_symlink(&path, directory) {
                Ok(()) => assert!(
                    matches!(resolve_from(&data, &host, 7), Err(ResolveError::Reparse)),
                    "accepted reparse at {target}"
                ),
                Err(error)
                    if error.kind() == std::io::ErrorKind::PermissionDenied
                        || error.raw_os_error() == Some(1314) => {}
                Err(error) => panic!("could not create {target} reparse fixture: {error}"),
            }
        }
    }
}
