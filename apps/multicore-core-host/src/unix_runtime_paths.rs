use multicore_core::RuntimePaths;
use std::{
    ffi::OsStr,
    fs::{self, File},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::OnceLock,
};

const HOST_NAME: &str = "multicore-core-host";
const STICKY: u32 = 0o1000;
const GROUP_OR_OTHER_WRITE: u32 = 0o022;

static CONTEXT: OnceLock<UserContext> = OnceLock::new();

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ResolveError {
    InvalidGeneration,
    Context,
    Layout,
    Symlink,
    Escape,
    Missing,
    Ownership,
    InvalidElf,
    Io,
}

#[derive(Debug)]
pub(crate) struct UserContext {
    pub(crate) uid: u32,
    pub(crate) data_root: PathBuf,
}

pub(crate) struct ResolvedRuntime {
    pub paths: RuntimePaths,
    pub data_root: PathBuf,
    guards: Vec<GuardedPath>,
}

struct GuardedPath {
    path: PathBuf,
    handle: File,
    identity: (u64, u64),
}

impl ResolvedRuntime {
    pub(crate) fn verify_unchanged(&self) -> Result<(), ResolveError> {
        for guard in &self.guards {
            let metadata = fs::symlink_metadata(&guard.path).map_err(|_| ResolveError::Missing)?;
            if metadata.file_type().is_symlink() {
                return Err(ResolveError::Symlink);
            }
            if (metadata.dev(), metadata.ino()) != guard.identity {
                return Err(ResolveError::Escape);
            }
        }
        Ok(())
    }

    /// Reads a guarded file (matched by file name, e.g. `mihomo.yaml`) through the handle opened
    /// with `O_NOFOLLOW` during resolution, never by re-opening its path: a symlink or FIFO
    /// renamed over it afterwards is not followed. More than `limit` bytes is `Layout`.
    pub(crate) fn read_guarded(&self, path: &Path, limit: u64) -> Result<Vec<u8>, ResolveError> {
        use std::io::{Read, Seek, SeekFrom};
        let guard = self
            .guards
            .iter()
            .rev()
            .find(|guard| {
                guard.path.file_name() == path.file_name()
                    && guard
                        .handle
                        .metadata()
                        .is_ok_and(|metadata| metadata.is_file())
            })
            .ok_or(ResolveError::Missing)?;
        let mut handle = &guard.handle;
        handle
            .seek(SeekFrom::Start(0))
            .map_err(|_| ResolveError::Io)?;
        let mut bytes = Vec::new();
        handle
            .take(limit + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ResolveError::Io)?;
        if bytes.len() as u64 > limit {
            return Err(ResolveError::Layout);
        }
        Ok(bytes)
    }
}

pub(crate) fn install_context(context: UserContext) -> Result<(), ResolveError> {
    CONTEXT.set(context).map_err(|_| ResolveError::Context)
}

pub(crate) fn resolve_current(generation: u64) -> Result<ResolvedRuntime, ResolveError> {
    let context = CONTEXT.get().ok_or(ResolveError::Context)?;
    let executable = std::env::current_exe().map_err(|_| ResolveError::Io)?;
    resolve_from(&context.data_root, &executable, generation, context.uid)
}

pub(crate) fn user_data_root(
    uid: u32,
    passwd: &str,
    peer_environment: &[u8],
) -> Result<PathBuf, ResolveError> {
    let home = passwd
        .lines()
        .find_map(|line| {
            let fields = line.split(':').collect::<Vec<_>>();
            (fields.len() >= 7 && fields[2].parse::<u32>().ok() == Some(uid))
                .then(|| fields[5].to_owned())
        })
        .ok_or(ResolveError::Context)?;
    let xdg_data_home = peer_environment
        .split(|byte| *byte == 0)
        .find_map(|entry| entry.strip_prefix(b"XDG_DATA_HOME="))
        .and_then(|value| std::str::from_utf8(value).ok());
    multicore_platform::dirs::unix_data_root(xdg_data_home.map(OsStr::new), Some(OsStr::new(&home)))
        .map_err(|_| ResolveError::Context)
}

