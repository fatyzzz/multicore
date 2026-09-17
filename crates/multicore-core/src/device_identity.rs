use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::snapshot::sync_directory;
#[cfg(not(windows))]
use crate::snapshot::{secure_directory, secure_file};

const IDENTITY_DIRECTORY: &str = "identity";
const IDENTITY_FILE: &str = "device_id";
const IDENTITY_LOCK_FILE: &str = ".device_id.lock";
const MAX_DESCRIPTOR_BYTES: usize = 128;

#[derive(Clone, PartialEq, Eq)]
pub struct DeviceIdentity {
    hwid: String,
    os_version: String,
    device_model: String,
}

impl std::fmt::Debug for DeviceIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("DeviceIdentity { redacted }")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("device identity is unavailable")]
pub struct DeviceIdentityError;

#[cfg(test)]
struct PlatformInputs {
    hardware: HardwareInputs,
    descriptors: DescriptorInputs,
}

struct HardwareInputs {
    machine_guid: Option<String>,
    hostname: Option<String>,
    os: Option<String>,
    arch: Option<String>,
    user: Option<String>,
}

struct DescriptorInputs {
    os_version: Option<String>,
    device_model: Option<String>,
}

impl DeviceIdentity {
    pub fn load_or_create(data_directory: &Path) -> Result<Self, DeviceIdentityError> {
        load_or_create_with_sources(
            data_directory,
            platform_descriptors(),
            platform_hardware_inputs,
        )
    }

    pub fn hwid(&self) -> &str {
        &self.hwid
    }

    pub fn os_version(&self) -> &str {
        &self.os_version
    }

    pub fn device_model(&self) -> &str {
        &self.device_model
    }
}

#[cfg(test)]
fn load_or_create_with_inputs(
    data_directory: &Path,
    inputs: PlatformInputs,
) -> Result<DeviceIdentity, DeviceIdentityError> {
    load_or_create_with_sources(data_directory, inputs.descriptors, || inputs.hardware)
}

fn load_or_create_with_sources(
    data_directory: &Path,
    descriptors: DescriptorInputs,
    hardware_inputs: impl FnOnce() -> HardwareInputs,
) -> Result<DeviceIdentity, DeviceIdentityError> {
    let _data_anchors =
        prepare_and_anchor_directory_tree(data_directory).map_err(|_| DeviceIdentityError)?;
    secure_directory_handle(data_directory).map_err(|_| DeviceIdentityError)?;
    let directory = data_directory.join(IDENTITY_DIRECTORY);
    let _identity_anchors =
        prepare_and_anchor_directory_tree(&directory).map_err(|_| DeviceIdentityError)?;
    secure_directory_handle(&directory).map_err(|_| DeviceIdentityError)?;
    let lock = open_secure_file(&directory.join(IDENTITY_LOCK_FILE), false, true)
        .map_err(|_| DeviceIdentityError)?;
    fs2::FileExt::lock_exclusive(&lock).map_err(|_| DeviceIdentityError)?;
    let path = directory.join(IDENTITY_FILE);
    let hwid = match read_valid_hwid(&path).map_err(|_| DeviceIdentityError)? {
        Some(hwid) => hwid,
        None => {
            let hwid = match derive_platform_hwid(&hardware_inputs()) {
                Some(hwid) => hwid,
                None => random_hwid().map_err(|_| DeviceIdentityError)?,
            };
            persist_hwid(&directory, &path, &hwid).map_err(|_| DeviceIdentityError)?;
            hwid
        }
    };
    let os_version = descriptors
        .os_version
        .as_deref()
        .and_then(|value| bounded_descriptor(value, "Windows"))
        .unwrap_or_else(|| "Windows".to_owned());
    let device_model = descriptors
        .device_model
        .as_deref()
        .and_then(|value| bounded_descriptor(value, "Windows PC"))
        .unwrap_or_else(|| "Windows PC".to_owned());
    Ok(DeviceIdentity {
        hwid,
        os_version,
        device_model,
    })
}

pub fn derive_incy_hwid(
    machine_guid: &str,
    hostname: &str,
    os: &str,
    arch: &str,
    user: &str,
) -> String {
    let raw = format!("{machine_guid}|{hostname}|{os}|{arch}|{user}");
    let device_id_hex = lowercase_hex(&Sha256::digest(raw.as_bytes()));
    let final_hash = Sha256::digest(format!("incy_hwid_{device_id_hex}").as_bytes());
    format_uuid_hash(&final_hash)
}

