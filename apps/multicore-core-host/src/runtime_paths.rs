use multicore_core::RuntimePaths;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    os::windows::{
        ffi::OsStringExt,
        fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};
use windows_sys::Win32::{
    Storage::FileSystem::{FILE_ATTRIBUTE_REPARSE_POINT, FILE_SHARE_READ},
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
    _guards: Vec<File>,
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
    let host = fs::canonicalize(host_executable).map_err(|_| ResolveError::Missing)?;
    let runtime_dir = host.parent().ok_or(ResolveError::Layout)?;
    if runtime_dir.file_name().and_then(|v| v.to_str()) != Some("runtime")
        || host.file_name().and_then(|v| v.to_str()) != Some("multicore-core-host.exe")
    {
        return Err(ResolveError::Layout);
    }
    let package = runtime_dir.parent().ok_or(ResolveError::Layout)?;
    let package = fs::canonicalize(package).map_err(|_| ResolveError::Missing)?;
    validate_chain(&package, &host, false)?;
    let cores = package.join("cores");
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
    let canonical_data = fs::canonicalize(data_root).map_err(|_| ResolveError::Missing)?;
    let canonical_generation =
        fs::canonicalize(&generation_dir).map_err(|_| ResolveError::Missing)?;
    validate_chain(&canonical_data, &canonical_generation, true)?;
    let mut guards = Vec::new();
    for binary in [&xray, &mihomo] {
        let canonical = fs::canonicalize(binary).map_err(|_| ResolveError::Missing)?;
        validate_chain(&package, &canonical, false)?;
        let mut guard = guarded_file(&canonical)?;
        validate_amd64_pe(&mut guard)?;
        guards.push(guard);
    }
    for config in [&xray_config, &mihomo_config] {
        let canonical = fs::canonicalize(config).map_err(|_| ResolveError::Missing)?;
        validate_chain(&canonical_generation, &canonical, false)?;
        guards.push(guarded_file(&canonical)?);
    }
    Ok(ResolvedRuntime {
        paths: RuntimePaths {
            xray_binary: fs::canonicalize(xray).map_err(|_| ResolveError::Missing)?,
            mihomo_binary: fs::canonicalize(mihomo).map_err(|_| ResolveError::Missing)?,
            xray_config: fs::canonicalize(xray_config).map_err(|_| ResolveError::Missing)?,
            mihomo_config: fs::canonicalize(mihomo_config).map_err(|_| ResolveError::Missing)?,
        },
        _guards: guards,
    })
}

fn validate_chain(root: &Path, target: &Path, target_directory: bool) -> Result<(), ResolveError> {
    let root = fs::canonicalize(root).map_err(|_| ResolveError::Missing)?;
    let target = fs::canonicalize(target).map_err(|_| ResolveError::Missing)?;
    if !target.starts_with(&root) {
        return Err(ResolveError::Escape);
    }
    let mut cursor = root.clone();
    check_not_reparse(&cursor)?;
    for part in target
        .strip_prefix(&root)
        .map_err(|_| ResolveError::Escape)?
        .components()
    {
        cursor.push(part);
        check_not_reparse(&cursor)?;
    }
    let metadata = fs::metadata(&target).map_err(|_| ResolveError::Missing)?;
    if target_directory && !metadata.is_dir() || !target_directory && !metadata.is_file() {
        return Err(ResolveError::Layout);
    }
    Ok(())
}

fn check_not_reparse(path: &Path) -> Result<(), ResolveError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| ResolveError::Missing)?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(ResolveError::Reparse);
    }
    Ok(())
}

fn guarded_file(path: &Path) -> Result<File, ResolveError> {
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(path)
        .map_err(|_| ResolveError::Io)
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
            validate_chain(&data, &host, false),
            Err(ResolveError::Escape)
        ));
        let target = root.path().join("real-directory");
        let link = root.path().join("linked-directory");
        fs::create_dir(&target).unwrap();
        match std::os::windows::fs::symlink_dir(&target, &link) {
            Ok(()) => assert!(matches!(
                check_not_reparse(&link),
                Err(ResolveError::Reparse)
            )),
            Err(error)
                if error.kind() == std::io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(1314) => {}
            Err(error) => panic!("could not create reparse test fixture: {error}"),
        }
    }
}