pub(crate) fn resolve_from(
    data_root: &Path,
    host_executable: &Path,
    generation: u64,
    owner_uid: u32,
) -> Result<ResolvedRuntime, ResolveError> {
    if generation == 0 {
        return Err(ResolveError::InvalidGeneration);
    }
    let runtime_dir = host_executable.parent().ok_or(ResolveError::Layout)?;
    if runtime_dir.file_name().and_then(|value| value.to_str()) != Some("runtime")
        || host_executable.file_name().and_then(|value| value.to_str()) != Some(HOST_NAME)
    {
        return Err(ResolveError::Layout);
    }
    let package = runtime_dir.parent().ok_or(ResolveError::Layout)?;
    let cores = package.join("cores");
    let runtime_root = data_root.join("runtime");
    let generation_dir = runtime_root.join(format!("runtime-{generation:020}"));
    let xray = cores.join("xray");
    let mihomo = cores.join("mihomo");
    let xray_config = generation_dir.join("xray.json");
    let mihomo_config = generation_dir.join("mihomo.yaml");
    // The vendored dashboard is optional: absent in builds without one, present as an exact
    // package-root sibling of `cores` otherwise. Its presence is decided once, up front, so a
    // directory that appears only after that check can never be picked up mid-resolution.
    //
    // Presence is "something exists at this path at all" (`is_ok()`), not "is a directory"
    // (`is_dir()`). On Unix, `symlink_metadata` never reports `is_dir()` true for a symlink -
    // `lstat`'s mode bits report `S_IFLNK`, never `S_IFDIR`, regardless of what the symlink
    // points to. Gating presence on `is_dir()` would make a dashboard directory replaced with a
    // symlink silently look identical to a genuinely absent dashboard: `dashboard_present` would
    // be `false`, the guard chain below would never run, and resolution would quietly succeed
    // with `dashboard_directory: None` instead of failing closed. That is a real divergence from
    // the Windows resolver's equivalent check just below: a Windows directory reparse
    // point/symlink keeps `FILE_ATTRIBUTE_DIRECTORY` set, so it is still seen as "present" there
    // and correctly rejected by the guard chain as `ResolveError::Reparse`. Checking `is_ok()`
    // instead makes a symlinked (or otherwise non-directory) dashboard path reach guard_chain
    // and its `O_NOFOLLOW` open below, which rejects a symlink as `ResolveError::Symlink` and
    // anything else as `ResolveError::Layout` - matching the Windows fail-closed behavior instead
    // of silently disabling the dashboard feature with no signal anything was wrong.
    let dashboard = package.join("dashboard");
    let dashboard_present = fs::symlink_metadata(&dashboard).is_ok();

    let mut guards = Vec::new();
    for directory in [
        data_root,
        &runtime_root,
        &generation_dir,
        package,
        runtime_dir,
        &cores,
    ] {
        guard_chain(directory, true, owner_uid, false, &mut guards)?;
    }
    if dashboard_present {
        guard_chain(&dashboard, true, owner_uid, false, &mut guards)?;
    }
    for file in [
        host_executable,
        &xray,
        &mihomo,
        &xray_config,
        &mihomo_config,
    ] {
        let staged_config = *file == xray_config || *file == mihomo_config;
        guard_chain(file, false, owner_uid, staged_config, &mut guards)?;
    }

    let canonical = |path: &Path| fs::canonicalize(path).map_err(|_| ResolveError::Missing);
    let canonical_package = canonical(package)?;
    let canonical_generation = canonical(&generation_dir)?;
    let canonical_data = canonical(data_root)?;
    if !canonical_generation.starts_with(&canonical_data) {
        return Err(ResolveError::Escape);
    }
    for binary in [host_executable, &xray, &mihomo] {
        if !canonical(binary)?.starts_with(&canonical_package) {
            return Err(ResolveError::Escape);
        }
    }
    for config in [&xray_config, &mihomo_config] {
        if !canonical(config)?.starts_with(&canonical_generation) {
            return Err(ResolveError::Escape);
        }
    }
    let canonical_dashboard = if dashboard_present {
        let canonical_dashboard = canonical(&dashboard)?;
        if !canonical_dashboard.starts_with(&canonical_package) {
            return Err(ResolveError::Escape);
        }
        Some(canonical_dashboard)
    } else {
        None
    };
    for binary in [&xray, &mihomo] {
        let guard = guards
            .iter_mut()
            .rev()
            .find(|guard| guard.path == *binary)
            .ok_or(ResolveError::Missing)?;
        #[cfg(target_os = "macos")]
        multicore_platform::macho::validate_macho64_arm64(&mut guard.handle)
            .map_err(|_| ResolveError::InvalidElf)?;
        #[cfg(not(target_os = "macos"))]
        multicore_platform::elf::validate_elf64_x86_64(&mut guard.handle)
            .map_err(|_| ResolveError::InvalidElf)?;
    }
    Ok(ResolvedRuntime {
        paths: RuntimePaths {
            xray_binary: canonical(&xray)?,
            mihomo_binary: canonical(&mihomo)?,
            xray_config: canonical(&xray_config)?,
            mihomo_config: canonical(&mihomo_config)?,
            dashboard_directory: canonical_dashboard,
            mihomo_home_directory: None,
        },
        data_root: canonical_data,
        guards,
    })
}

