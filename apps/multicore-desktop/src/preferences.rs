use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const PREFERENCES_SCHEMA_VERSION: u32 = 1;

const PREFERENCES_DIRECTORY: &str = "MultiCore";
const PREFERENCES_FILE: &str = "preferences.json";
const PREFERENCES_TEMP_FILE: &str = "preferences.json.tmp";
const PREFERENCES_CORRUPT_FILE: &str = "preferences.corrupt.json";
const MAX_PREFERENCES_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowBounds {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VisiblePage {
    #[default]
    Home,
    Status,
    Settings,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppPreferences {
    pub schema_version: u32,
    pub restored_bounds: Option<WindowBounds>,
    pub maximized: bool,
    pub visible_page: VisiblePage,
    pub active_profile_hint: Option<String>,
    pub last_group_by_profile: BTreeMap<String, String>,
    pub selections_by_profile: BTreeMap<String, BTreeMap<String, String>>,
    pub ambient_background: bool,
}

impl Default for AppPreferences {
    fn default() -> Self {
        Self {
            schema_version: PREFERENCES_SCHEMA_VERSION,
            restored_bounds: None,
            maximized: false,
            visible_page: VisiblePage::Home,
            active_profile_hint: None,
            last_group_by_profile: BTreeMap::new(),
            selections_by_profile: BTreeMap::new(),
            ambient_background: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreferenceStore {
    path: PathBuf,
}

impl PreferenceStore {
    pub fn from_local_app_data() -> io::Result<Self> {
        preference_path_from_local_app_data(std::env::var_os("LOCALAPPDATA")).map(Self::at)
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> io::Result<AppPreferences> {
        reject_parent_components(&self.path)?;
        let Some(parent) = self.path.parent() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "preferences path has no parent directory",
            ));
        };
        let _anchors = anchor_existing_directory_tree(parent)?;
        match fs::symlink_metadata(&self.path) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(AppPreferences::default());
            }
            Err(error) => return Err(error),
        }
        secure_directory(parent)?;
        let mut file = match open_secure_file(&self.path, false) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(AppPreferences::default());
            }
            Err(error) => return Err(error),
        };
        if file.metadata()?.len() > MAX_PREFERENCES_BYTES {
            drop(file);
            self.quarantine(parent)?;
            return Ok(AppPreferences::default());
        }
        let mut raw = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_PREFERENCES_BYTES + 1)
            .read_to_end(&mut raw)?;
        drop(file);
        if raw.len() as u64 > MAX_PREFERENCES_BYTES {
            self.quarantine(parent)?;
            return Ok(AppPreferences::default());
        }
        match serde_json::from_slice::<AppPreferences>(&raw) {
            Ok(value) if value.schema_version == PREFERENCES_SCHEMA_VERSION => Ok(value),
            Ok(_) | Err(_) => {
                self.quarantine(parent)?;
                Ok(AppPreferences::default())
            }
        }
    }

    pub fn save(&self, value: &AppPreferences) -> io::Result<()> {
        reject_parent_components(&self.path)?;
        if value.schema_version != PREFERENCES_SCHEMA_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported preferences schema",
            ));
        }
        let parent = self.path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "preferences path has no parent directory",
            )
        })?;
        let _anchors = prepare_and_anchor_directory_tree(parent)?;
        secure_directory(parent)?;
        let temp = parent.join(PREFERENCES_TEMP_FILE);
        reject_existing_reparse(&self.path, false)?;
        reject_existing_reparse(&temp, false)?;
        let encoded = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
        if encoded.len() as u64 + 1 > MAX_PREFERENCES_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "preferences exceed the size limit",
            ));
        }
        let result = (|| {
            let mut file = create_secure_file(&temp)?;
            file.write_all(&encoded)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            drop(file);
            atomic_replace(&temp, &self.path)?;
            let published = open_secure_file(&self.path, false)?;
            published.sync_all()?;
            sync_directory(parent)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    fn quarantine(&self, parent: &Path) -> io::Result<()> {
        let quarantine = parent.join(PREFERENCES_CORRUPT_FILE);
        reject_existing_reparse(&quarantine, false)?;
        atomic_replace(&self.path, &quarantine)?;
        sync_directory(parent)
    }
}

fn preference_path_from_local_app_data(root: Option<OsString>) -> io::Result<PathBuf> {
    let root = root
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "LOCALAPPDATA is unavailable"))?;
    if root.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "LOCALAPPDATA is empty",
        ));
    }
    if !root.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "LOCALAPPDATA is not absolute",
        ));
    }
    Ok(root.join(PREFERENCES_DIRECTORY).join(PREFERENCES_FILE))
}