pub fn is_valid_hwid(value: &str) -> bool {
    if value.len() != 36 {
        return false;
    }
    value.bytes().enumerate().all(|(index, byte)| {
        if matches!(index, 8 | 13 | 18 | 23) {
            byte == b'-'
        } else {
            byte.is_ascii_digit() || (b'A'..=b'F').contains(&byte)
        }
    })
}

fn lowercase_hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn format_uuid_hash(bytes: &[u8]) -> String {
    let hex = lowercase_hex(&bytes[..16]).to_ascii_uppercase();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn random_hwid() -> Result<String, getrandom::Error> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)?;
    Ok(format_uuid_hash(&bytes))
}

#[cfg(not(windows))]
fn prepare_identity_directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if is_safe_directory(&metadata) => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid identity directory",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir_all(path)?,
        Err(error) => return Err(error),
    }
    secure_directory(path)?;
    Ok(())
}

#[cfg(not(windows))]
fn read_valid_hwid(path: &Path) -> io::Result<Option<String>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !is_safe_file(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid identity file",
        ));
    }
    secure_file(path)?;
    if metadata.len() != 36 {
        return Ok(None);
    }
    let value = fs::read_to_string(path)?;
    Ok(is_valid_hwid(&value).then_some(value))
}

#[cfg(windows)]
fn read_valid_hwid(path: &Path) -> io::Result<Option<String>> {
    use std::io::Read as _;

    let mut file = match open_secure_file(path, false, false) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if file.metadata()?.len() != 36 {
        return Ok(None);
    }
    let mut value = String::new();
    file.read_to_string(&mut value)?;
    Ok(is_valid_hwid(&value).then_some(value))
}

fn persist_hwid(directory: &Path, target: &Path, hwid: &str) -> io::Result<()> {
    let temp = temporary_path(directory)?;
    let result = (|| {
        let mut file = open_secure_file(&temp, true, false)?;
        file.write_all(hwid.as_bytes())?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        atomic_replace(&temp, target)?;
        let target_file = open_secure_file(target, false, false)?;
        target_file.sync_all()?;
        sync_directory(directory)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn temporary_path(directory: &Path) -> io::Result<PathBuf> {
    loop {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).map_err(io::Error::other)?;
        let suffix = lowercase_hex(&random);
        let candidate = directory.join(format!(".{IDENTITY_FILE}.{suffix}.tmp"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
}

#[cfg(not(windows))]
fn open_secure_file(path: &Path, create_new: bool, create: bool) -> io::Result<fs::File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(create_new)
        .create(create)
        .open(path)?;
    secure_file(path)?;
    Ok(file)
}

#[cfg(not(windows))]
fn prepare_and_anchor_directory_tree(path: &Path) -> io::Result<Vec<fs::File>> {
    prepare_identity_directory(path)?;
    Ok(Vec::new())
}

#[cfg(not(windows))]
fn secure_directory_handle(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(windows)]
fn prepare_and_anchor_directory_tree(path: &Path) -> io::Result<Vec<fs::File>> {
    use std::path::Component;

    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut current = PathBuf::new();
    let mut anchors = Vec::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                current.push(component.as_os_str());
            }
            Component::CurDir => continue,
            Component::ParentDir => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "identity path contains a parent component",
                ));
            }
        }
        if matches!(component, Component::RootDir | Component::Normal(_)) {
            let directory = match open_directory_no_follow(&current, false) {
                Ok(directory) => directory,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    match fs::create_dir(&current) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(error) => return Err(error),
                    }
                    open_directory_no_follow(&current, false)?
                }
                Err(error) => return Err(error),
            };
            anchors.push(directory);
        }
    }
    if anchors.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "identity path has no directory components",
        ));
    }
    Ok(anchors)
}

#[cfg(windows)]
fn secure_directory_handle(path: &Path) -> io::Result<()> {
    let directory = open_directory_no_follow(path, true)?;
    apply_owner_only_acl_handle(&directory, true)
}

#[cfg(windows)]
fn open_directory_no_follow(path: &Path, writable_acl: bool) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL, WRITE_DAC,
    };

    let mut access = FILE_READ_ATTRIBUTES;
    if writable_acl {
        access |= READ_CONTROL | WRITE_DAC;
    }
    let file = OpenOptions::new()
        .read(true)
        .access_mode(access)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    validate_windows_handle(&file, true)?;
    Ok(file)
}

