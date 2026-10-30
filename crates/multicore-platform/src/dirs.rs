use std::{
    ffi::OsStr,
    io,
    path::{Path, PathBuf},
};

#[cfg(windows)]
const WINDOWS_APP_DIRECTORY: &str = "MultiCore";
#[cfg(unix)]
const UNIX_APP_DIRECTORY: &str = "multicore";
#[cfg(target_os = "macos")]
const MACOS_APP_DIRECTORY: &str = "MultiCore";

pub fn user_data_root() -> io::Result<PathBuf> {
    #[cfg(windows)]
    {
        windows_data_root(std::env::var_os("LOCALAPPDATA").as_deref())
    }
    #[cfg(target_os = "macos")]
    {
        macos_library_root(std::env::var_os("HOME").as_deref(), "Application Support")
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        unix_data_root(
            std::env::var_os("XDG_DATA_HOME").as_deref(),
            std::env::var_os("HOME").as_deref(),
        )
    }
}

/// `~/Library/<folder>/MultiCore`, where macOS apps keep their data and caches.
#[cfg(target_os = "macos")]
fn macos_library_root(home: Option<&OsStr>, folder: &str) -> io::Result<PathBuf> {
    Ok(absolute(home)
        .ok_or_else(|| unavailable("HOME"))?
        .join("Library")
        .join(folder)
        .join(MACOS_APP_DIRECTORY))
}