fn reject_parent_components(path: &Path) -> io::Result<()> {
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "preferences path contains a parent component",
        ))
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn prepare_and_anchor_directory_tree(path: &Path) -> io::Result<Vec<fs::File>> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "preferences directory is not a plain directory",
        ));
    }
    Ok(Vec::new())
}

#[cfg(not(windows))]
fn anchor_existing_directory_tree(path: &Path) -> io::Result<Vec<fs::File>> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        current.push(component.as_os_str());
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(error),
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "preferences directory is not a plain directory",
            ));
        }
    }
    Ok(Vec::new())
}

#[cfg(windows)]
fn prepare_and_anchor_directory_tree(path: &Path) -> io::Result<Vec<fs::File>> {
    anchor_directory_tree(path, true)
}

#[cfg(windows)]
fn anchor_existing_directory_tree(path: &Path) -> io::Result<Vec<fs::File>> {
    anchor_directory_tree(path, false)
}

#[cfg(windows)]
fn anchor_directory_tree(path: &Path, create: bool) -> io::Result<Vec<fs::File>> {
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
                    "preferences path contains a parent component",
                ));
            }
        }
        if matches!(component, Component::RootDir | Component::Normal(_)) {
            let directory = match open_directory_no_follow(&current, false) {
                Ok(directory) => directory,
                Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                    match fs::create_dir(&current) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(error) => return Err(error),
                    }
                    open_directory_no_follow(&current, false)?
                }
                Err(error) if !create && error.kind() == io::ErrorKind::NotFound => break,
                Err(error) => return Err(error),
            };
            anchors.push(directory);
        }
    }
    if anchors.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "preferences path has no directory components",
        ));
    }
    Ok(anchors)
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

#[cfg(not(windows))]
fn create_secure_file(path: &Path) -> io::Result<fs::File> {
    let file = OpenOptions::new().write(true).create_new(true).open(path)?;
    secure_file(path)?;
    Ok(file)
}

#[cfg(not(windows))]
fn open_secure_file(path: &Path, _create: bool) -> io::Result<fs::File> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "preferences path is not a plain file",
        ));
    }
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    secure_file(path)?;
    Ok(file)
}

#[cfg(windows)]
fn create_secure_file(path: &Path) -> io::Result<fs::File> {
    open_secure_file(path, true)
}

#[cfg(windows)]
fn open_secure_file(path: &Path, create_new: bool) -> io::Result<fs::File> {
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
            "preferences path is reparse-backed or has the wrong type",
        ));
    }
    Ok(())
}

