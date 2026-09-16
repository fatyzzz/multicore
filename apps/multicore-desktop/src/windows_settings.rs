//! Windows lifecycle settings that belong to the signed-in user.
//!
//! Command construction and classification are platform-neutral on purpose. Only
//! the small registry adapter at the bottom of this file is Windows-specific.

use std::io;
use std::path::{Path, PathBuf};

pub const RUN_VALUE_NAME: &str = "MultiCore";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchAtSignInState {
    Disabled,
    Enabled,
    Stale { stored_command: String },
}

pub fn build_launch_command(executable: &Path) -> io::Result<String> {
    if executable.as_os_str().is_empty()
        || !executable.is_absolute()
        || executable.file_name().is_none()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "launch executable must be a non-root absolute path",
        ));
    }

    let executable = executable.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "launch executable path must be valid Unicode",
        )
    })?;

    if executable.contains(['\0', '\r', '\n', '"']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "launch executable path contains unsafe command characters",
        ));
    }

    Ok(format!("\"{executable}\" --background"))
}

pub fn classify_launch_command(
    stored_command: Option<&str>,
    current_executable: &Path,
) -> io::Result<LaunchAtSignInState> {
    let expected_command = build_launch_command(current_executable)?;

    let Some(stored_command) = stored_command else {
        return Ok(LaunchAtSignInState::Disabled);
    };

    let Some(stored_executable) = parse_launch_command(stored_command) else {
        return Ok(stale(stored_command));
    };

    if stored_command == expected_command
        && paths_resolve_to_same_file(&stored_executable, current_executable)
    {
        Ok(LaunchAtSignInState::Enabled)
    } else {
        Ok(stale(stored_command))
    }
}

fn parse_launch_command(command: &str) -> Option<PathBuf> {
    let executable = command.strip_suffix(" --background")?;
    let inner = executable.strip_prefix('"')?.strip_suffix('"')?;
    if inner.is_empty() || inner.contains(['\0', '\r', '\n', '"']) {
        return None;
    }

    let path = PathBuf::from(inner);
    (path.is_absolute() && path.file_name().is_some()).then_some(path)
}

fn paths_resolve_to_same_file(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }

    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn stale(stored_command: &str) -> LaunchAtSignInState {
    LaunchAtSignInState::Stale {
        stored_command: stored_command.to_owned(),
    }
}

#[cfg(windows)]
pub fn query_launch_at_sign_in() -> io::Result<LaunchAtSignInState> {
    let current_executable = std::env::current_exe()?;
    classify_launch_command(registry::read_run_value()?.as_deref(), &current_executable)
}

#[cfg(not(windows))]
pub fn query_launch_at_sign_in() -> io::Result<LaunchAtSignInState> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "launch at sign-in is only supported on Windows",
    ))
}

#[cfg(windows)]
pub fn set_launch_at_sign_in(enabled: bool) -> io::Result<LaunchAtSignInState> {
    let current_executable = std::env::current_exe()?;
    apply_launch_at_sign_in_change(
        enabled,
        &current_executable,
        registry::write_run_value,
        registry::delete_run_value,
        registry::read_run_value,
    )
}

#[cfg(not(windows))]
pub fn set_launch_at_sign_in(_enabled: bool) -> io::Result<LaunchAtSignInState> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "launch at sign-in is only supported on Windows",
    ))
}

fn apply_launch_at_sign_in_change<Write, Delete, Read>(
    enabled: bool,
    current_executable: &Path,
    mut write: Write,
    mut delete: Delete,
    mut read: Read,
) -> io::Result<LaunchAtSignInState>
where
    Write: FnMut(&str) -> io::Result<()>,
    Delete: FnMut() -> io::Result<()>,
    Read: FnMut() -> io::Result<Option<String>>,
{
    if enabled {
        write(&build_launch_command(current_executable)?)?;
    } else {
        delete()?;
    }

    let observed = classify_launch_command(read()?.as_deref(), current_executable)?;
    let verified = matches!(
        (enabled, &observed),
        (true, LaunchAtSignInState::Enabled) | (false, LaunchAtSignInState::Disabled)
    );
    if verified {
        Ok(observed)
    } else {
        Err(io::Error::other(
            "launch-at-sign-in registry change could not be verified",
        ))
    }
}