/// Guards every component of `path`, opening each one with `openat` relative to its
/// already-opened parent and `O_NOFOLLOW`, so no symbolic link is followed anywhere in the chain
/// (an intermediate directory swapped for a symlink mid-walk is refused, not traversed). With
/// `owned_by_client`, the final component must belong to `owner_uid` itself (not root): the
/// staged configs the privileged reader consumes.
fn guard_chain(
    path: &Path,
    target_directory: bool,
    owner_uid: u32,
    owned_by_client: bool,
    guards: &mut Vec<GuardedPath>,
) -> Result<(), ResolveError> {
    use std::path::Component;
    if !path.is_absolute()
        || !path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
    {
        return Err(ResolveError::Layout);
    }
    let mut ancestors = path.ancestors().collect::<Vec<_>>();
    ancestors.reverse();
    let mut parent: Option<File> = None;
    for ancestor in ancestors {
        if ancestor.parent().is_none() {
            let root = rustix::fs::open(
                ancestor,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .map_err(|_| ResolveError::Io)?;
            parent = Some(File::from(root));
            continue;
        }
        if let Some(existing) = guards.iter().find(|guard| guard.path == ancestor) {
            parent = Some(existing.handle.try_clone().map_err(|_| ResolveError::Io)?);
            continue;
        }
        let final_component = ancestor == path;
        let directory = !final_component || target_directory;
        let guard = open_guard(
            parent.as_ref().ok_or(ResolveError::Layout)?,
            ancestor,
            directory,
            final_component,
            owner_uid,
            final_component && owned_by_client,
        )?;
        parent = Some(guard.handle.try_clone().map_err(|_| ResolveError::Io)?);
        guards.push(guard);
    }
    Ok(())
}

fn open_guard(
    parent: &File,
    path: &Path,
    directory: bool,
    final_component: bool,
    owner_uid: u32,
    owned_by_client: bool,
) -> Result<GuardedPath, ResolveError> {
    use rustix::fs::{AtFlags, FileType, Mode, OFlags};
    let name = path.file_name().ok_or(ResolveError::Layout)?;
    // `NONBLOCK`: a FIFO planted in the user-owned tree must not block the privileged opener
    // (it is then rejected as not a regular file below).
    let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    if directory {
        flags |= OFlags::DIRECTORY;
    }
    let handle = rustix::fs::openat(parent, name, flags, Mode::empty())
        .map(File::from)
        .map_err(|error| {
            let symlink = rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
                .is_ok_and(|stat| FileType::from_raw_mode(stat.st_mode) == FileType::Symlink);
            if symlink {
                ResolveError::Symlink
            } else if error == rustix::io::Errno::NOENT {
                ResolveError::Missing
            } else {
                ResolveError::Layout
            }
        })?;
    let metadata = handle.metadata().map_err(|_| ResolveError::Io)?;
    if metadata.is_dir() != directory || (!directory && !metadata.is_file()) {
        return Err(ResolveError::Layout);
    }
    if (metadata.uid() != 0 || owned_by_client) && metadata.uid() != owner_uid {
        return Err(ResolveError::Ownership);
    }
    let mode = metadata.mode();
    let shared_sticky_root =
        directory && !final_component && metadata.uid() == 0 && mode & STICKY != 0;
    if mode & GROUP_OR_OTHER_WRITE != 0 && !shared_sticky_root {
        return Err(ResolveError::Ownership);
    }
    Ok(GuardedPath {
        path: path.to_path_buf(),
        identity: (metadata.dev(), metadata.ino()),
        handle,
    })
}