#[cfg(windows)]
fn open_secure_file(path: &Path, create_new: bool, create: bool) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, GENERIC_WRITE},
        Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL,
            WRITE_DAC,
        },
    };

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .access_mode(GENERIC_READ | GENERIC_WRITE | READ_CONTROL | WRITE_DAC)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .create_new(create_new)
        .create(create)
        .open(path)?;
    validate_windows_handle(&file, false)?;
    apply_owner_only_acl_handle(&file, false)?;
    Ok(file)
}

#[cfg(windows)]
fn validate_windows_handle(file: &fs::File, expect_directory: bool) -> io::Result<()> {
    use std::{ffi::c_void, os::windows::io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
        FileAttributeTagInfo, GetFileInformationByHandleEx,
    };

    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileAttributeTagInfo,
            (&mut info as *mut FILE_ATTRIBUTE_TAG_INFO).cast::<c_void>(),
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let is_directory = info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    if info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 || is_directory != expect_directory {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "identity path is reparse-backed or has the wrong type",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn apply_owner_only_acl_handle(file: &fs::File, inheritable: bool) -> io::Result<()> {
    use std::{ffi::c_void, os::windows::io::AsRawHandle, ptr};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, LocalFree},
        Security::{
            Authorization::{
                EXPLICIT_ACCESS_W, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, SET_ACCESS,
                SetEntriesInAclW, SetSecurityInfo, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
            },
            DACL_SECURITY_INFORMATION, GetTokenInformation, PROTECTED_DACL_SECURITY_INFORMATION,
            SUB_CONTAINERS_AND_OBJECTS_INHERIT, TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        Storage::FileSystem::FILE_ALL_ACCESS,
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    let mut token: HANDLE = ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let result = (|| {
        let mut bytes_needed = 0_u32;
        unsafe { GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut bytes_needed) };
        if bytes_needed == 0 {
            return Err(io::Error::last_os_error());
        }
        let word_size = std::mem::size_of::<usize>();
        let mut token_info = vec![0_usize; (bytes_needed as usize).div_ceil(word_size)];
        if unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                token_info.as_mut_ptr().cast::<c_void>(),
                bytes_needed,
                &mut bytes_needed,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let user = unsafe { &*token_info.as_ptr().cast::<TOKEN_USER>() };
        let trustee = TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: user.User.Sid.cast(),
        };
        let access = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_ALL_ACCESS,
            grfAccessMode: SET_ACCESS,
            grfInheritance: if inheritable {
                SUB_CONTAINERS_AND_OBJECTS_INHERIT
            } else {
                0
            },
            Trustee: trustee,
        };
        let mut acl = ptr::null_mut();
        let status = unsafe { SetEntriesInAclW(1, &access, ptr::null(), &mut acl) };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let status = unsafe {
            SetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                acl,
                ptr::null(),
            )
        };
        unsafe { LocalFree(acl.cast()) };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(())
    })();
    unsafe { CloseHandle(token) };
    result
}

#[cfg(unix)]
fn atomic_replace(source: &Path, target: &Path) -> io::Result<()> {
    fs::rename(source, target)
}

#[cfg(windows)]
fn atomic_replace(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
fn atomic_replace(_source: &Path, _target: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic replacement unsupported",
    ))
}

#[cfg(unix)]
fn is_safe_directory(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_dir() && !metadata.file_type().is_symlink()
}

#[cfg(unix)]
fn is_safe_file(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_file() && !metadata.file_type().is_symlink()
}

#[cfg(not(any(unix, windows)))]
fn is_safe_directory(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(not(any(unix, windows)))]
fn is_safe_file(_metadata: &fs::Metadata) -> bool {
    false
}

fn derive_platform_hwid(inputs: &HardwareInputs) -> Option<String> {
    let machine_guid = inputs
        .machine_guid
        .as_deref()
        .filter(|value| !value.is_empty())?;
    let hostname = inputs
        .hostname
        .as_deref()
        .filter(|value| !value.is_empty())?;
    let os = inputs.os.as_deref().filter(|value| !value.is_empty())?;
    let arch = inputs.arch.as_deref().filter(|value| !value.is_empty())?;
    let user = inputs.user.as_deref().filter(|value| !value.is_empty())?;
    Some(derive_incy_hwid(machine_guid, hostname, os, arch, user))
}

#[cfg(windows)]
fn platform_hardware_inputs() -> HardwareInputs {
    HardwareInputs {
        machine_guid: registry_string(r"SOFTWARE\Microsoft\Cryptography", "MachineGuid"),
        hostname: computer_name(),
        os: Some(std::env::consts::OS.to_owned()),
        arch: Some(std::env::consts::ARCH.to_owned()),
        user: user_name(),
    }
}