#[cfg(windows)]
mod registry {
    use super::RUN_VALUE_NAME;
    use std::ffi::c_void;
    use std::io;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
        RegCloseKey, RegCreateKeyExW, RegDeleteKeyValueW, RegGetValueW, RegSetValueExW,
    };

    const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
    const MAX_VALUE_BYTES: u32 = 64 * 1024;

    pub(super) fn read_run_value() -> io::Result<Option<String>> {
        let subkey = wide(RUN_KEY);
        let value_name = wide(RUN_VALUE_NAME);
        let mut byte_len = 0_u32;

        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                value_name.as_ptr(),
                RRF_RT_REG_SZ,
                null_mut(),
                null_mut(),
                &mut byte_len,
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        win32_result(status)?;
        if byte_len > MAX_VALUE_BYTES || !byte_len.is_multiple_of(2) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid launch-at-sign-in registry value size",
            ));
        }

        let mut buffer = vec![0_u16; (byte_len as usize / 2).max(1)];
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                value_name.as_ptr(),
                RRF_RT_REG_SZ,
                null_mut(),
                buffer.as_mut_ptr().cast::<c_void>(),
                &mut byte_len,
            )
        };
        win32_result(status)?;
        buffer.truncate(byte_len as usize / 2);
        while buffer.last() == Some(&0) {
            buffer.pop();
        }

        String::from_utf16(&buffer)
            .map(Some)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "registry value is not UTF-16"))
    }

    pub(super) fn write_run_value(command: &str) -> io::Result<()> {
        let subkey = wide(RUN_KEY);
        let mut key: HKEY = null_mut();
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                0,
                null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                null(),
                &mut key,
                null_mut(),
            )
        };
        win32_result(status)?;
        let key = RegistryKey(key);
        let value_name = wide(RUN_VALUE_NAME);
        let command = wide(command);
        let bytes = command
            .len()
            .checked_mul(2)
            .and_then(|size| u32::try_from(size).ok())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "launch command is too large")
            })?;
        let status = unsafe {
            RegSetValueExW(
                key.0,
                value_name.as_ptr(),
                0,
                REG_SZ,
                command.as_ptr().cast::<u8>(),
                bytes,
            )
        };
        win32_result(status)
    }

    pub(super) fn delete_run_value() -> io::Result<()> {
        let subkey = wide(RUN_KEY);
        let value_name = wide(RUN_VALUE_NAME);
        let status =
            unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, subkey.as_ptr(), value_name.as_ptr()) };
        if status == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            win32_result(status)
        }
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }

    fn win32_result(status: u32) -> io::Result<()> {
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status as i32))
        }
    }

    struct RegistryKey(HKEY);

    impl Drop for RegistryKey {
        fn drop(&mut self) {
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LaunchAtSignInState, apply_launch_at_sign_in_change, build_launch_command,
        classify_launch_command,
    };
    use std::cell::RefCell;
    use std::path::Path;
    use std::rc::Rc;

    #[test]
    fn launch_command_is_exactly_quoted_and_backgrounded() {
        assert_eq!(
            build_launch_command(Path::new(r"C:\Program Files\MultiCore\MultiCore.exe")).unwrap(),
            r#""C:\Program Files\MultiCore\MultiCore.exe" --background"#
        );
    }

    #[test]
    fn only_the_exact_background_command_is_enabled() {
        let executable = Path::new(r"C:\Apps\MultiCore.exe");
        let exact = r#""C:\Apps\MultiCore.exe" --background"#;
        assert_eq!(
            classify_launch_command(Some(exact), executable).unwrap(),
            LaunchAtSignInState::Enabled
        );

        for stale in [
            r#""C:\Apps\MultiCore.exe""#,
            r#""C:\Apps\MultiCore.exe" --background --extra"#,
            r#"C:\Apps\MultiCore.exe --background"#,
            r#" "C:\Apps\MultiCore.exe" --background "#,
        ] {
            assert!(matches!(
                classify_launch_command(Some(stale), executable).unwrap(),
                LaunchAtSignInState::Stale { .. }
            ));
        }
    }

    #[test]
    fn persistence_is_reread_and_verified_without_real_registry_access() {
        let executable = Path::new(r"C:\Apps\MultiCore.exe");
        let stored = Rc::new(RefCell::new(None::<String>));
        let write_store = stored.clone();
        let delete_store = stored.clone();
        let read_store = stored.clone();

        let enabled = apply_launch_at_sign_in_change(
            true,
            executable,
            move |command| {
                *write_store.borrow_mut() = Some(command.to_owned());
                Ok(())
            },
            move || {
                *delete_store.borrow_mut() = None;
                Ok(())
            },
            move || Ok(read_store.borrow().clone()),
        )
        .unwrap();
        assert_eq!(enabled, LaunchAtSignInState::Enabled);

        let ignored_write =
            apply_launch_at_sign_in_change(true, executable, |_| Ok(()), || Ok(()), || Ok(None));
        assert!(ignored_write.is_err());
    }
}