#[cfg(unix)]
pub fn user_config_root() -> io::Result<PathBuf> {
    unix_config_root(
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

#[cfg(target_os = "macos")]
pub fn user_cache_root() -> io::Result<PathBuf> {
    macos_library_root(std::env::var_os("HOME").as_deref(), "Caches")
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn user_cache_root() -> io::Result<PathBuf> {
    unix_cache_root(
        std::env::var_os("XDG_CACHE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// macOS has no `XDG_RUNTIME_DIR`; the per-user `TMPDIR` (`/var/folders/…/T/`, private to
/// the user and cleaned at boot) plays its part.
#[cfg(unix)]
pub fn user_runtime_dir() -> io::Result<PathBuf> {
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("XDG_RUNTIME_DIR").or_else(|| std::env::var_os("TMPDIR"));
    #[cfg(not(target_os = "macos"))]
    let base = std::env::var_os("XDG_RUNTIME_DIR");
    let directory = unix_runtime_dir(base.as_deref())?;
    ensure_private_directory(&directory)?;
    Ok(directory)
}

fn absolute(value: Option<&OsStr>) -> Option<&Path> {
    value
        .filter(|value| !value.is_empty())
        .map(Path::new)
        .filter(|path| path.is_absolute())
}

fn unavailable(name: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("{name} is not an absolute directory"),
    )
}

#[cfg(windows)]
fn windows_data_root(local_app_data: Option<&OsStr>) -> io::Result<PathBuf> {
    absolute(local_app_data)
        .map(|root| root.join(WINDOWS_APP_DIRECTORY))
        .ok_or_else(|| unavailable("LOCALAPPDATA"))
}

#[cfg(unix)]
pub fn unix_data_root(xdg_data_home: Option<&OsStr>, home: Option<&OsStr>) -> io::Result<PathBuf> {
    let base = match absolute(xdg_data_home) {
        Some(base) => base.to_path_buf(),
        None => absolute(home)
            .ok_or_else(|| unavailable("HOME"))?
            .join(".local")
            .join("share"),
    };
    Ok(base.join(UNIX_APP_DIRECTORY))
}

#[cfg(unix)]
fn unix_config_root(xdg_config_home: Option<&OsStr>, home: Option<&OsStr>) -> io::Result<PathBuf> {
    match absolute(xdg_config_home) {
        Some(base) => Ok(base.to_path_buf()),
        None => Ok(absolute(home)
            .ok_or_else(|| unavailable("HOME"))?
            .join(".config")),
    }
}

#[cfg(unix)]
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn unix_cache_root(xdg_cache_home: Option<&OsStr>, home: Option<&OsStr>) -> io::Result<PathBuf> {
    let base = match absolute(xdg_cache_home) {
        Some(base) => base.to_path_buf(),
        None => absolute(home)
            .ok_or_else(|| unavailable("HOME"))?
            .join(".cache"),
    };
    Ok(base.join(UNIX_APP_DIRECTORY))
}

#[cfg(unix)]
fn unix_runtime_dir(xdg_runtime_dir: Option<&OsStr>) -> io::Result<PathBuf> {
    absolute(xdg_runtime_dir)
        .map(|root| root.join(UNIX_APP_DIRECTORY))
        .ok_or_else(|| unavailable("XDG_RUNTIME_DIR"))
}

#[cfg(unix)]
fn ensure_private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};

    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match std::fs::DirBuilder::new().mode(0o700).create(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
    }
    let metadata = std::fs::symlink_metadata(path)?;
    let unsafe_entry = |reason: &str| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("private directory {reason}"),
        )
    };
    if !metadata.file_type().is_dir() {
        return Err(unsafe_entry("is not a real directory"));
    }
    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(unsafe_entry("is owned by another user"));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(unsafe_entry("is accessible to other users"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn windows_data_root_requires_a_nonempty_absolute_local_app_data() {
        assert!(windows_data_root(None).is_err());
        assert!(windows_data_root(Some(OsStr::new(""))).is_err());
        assert!(windows_data_root(Some(OsStr::new("relative-root"))).is_err());
        assert_eq!(
            windows_data_root(Some(OsStr::new(r"C:\Users\u\AppData\Local"))).unwrap(),
            PathBuf::from(r"C:\Users\u\AppData\Local\MultiCore")
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_data_root_prefers_absolute_xdg_data_home() {
        assert_eq!(
            unix_data_root(Some(OsStr::new("/data/xdg")), Some(OsStr::new("/home/u"))).unwrap(),
            PathBuf::from("/data/xdg/multicore")
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_data_root_ignores_empty_or_relative_xdg_and_uses_home_default() {
        for xdg in [None, Some(OsStr::new("")), Some(OsStr::new("relative"))] {
            assert_eq!(
                unix_data_root(xdg, Some(OsStr::new("/home/u"))).unwrap(),
                PathBuf::from("/home/u/.local/share/multicore")
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_data_root_requires_an_absolute_home_without_xdg() {
        assert!(unix_data_root(None, None).is_err());
        assert!(unix_data_root(None, Some(OsStr::new(""))).is_err());
        assert!(unix_data_root(None, Some(OsStr::new("home/u"))).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unix_config_root_follows_xdg_config_home_then_home() {
        assert_eq!(
            unix_config_root(Some(OsStr::new("/cfg")), Some(OsStr::new("/home/u"))).unwrap(),
            PathBuf::from("/cfg")
        );
        assert_eq!(
            unix_config_root(Some(OsStr::new("cfg")), Some(OsStr::new("/home/u"))).unwrap(),
            PathBuf::from("/home/u/.config")
        );
        assert!(unix_config_root(None, None).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unix_cache_root_follows_xdg_cache_home_then_home() {
        assert_eq!(
            unix_cache_root(Some(OsStr::new("/c")), Some(OsStr::new("/home/u"))).unwrap(),
            PathBuf::from("/c/multicore")
        );
        assert_eq!(
            unix_cache_root(Some(OsStr::new("c")), Some(OsStr::new("/home/u"))).unwrap(),
            PathBuf::from("/home/u/.cache/multicore")
        );
        assert!(unix_cache_root(None, None).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unix_runtime_dir_requires_absolute_xdg_runtime_dir() {
        assert!(unix_runtime_dir(None).is_err());
        assert!(unix_runtime_dir(Some(OsStr::new(""))).is_err());
        assert!(unix_runtime_dir(Some(OsStr::new("run/user/1000"))).is_err());
        assert_eq!(
            unix_runtime_dir(Some(OsStr::new("/run/user/1000"))).unwrap(),
            PathBuf::from("/run/user/1000/multicore")
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_directory_is_created_owner_only_and_rejects_unsafe_existing_entries() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let root = tempfile::tempdir().unwrap();
        let created = root.path().join("created");
        ensure_private_directory(&created).unwrap();
        let mode = std::fs::metadata(&created).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        ensure_private_directory(&created).unwrap();

        let open = root.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ensure_private_directory(&open).is_err());

        let file = root.path().join("file");
        std::fs::write(&file, b"").unwrap();
        assert!(ensure_private_directory(&file).is_err());

        let link = root.path().join("link");
        symlink(&created, &link).unwrap();
        assert!(ensure_private_directory(&link).is_err());
    }
}