#[cfg(windows)]
fn platform_descriptors() -> DescriptorInputs {
    let os_version = registry_string(
        r"SOFTWARE\Microsoft\Windows NT\CurrentVersion",
        "ProductName",
    )
    .map(|product| {
        let release = registry_string(
            r"SOFTWARE\Microsoft\Windows NT\CurrentVersion",
            "DisplayVersion",
        );
        format!("{product} {}", release.unwrap_or_default())
    });
    DescriptorInputs {
        os_version,
        device_model: registry_string(r"HARDWARE\DESCRIPTION\System\BIOS", "SystemProductName"),
    }
}

#[cfg(not(windows))]
fn platform_hardware_inputs() -> HardwareInputs {
    HardwareInputs {
        machine_guid: None,
        hostname: None,
        os: Some(std::env::consts::OS.to_owned()),
        arch: Some(std::env::consts::ARCH.to_owned()),
        user: None,
    }
}

#[cfg(not(windows))]
fn platform_descriptors() -> DescriptorInputs {
    DescriptorInputs {
        os_version: None,
        device_model: None,
    }
}

fn bounded_descriptor(value: &str, fallback: &str) -> Option<String> {
    let sanitized = value
        .chars()
        .map(|character| {
            if character.is_ascii_graphic() {
                character
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if sanitized.is_empty() {
        return Some(fallback.to_owned());
    }
    Some(sanitized.chars().take(MAX_DESCRIPTOR_BYTES).collect())
}

#[cfg(windows)]
fn registry_string(subkey: &str, value: &str) -> Option<String> {
    use std::{ffi::c_void, ptr};
    use windows_sys::Win32::{
        Foundation::ERROR_SUCCESS,
        System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW},
    };
    let subkey: Vec<u16> = subkey.encode_utf16().chain(Some(0)).collect();
    let value: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
    let mut bytes = 0_u32;
    if unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut bytes,
        )
    } != ERROR_SUCCESS
        || !(2..=4096).contains(&bytes)
    {
        return None;
    }
    let mut buffer = vec![0_u16; (bytes as usize).div_ceil(2)];
    if unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            buffer.as_mut_ptr().cast::<c_void>(),
            &mut bytes,
        )
    } != ERROR_SUCCESS
    {
        return None;
    }
    while buffer.last() == Some(&0) {
        buffer.pop();
    }
    String::from_utf16(&buffer)
        .ok()
        .filter(|value| !value.is_empty())
}

#[cfg(windows)]
fn computer_name() -> Option<String> {
    use windows_sys::Win32::System::WindowsProgramming::GetComputerNameW;
    let mut buffer = [0_u16; 256];
    let mut length = buffer.len() as u32;
    if unsafe { GetComputerNameW(buffer.as_mut_ptr(), &mut length) } == 0 {
        None
    } else {
        String::from_utf16(&buffer[..length as usize]).ok()
    }
}