fn reject_existing_reparse(path: &Path, expect_directory: bool) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink()
                || (expect_directory && !metadata.is_dir())
                || (!expect_directory && !metadata.is_file())
            {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "preferences path has an unsafe type",
                ))
            } else {
                #[cfg(windows)]
                {
                    let handle = if expect_directory {
                        open_directory_no_follow(path, false)?
                    } else {
                        open_secure_file(path, false)?
                    };
                    validate_windows_handle(&handle, expect_directory)?;
                }
                Ok(())
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn secure_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(unix)]
fn secure_file(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(windows)]
fn secure_directory(path: &Path) -> io::Result<()> {
    let directory = open_directory_no_follow(path, true)?;
    apply_owner_only_acl_handle(&directory, true)
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

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use super::{
        AppPreferences, PREFERENCES_SCHEMA_VERSION, PreferenceStore, VisiblePage, WindowBounds,
        preference_path_from_local_app_data,
    };

    #[test]
    fn local_app_data_root_must_be_nonempty_and_absolute() {
        assert!(preference_path_from_local_app_data(None).is_err());
        assert!(preference_path_from_local_app_data(Some("".into())).is_err());
        assert!(preference_path_from_local_app_data(Some("relative-root".into())).is_err());
    }

    #[test]
    fn absolute_local_app_data_root_builds_the_multicore_preferences_path() {
        let root = tempfile::tempdir().unwrap();

        assert_eq!(
            preference_path_from_local_app_data(Some(root.path().as_os_str().to_owned())).unwrap(),
            root.path().join("MultiCore").join("preferences.json")
        );
    }

    #[test]
    fn round_trip_keeps_only_durable_desktop_state() {
        let root = tempfile::tempdir().unwrap();
        let store = PreferenceStore::at(root.path().join("preferences.json"));
        let expected = AppPreferences {
            schema_version: PREFERENCES_SCHEMA_VERSION,
            restored_bounds: Some(WindowBounds {
                x: 40,
                y: 60,
                width: 920,
                height: 700,
            }),
            maximized: true,
            visible_page: VisiblePage::Status,
            active_profile_hint: Some("0f15b0f1-2a4b-4e2f-91cc-a82e5fcb5140".into()),
            last_group_by_profile: BTreeMap::from([("profile-a".into(), "group-b".into())]),
            selections_by_profile: BTreeMap::from([(
                "profile-a".into(),
                BTreeMap::from([("group-b".into(), "node-c".into())]),
            )]),
            ambient_background: true,
        };

        store.save(&expected).unwrap();

        assert_eq!(store.load().unwrap(), expected);
        let raw = fs::read_to_string(store.path()).unwrap();
        assert!(!raw.contains("connected"));
        assert!(!raw.contains("https://"));
    }

    #[test]
    fn missing_file_returns_desktop_defaults() {
        let root = tempfile::tempdir().unwrap();
        let store = PreferenceStore::at(root.path().join("preferences.json"));

        assert_eq!(store.load().unwrap(), AppPreferences::default());
        assert_eq!(
            AppPreferences::default(),
            AppPreferences {
                schema_version: 1,
                restored_bounds: None,
                maximized: false,
                visible_page: VisiblePage::Home,
                active_profile_hint: None,
                last_group_by_profile: BTreeMap::new(),
                selections_by_profile: BTreeMap::new(),
                ambient_background: true,
            }
        );
    }

    #[test]
    fn missing_preferences_directory_returns_desktop_defaults() {
        let root = tempfile::tempdir().unwrap();
        let store = PreferenceStore::at(root.path().join("MultiCore/preferences.json"));

        assert_eq!(store.load().unwrap(), AppPreferences::default());
        assert!(!store.path().exists());
    }

    #[test]
    fn unknown_schema_is_quarantined_and_defaults_are_used() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("preferences.json");
        fs::write(&path, br#"{"schema_version":2}"#).unwrap();
        let store = PreferenceStore::at(path);

        assert_eq!(store.load().unwrap(), AppPreferences::default());
        assert!(!store.path().exists());
        assert!(root.path().join("preferences.corrupt.json").is_file());
    }

    #[test]
    fn save_atomically_replaces_existing_value_without_temp_residue() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("preferences.json");
        fs::write(&path, b"old-value").unwrap();
        let store = PreferenceStore::at(path);
        let value = AppPreferences {
            visible_page: VisiblePage::Settings,
            ..AppPreferences::default()
        };

        store.save(&value).unwrap();

        assert_eq!(store.load().unwrap(), value);
        assert!(!root.path().join("preferences.json.tmp").exists());
    }

    #[test]
    fn corrupt_preferences_are_quarantined_without_touching_siblings() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("preferences.json");
        fs::write(&path, b"{broken").unwrap();
        let sibling = root.path().join("device-identity");
        fs::write(&sibling, b"sentinel").unwrap();
        let store = PreferenceStore::at(path);

        assert_eq!(store.load().unwrap(), AppPreferences::default());
        assert!(root.path().join("preferences.corrupt.json").is_file());
        assert_eq!(fs::read(sibling).unwrap(), b"sentinel");
    }
}