#[cfg(windows)]
fn user_name() -> Option<String> {
    use windows_sys::Win32::System::WindowsProgramming::GetUserNameW;
    let mut buffer = [0_u16; 256];
    let mut length = buffer.len() as u32;
    if unsafe { GetUserNameW(buffer.as_mut_ptr(), &mut length) } == 0 || length <= 1 {
        None
    } else {
        String::from_utf16(&buffer[..length as usize - 1]).ok()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{Read, Write},
        net::TcpListener,
        path::PathBuf,
        sync::{Arc, Barrier},
        thread,
    };

    use crate::{HttpClient, ReqwestHttpClient, UA_NATIVE};

    use super::{
        DescriptorInputs, HardwareInputs, PlatformInputs, is_valid_hwid, load_or_create_with_inputs,
    };

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let mut random = [0_u8; 8];
            getrandom::fill(&mut random).unwrap();
            let path = std::env::temp_dir().join(format!(
                "multicore-device-inputs-{label}-{}-{}",
                std::process::id(),
                u64::from_ne_bytes(random)
            ));
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn complete_inputs() -> PlatformInputs {
        PlatformInputs {
            hardware: HardwareInputs {
                machine_guid: Some("MACHINE-GUID-SECRET".to_owned()),
                hostname: Some("HOST-SECRET".to_owned()),
                os: Some("OS-SECRET".to_owned()),
                arch: Some("ARCH-SECRET".to_owned()),
                user: Some("USER-SECRET".to_owned()),
            },
            descriptors: DescriptorInputs {
                os_version: Some(format!("{}\0🚀SECRET-DESCRIPTOR", "V".repeat(140))),
                device_model: Some("\0🚀\u{202e}".to_owned()),
            },
        }
    }

    fn missing_inputs() -> PlatformInputs {
        let mut inputs = complete_inputs();
        inputs.hardware.user = None;
        inputs
    }

    #[tokio::test]
    async fn injected_inputs_derive_persist_and_send_only_safe_final_values() {
        let root = TestDirectory::new("complete");
        let raw_inputs = [
            "MACHINE-GUID-SECRET",
            "HOST-SECRET",
            "OS-SECRET",
            "ARCH-SECRET",
            "USER-SECRET",
            "SECRET-DESCRIPTOR",
        ];
        let identity = load_or_create_with_inputs(&root.0, complete_inputs()).unwrap();
        assert_eq!(identity.hwid(), "B64CD3CC-5D61-72F2-AE98-7277AF087AAD");
        assert_eq!(identity.os_version(), "V".repeat(128));
        assert_eq!(identity.device_model(), "Windows PC");
        assert!(identity.os_version().is_ascii());
        assert!(identity.device_model().is_ascii());
        assert!(identity.os_version().len() <= 128);
        assert!(identity.device_model().len() <= 128);

        let persisted = fs::read(root.0.join("identity/device_id")).unwrap();
        assert_eq!(persisted, identity.hwid().as_bytes());
        for raw in raw_inputs {
            assert!(
                !persisted
                    .windows(raw.len())
                    .any(|window| window == raw.as_bytes())
            );
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0_u8; 1024];
            loop {
                let read = stream.read(&mut chunk).unwrap();
                bytes.extend_from_slice(&chunk[..read]);
                if read == 0 || bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
            String::from_utf8(bytes).unwrap()
        });
        ReqwestHttpClient::new(identity)
            .unwrap()
            .get(&format!("http://{address}/subscription"), UA_NATIVE)
            .await
            .unwrap();
        let request = server.join().unwrap();
        assert!(request.contains("x-hwid: B64CD3CC-5D61-72F2-AE98-7277AF087AAD"));
        assert!(request.contains(&format!("x-ver-os: {}", "V".repeat(128))));
        assert!(request.contains("x-device-model: Windows PC"));
        for raw in raw_inputs {
            assert!(!request.contains(raw));
        }
    }

    #[test]
    fn any_missing_hardware_input_uses_a_random_persisted_identifier() {
        let first_root = TestDirectory::new("missing-one");
        let second_root = TestDirectory::new("missing-two");
        let mut first_inputs = complete_inputs();
        first_inputs.hardware.user = None;
        let mut second_inputs = complete_inputs();
        second_inputs.hardware.user = None;
        let first = load_or_create_with_inputs(&first_root.0, first_inputs).unwrap();
        let second = load_or_create_with_inputs(&second_root.0, second_inputs).unwrap();
        assert!(is_valid_hwid(first.hwid()));
        assert!(is_valid_hwid(second.hwid()));
        assert_ne!(first.hwid(), "B64CD3CC-5D61-72F2-AE98-7277AF087AAD");
        assert_ne!(first.hwid(), second.hwid());
        assert_eq!(
            fs::read(first_root.0.join("identity/device_id")).unwrap(),
            first.hwid().as_bytes()
        );
    }

    #[test]
    fn simultaneous_first_use_converges_on_the_single_persisted_winner() {
        const CONTENDERS: usize = 24;
        let root = TestDirectory::new("concurrent");
        let barrier = Arc::new(Barrier::new(CONTENDERS));
        let handles: Vec<_> = (0..CONTENDERS)
            .map(|_| {
                let root = root.0.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    load_or_create_with_inputs(&root, missing_inputs())
                        .unwrap()
                        .hwid
                })
            })
            .collect();
        let returned: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        let persisted = fs::read_to_string(root.0.join("identity/device_id")).unwrap();
        assert!(returned.iter().all(|hwid| hwid == &persisted));
        let entries: Vec<_> = fs::read_dir(root.0.join("identity"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|name| {
            let name = name.to_string_lossy();
            name == "device_id" || name == ".device_id.lock"
        }));
        let lock = root.0.join("identity/.device_id.lock");
        if lock.exists() {
            assert_eq!(fs::metadata(lock).unwrap().len(), 0);
        }
    }
}
