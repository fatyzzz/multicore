use multicore_i18n::tr;
use std::{
    ffi::{OsStr, OsString},
    fmt,
    fs::File,
    io::Read,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use crate::daemon::{DaemonClient, DaemonStatus, HttpDaemonClient};

#[cfg(windows)]
use std::io::{Seek, SeekFrom};

pub(crate) const READINESS_LIMIT: usize = 96;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
const RETRY_DELAY: Duration = Duration::from_millis(40);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BootstrapError {
    Configuration,
    Package,
    Random,
    Startup,
    Readiness,
    Authentication,
}

impl BootstrapError {
    pub(crate) fn user_message(&self) -> &'static str {
        match self {
            Self::Configuration => tr!("Проверьте настройки подключения к фоновому сервису."),
            Self::Package => {
                tr!("Файлы приложения повреждены или отсутствуют. Распакуйте пакет заново.")
            }
            Self::Random => tr!("Не удалось безопасно подготовить запуск фонового сервиса."),
            Self::Startup => tr!("Не удалось запустить фоновый сервис."),
            Self::Readiness => tr!("Фоновый сервис запустился некорректно."),
            Self::Authentication => tr!("Фоновый сервис не прошёл проверку подлинности."),
        }
    }
}

impl fmt::Display for BootstrapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.user_message())
    }
}

impl std::error::Error for BootstrapError {}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StartupSelection {
    Packaged,
    External { url: String, token: String },
}

fn select_startup(
    url: Option<&OsStr>,
    token: Option<&OsStr>,
) -> Result<StartupSelection, BootstrapError> {
    match (url, token) {
        (None, None) => Ok(StartupSelection::Packaged),
        (Some(url), Some(token)) if !url.is_empty() && !token.is_empty() => {
            let url = url.to_str().ok_or(BootstrapError::Configuration)?;
            let token = token.to_str().ok_or(BootstrapError::Configuration)?;
            Ok(StartupSelection::External {
                url: url.to_owned(),
                token: token.to_owned(),
            })
        }
        _ => Err(BootstrapError::Configuration),
    }
}

pub(crate) struct Bootstrap {
    client: Arc<dyn DaemonClient>,
    _owned: Option<OwnedDaemon>,
}

impl Bootstrap {
    pub(crate) fn client(&self) -> Arc<dyn DaemonClient> {
        Arc::clone(&self.client)
    }
}

/// Upper bound for the best-effort disconnect performed before the owned
/// daemon is terminated on exit.
const SHUTDOWN_DISCONNECT_TIMEOUT: Duration = Duration::from_secs(5);

impl Drop for Bootstrap {
    fn drop(&mut self) {
        // Runs before `_owned` is dropped, i.e. before the daemon is killed, so
        // an active tunnel is torn down in order instead of by process death.
        disconnect_before_shutdown(
            self._owned.is_some(),
            Arc::clone(&self.client),
            SHUTDOWN_DISCONNECT_TIMEOUT,
        );
    }
}

/// Ask an owned daemon to disconnect and then to let the always-ready service
/// stop the cores (Quit), waiting at most `timeout`. An external daemon is not
/// ours to disconnect. Returns whether the disconnect succeeded.
fn disconnect_before_shutdown(
    owned: bool,
    client: Arc<dyn DaemonClient>,
    timeout: Duration,
) -> bool {
    if !owned {
        return false;
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("multicore-shutdown-disconnect".into())
        .spawn(move || {
            let disconnected = client.disconnect().is_ok();
            let _ = client.shutdown();
            let _ = sender.send(disconnected);
        });
    spawned.is_ok() && receiver.recv_timeout(timeout).unwrap_or(false)
}

pub(crate) fn start() -> Result<Bootstrap, BootstrapError> {
    let selection = select_startup(
        std::env::var_os("MULTICORE_DAEMON_URL").as_deref(),
        std::env::var_os("MULTICORE_DAEMON_TOKEN").as_deref(),
    )?;
    match selection {
        StartupSelection::External { url, token } => {
            let client =
                HttpDaemonClient::new(url, token).map_err(|_| BootstrapError::Configuration)?;
            Ok(Bootstrap {
                client: Arc::new(client),
                _owned: None,
            })
        }
        StartupSelection::Packaged => start_packaged(),
    }
}

#[cfg(windows)]
mod layout {
    pub(super) const DAEMON: &str = "multicore-daemon.exe";
    pub(super) const XRAY: &str = "xray.exe";
    pub(super) const MIHOMO: &str = "mihomo.exe";
    pub(super) const RUNTIME_SUPPORT: &[&str] = &["multicore-updater.exe"];
}

#[cfg(not(windows))]
mod layout {
    pub(super) const DAEMON: &str = "multicore-daemon";
    pub(super) const XRAY: &str = "xray";
    pub(super) const MIHOMO: &str = "mihomo";
    pub(super) const RUNTIME_SUPPORT: &[&str] = &["multicore-core-host"];
}

struct PackagePaths {
    package_root: PathBuf,
    daemon: PathBuf,
    xray: PathBuf,
    mihomo: PathBuf,
    data_dir: PathBuf,
    locks: Vec<File>,
}

fn derive_and_validate_paths(
    current_exe: &Path,
    data_dir: impl AsRef<Path>,
) -> Result<PackagePaths, BootstrapError> {
    let package_root = current_exe
        .parent()
        .ok_or(BootstrapError::Package)?
        .to_path_buf();
    let runtime_dir = package_root.join("runtime");
    let cores_dir = package_root.join("cores");
    let mut locks = vec![
        open_locked_directory(&package_root)?,
        open_locked_directory(&runtime_dir)?,
        open_locked_directory(&cores_dir)?,
    ];
    let package_root_canonical = package_root
        .canonicalize()
        .map_err(|_| BootstrapError::Package)?;
    let daemon = runtime_dir.join(layout::DAEMON);
    let support = layout::RUNTIME_SUPPORT
        .iter()
        .map(|name| runtime_dir.join(name))
        .collect::<Vec<_>>();
    let xray = cores_dir.join(layout::XRAY);
    let mihomo = cores_dir.join(layout::MIHOMO);
    locks.extend(
        [&daemon]
            .into_iter()
            .chain(&support)
            .chain([&xray, &mihomo])
            .map(|executable| {
                validate_packaged_executable(&package_root, &package_root_canonical, executable)
            })
            .collect::<Result<Vec<_>, _>>()?,
    );
    Ok(PackagePaths {
        package_root,
        daemon,
        xray,
        mihomo,
        data_dir: data_dir.as_ref().to_path_buf(),
        locks,
    })
}

#[cfg(windows)]
fn open_locked_directory(path: &Path) -> Result<File, BootstrapError> {
    use std::{fs::OpenOptions, os::windows::fs::OpenOptionsExt};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    reject_reparse(path)?;
    let directory = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|_| BootstrapError::Package)?;
    if !directory
        .metadata()
        .map_err(|_| BootstrapError::Package)?
        .is_dir()
    {
        return Err(BootstrapError::Package);
    }
    Ok(directory)
}

#[cfg(not(windows))]
fn open_locked_directory(path: &Path) -> Result<File, BootstrapError> {
    reject_reparse(path)?;
    File::open(path).map_err(|_| BootstrapError::Package)
}

fn reject_reparse(path: &Path) -> Result<(), BootstrapError> {
    let metadata = path
        .symlink_metadata()
        .map_err(|_| BootstrapError::Package)?;
    if metadata.file_type().is_symlink() {
        return Err(BootstrapError::Package);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(BootstrapError::Package);
        }
    }
    Ok(())
}

fn validate_packaged_executable(
    package_root: &Path,
    canonical_root: &Path,
    executable: &Path,
) -> Result<File, BootstrapError> {
    let relative = executable
        .strip_prefix(package_root)
        .map_err(|_| BootstrapError::Package)?;
    let mut component = package_root.to_path_buf();
    for part in relative.components() {
        component.push(part);
        reject_reparse(&component)?;
    }
    let canonical = executable
        .canonicalize()
        .map_err(|_| BootstrapError::Package)?;
    if !canonical.starts_with(canonical_root) {
        return Err(BootstrapError::Package);
    }
    let mut file = open_locked_executable(executable)?;
    if !file
        .metadata()
        .map_err(|_| BootstrapError::Package)?
        .is_file()
    {
        return Err(BootstrapError::Package);
    }
    validate_native_executable(&mut file)?;
    Ok(file)
}

#[cfg(windows)]
fn validate_native_executable(file: &mut File) -> Result<(), BootstrapError> {
    validate_native_pe(file)
}

#[cfg(not(windows))]
fn validate_native_executable(file: &mut File) -> Result<(), BootstrapError> {
    use std::os::unix::fs::PermissionsExt;

    let mode = file
        .metadata()
        .map_err(|_| BootstrapError::Package)?
        .permissions()
        .mode();
    if mode & 0o111 == 0 {
        return Err(BootstrapError::Package);
    }
    #[cfg(target_os = "macos")]
    return multicore_platform::macho::validate_macho64_arm64(file)
        .map_err(|_| BootstrapError::Package);
    #[cfg(not(target_os = "macos"))]
    multicore_platform::elf::validate_elf64_x86_64(file).map_err(|_| BootstrapError::Package)
}

#[cfg(windows)]
fn open_locked_executable(path: &Path) -> Result<File, BootstrapError> {
    use std::{fs::OpenOptions, os::windows::fs::OpenOptionsExt};
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ};

    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|_| BootstrapError::Package)
}

#[cfg(not(windows))]
fn open_locked_executable(path: &Path) -> Result<File, BootstrapError> {
    File::open(path).map_err(|_| BootstrapError::Package)
}

/// The package's executables must be images of this build's architecture: PE32+ AMD64 for the
/// x64 build, PE32 i386 for the x86 build, PE32+ ARM64 for the ARM64 build.
#[cfg(windows)]
fn validate_native_pe(file: &mut File) -> Result<(), BootstrapError> {
    const MAX_PE_HEADER_BYTES: u64 = 1024 * 1024;
    const MAX_OPTIONAL_HEADER: u16 = 4096;
    let arch = multicore_release::WindowsArch::CURRENT;
    let fixed_optional_header = arch.pe_optional_header_fixed_size();
    const MAX_SECTIONS: u16 = 96;
    const IMAGE_FILE_EXECUTABLE_IMAGE: u16 = 0x0002;
    const IMAGE_FILE_DLL: u16 = 0x2000;

    let mut dos = [0_u8; 64];
    file.read_exact(&mut dos)
        .map_err(|_| BootstrapError::Package)?;
    if &dos[..2] != b"MZ" {
        return Err(BootstrapError::Package);
    }
    let offset = u32::from_le_bytes(dos[0x3c..0x40].try_into().expect("fixed slice")) as u64;
    let length = file.metadata().map_err(|_| BootstrapError::Package)?.len();
    let optional_start = offset.checked_add(24).ok_or(BootstrapError::Package)?;
    if !(64..=MAX_PE_HEADER_BYTES).contains(&offset) || optional_start > length {
        return Err(BootstrapError::Package);
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(|_| BootstrapError::Package)?;
    let mut coff = [0_u8; 24];
    file.read_exact(&mut coff)
        .map_err(|_| BootstrapError::Package)?;
    let machine = u16::from_le_bytes([coff[4], coff[5]]);
    let section_count = u16::from_le_bytes([coff[6], coff[7]]);
    let optional_size = u16::from_le_bytes([coff[20], coff[21]]);
    let characteristics = u16::from_le_bytes([coff[22], coff[23]]);
    if &coff[..4] != b"PE\0\0"
        || machine != arch.pe_machine()
        || section_count == 0
        || section_count > MAX_SECTIONS
        || characteristics & IMAGE_FILE_EXECUTABLE_IMAGE == 0
        || characteristics & IMAGE_FILE_DLL != 0
        || !(fixed_optional_header..=MAX_OPTIONAL_HEADER).contains(&optional_size)
    {
        return Err(BootstrapError::Package);
    }

    let optional_end = optional_start
        .checked_add(u64::from(optional_size))
        .ok_or(BootstrapError::Package)?;
    let section_table_end = optional_end
        .checked_add(u64::from(section_count) * 40)
        .ok_or(BootstrapError::Package)?;
    if section_table_end > length || section_table_end > MAX_PE_HEADER_BYTES {
        return Err(BootstrapError::Package);
    }

    file.seek(SeekFrom::Start(optional_start))
        .map_err(|_| BootstrapError::Package)?;
    // PE32 and PE32+ share SizeOfHeaders at offset 60; NumberOfRvaAndSizes closes the fixed part.
    let fixed = usize::from(fixed_optional_header);
    let mut optional = [0_u8; 112];
    file.read_exact(&mut optional[..fixed])
        .map_err(|_| BootstrapError::Package)?;
    let optional_magic = u16::from_le_bytes([optional[0], optional[1]]);
    let size_of_headers =
        u32::from_le_bytes(optional[60..64].try_into().expect("fixed slice")) as u64;
    let data_directory_count =
        u32::from_le_bytes(optional[fixed - 4..fixed].try_into().expect("fixed slice"));
    let required_optional_size = u64::from(fixed_optional_header)
        .checked_add(u64::from(data_directory_count) * 8)
        .ok_or(BootstrapError::Package)?;
    if optional_magic != arch.pe_optional_magic()
        || data_directory_count > 16
        || required_optional_size > u64::from(optional_size)
        || size_of_headers < section_table_end
        || size_of_headers > length
        || size_of_headers > MAX_PE_HEADER_BYTES
    {
        return Err(BootstrapError::Package);
    }

    file.seek(SeekFrom::Start(optional_end))
        .map_err(|_| BootstrapError::Package)?;
    let mut raw_ranges = Vec::with_capacity(section_count as usize);
    for _ in 0..section_count {
        let mut section = [0_u8; 40];
        file.read_exact(&mut section)
            .map_err(|_| BootstrapError::Package)?;
        let raw_size = u32::from_le_bytes(section[16..20].try_into().expect("fixed slice")) as u64;
        let raw_start = u32::from_le_bytes(section[20..24].try_into().expect("fixed slice")) as u64;
        if raw_size == 0 {
            continue;
        }
        let raw_end = raw_start
            .checked_add(raw_size)
            .ok_or(BootstrapError::Package)?;
        if raw_start < size_of_headers || raw_end > length {
            return Err(BootstrapError::Package);
        }
        if raw_ranges
            .iter()
            .any(|&(start, end)| raw_start < end && start < raw_end)
        {
            return Err(BootstrapError::Package);
        }
        raw_ranges.push((raw_start, raw_end));
    }
    Ok(())
}

fn generate_token() -> Result<String, BootstrapError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| BootstrapError::Random)?;
    let mut token = String::with_capacity(64);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        token.push(HEX[(byte >> 4) as usize] as char);
        token.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(token)
}

fn parse_readiness(bytes: &[u8]) -> Result<SocketAddr, BootstrapError> {
    if bytes.len() > READINESS_LIMIT
        || !bytes.ends_with(b"\n")
        || bytes[..bytes.len().saturating_sub(1)].contains(&b'\n')
        || !bytes.is_ascii()
    {
        return Err(BootstrapError::Readiness);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| BootstrapError::Readiness)?;
    let address = text
        .strip_prefix("MULTICORE_READY ")
        .and_then(|value| value.strip_suffix('\n'))
        .ok_or(BootstrapError::Readiness)?
        .parse::<SocketAddr>()
        .map_err(|_| BootstrapError::Readiness)?;
    if address.ip() != IpAddr::V4(std::net::Ipv4Addr::LOCALHOST) || address.port() == 0 {
        return Err(BootstrapError::Readiness);
    }
    Ok(address)
}

#[cfg(windows)]
use windows_bootstrap as platform_bootstrap;

#[cfg(not(windows))]
use unix_bootstrap as platform_bootstrap;

use platform_bootstrap::OwnedDaemon;

const DAEMON_ENVIRONMENT_KEYS: &[&str] = &[
    "MULTICORE_DAEMON_URL",
    "MULTICORE_DAEMON_TOKEN",
    "MULTICORE_DAEMON_READY_STDOUT",
    "MULTICORE_DAEMON_ADDR",
    "MULTICORE_DAEMON_DATA_DIR",
    "MULTICORE_XRAY_BIN",
    "MULTICORE_MIHOMO_BIN",
    "MULTICORE_MIHOMO_CONTROLLER_ADDR",
];

fn daemon_environment(paths: &PackagePaths, token: &str) -> Vec<(OsString, OsString)> {
    vec![
        (
            OsString::from("MULTICORE_DAEMON_TOKEN"),
            OsString::from(token),
        ),
        (
            OsString::from("MULTICORE_DAEMON_READY_STDOUT"),
            OsString::from("1"),
        ),
        (
            OsString::from("MULTICORE_DAEMON_ADDR"),
            OsString::from("127.0.0.1:0"),
        ),
        (
            OsString::from("MULTICORE_DAEMON_DATA_DIR"),
            paths.data_dir.as_os_str().to_owned(),
        ),
        (
            OsString::from("MULTICORE_XRAY_BIN"),
            paths.xray.as_os_str().to_owned(),
        ),
        (
            OsString::from("MULTICORE_MIHOMO_BIN"),
            paths.mihomo.as_os_str().to_owned(),
        ),
        (
            OsString::from("MULTICORE_MIHOMO_CONTROLLER_ADDR"),
            OsString::from("127.0.0.1:19090"),
        ),
    ]
}

type ReadinessReceiver = std::sync::mpsc::Receiver<Result<SocketAddr, BootstrapError>>;

fn spawn_readiness_reader(
    mut pipe: impl Read + Send + 'static,
) -> Result<ReadinessReceiver, BootstrapError> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("multicore-readiness".into())
        .spawn(move || {
            let mut bytes = Vec::with_capacity(READINESS_LIMIT);
            let result = loop {
                let mut byte = [0_u8; 1];
                match pipe.read(&mut byte) {
                    Ok(0) => break Err(BootstrapError::Readiness),
                    Ok(_) => {
                        bytes.push(byte[0]);
                        if bytes.len() > READINESS_LIMIT {
                            break Err(BootstrapError::Readiness);
                        }
                        if byte[0] == b'\n' {
                            break parse_readiness(&bytes);
                        }
                    }
                    Err(_) => break Err(BootstrapError::Readiness),
                }
            };
            let _ = sender.send(result);
        })
        .map_err(|_| BootstrapError::Startup)?;
    Ok(receiver)
}

fn await_readiness_signal(
    receiver: ReadinessReceiver,
    deadline: Instant,
    mut has_exited: impl FnMut() -> Result<bool, BootstrapError>,
) -> Result<SocketAddr, BootstrapError> {
    loop {
        if has_exited()? {
            return Err(BootstrapError::Startup);
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(BootstrapError::Startup)?;
        match receiver.recv_timeout(Duration::from_millis(20).min(remaining)) {
            Ok(result) => return result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(BootstrapError::Readiness);
            }
        }
    }
}

fn start_packaged() -> Result<Bootstrap, BootstrapError> {
    let current_exe = std::env::current_exe().map_err(|_| BootstrapError::Package)?;
    let data_dir = crate::data_root::user_data_root().map_err(|_| BootstrapError::Package)?;
    let paths = derive_and_validate_paths(&current_exe, data_dir)?;
    std::fs::create_dir_all(&paths.data_dir).map_err(|_| {
        eprintln!("bootstrap stage failed: data-directory");
        BootstrapError::Startup
    })?;
    let token = generate_token()?;
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let owned = platform_bootstrap::spawn(paths, &token).inspect_err(|_| {
        eprintln!("bootstrap stage failed: process-create");
    })?;
    let (owned, address) = await_owned_readiness(owned, deadline).inspect_err(|_| {
        eprintln!("bootstrap stage failed: readiness");
    })?;
    let url = format!("http://{address}");
    let owned = verify_status(&url, &token, owned, deadline).inspect_err(|_| {
        eprintln!("bootstrap stage failed: authenticated-status");
    })?;
    let client = HttpDaemonClient::new(url, token).map_err(|_| {
        eprintln!("bootstrap stage failed: client-create");
        BootstrapError::Startup
    })?;
    Ok(Bootstrap {
        client: Arc::new(client),
        _owned: Some(owned),
    })
}

fn verify_status(
    url: &str,
    token: &str,
    owned: OwnedDaemon,
    deadline: Instant,
) -> Result<OwnedDaemon, BootstrapError> {
    let http = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| BootstrapError::Startup)?;
    await_owned_status_with(
        owned,
        deadline,
        |remaining| {
            let response = http
                .get(format!("{url}/v1/status"))
                .bearer_auth(token)
                .timeout(remaining)
                .send();
            match response {
                Ok(response) if response.status().is_success() => {
                    if response
                        .content_length()
                        .is_some_and(|length| length > 64 * 1024)
                    {
                        return StatusProbe::Rejected;
                    }
                    let mut body = Vec::new();
                    if response.take(64 * 1024 + 1).read_to_end(&mut body).is_err()
                        || body.len() > 64 * 1024
                        || serde_json::from_slice::<DaemonStatus>(&body).is_err()
                    {
                        StatusProbe::Rejected
                    } else {
                        StatusProbe::Authenticated
                    }
                }
                Ok(_) => StatusProbe::Rejected,
                Err(error) if error.is_connect() || error.is_timeout() => StatusProbe::Transient,
                Err(_) => StatusProbe::Fatal,
            }
        },
        std::thread::sleep,
    )
}

fn await_owned_readiness(
    mut owned: OwnedDaemon,
    deadline: Instant,
) -> Result<(OwnedDaemon, SocketAddr), BootstrapError> {
    let address = owned.readiness(deadline)?;
    Ok((owned, address))
}

fn await_owned_status_with(
    mut owned: OwnedDaemon,
    deadline: Instant,
    probe: impl FnMut(Duration) -> StatusProbe,
    pause: impl FnMut(Duration),
) -> Result<OwnedDaemon, BootstrapError> {
    await_authenticated_status(deadline, || owned.has_exited(), probe, pause)?;
    Ok(owned)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusProbe {
    Authenticated,
    Transient,
    Rejected,
    Fatal,
}

fn await_authenticated_status(
    deadline: Instant,
    mut has_exited: impl FnMut() -> Result<bool, BootstrapError>,
    mut probe: impl FnMut(Duration) -> StatusProbe,
    mut pause: impl FnMut(Duration),
) -> Result<(), BootstrapError> {
    loop {
        if has_exited()? {
            return Err(BootstrapError::Startup);
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(BootstrapError::Startup)?;
        match probe(remaining) {
            StatusProbe::Authenticated => return Ok(()),
            StatusProbe::Rejected => return Err(BootstrapError::Authentication),
            StatusProbe::Fatal => return Err(BootstrapError::Startup),
            StatusProbe::Transient => {
                let remaining = deadline
                    .checked_duration_since(Instant::now())
                    .ok_or(BootstrapError::Startup)?;
                pause(RETRY_DELAY.min(remaining));
            }
        }
    }
}

#[cfg(windows)]
mod windows_bootstrap {
    use super::*;
    use std::{
        mem::{size_of, zeroed},
        os::windows::{ffi::OsStrExt, io::FromRawHandle},
        ptr::{null, null_mut},
    };
    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, HANDLE_FLAG_INHERIT,
            INVALID_HANDLE_VALUE, SetHandleInformation, WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        Security::SECURITY_ATTRIBUTES,
        Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, OPEN_EXISTING},
        System::{
            JobObjects::{
                CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject, TerminateJobObject,
            },
            Pipes::CreatePipe,
            Threading::{
                CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
                DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, INFINITE,
                InitializeProcThreadAttributeList, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION, STARTF_USESTDHANDLES,
                STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
            },
        },
    };
    #[cfg(test)]
    use windows_sys::Win32::{
        Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle},
        System::Threading::GetCurrentProcess,
    };

    struct Handle(HANDLE);

    impl Handle {
        fn new(value: HANDLE) -> Result<Self, BootstrapError> {
            if value.is_null() || value == INVALID_HANDLE_VALUE {
                Err(BootstrapError::Startup)
            } else {
                Ok(Self(value))
            }
        }

        fn into_raw(self) -> HANDLE {
            let raw = self.0;
            std::mem::forget(self);
            raw
        }
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    struct AttributeList {
        buffer: Vec<usize>,
        initialized: bool,
    }

    impl AttributeList {
        fn new(count: u32) -> Result<Self, BootstrapError> {
            let mut bytes = 0;
            unsafe {
                InitializeProcThreadAttributeList(null_mut(), count, 0, &mut bytes);
            }
            if bytes == 0 {
                return Err(BootstrapError::Startup);
            }
            let words = bytes.div_ceil(size_of::<usize>());
            let mut list = Self {
                buffer: vec![0; words],
                initialized: false,
            };
            if unsafe { InitializeProcThreadAttributeList(list.as_ptr(), count, 0, &mut bytes) }
                == 0
            {
                return Err(BootstrapError::Startup);
            }
            list.initialized = true;
            Ok(list)
        }

        fn as_ptr(
            &mut self,
        ) -> windows_sys::Win32::System::Threading::LPPROC_THREAD_ATTRIBUTE_LIST {
            self.buffer.as_mut_ptr().cast()
        }

        fn update<T>(&mut self, attribute: usize, values: &[T]) -> Result<(), BootstrapError> {
            if unsafe {
                UpdateProcThreadAttribute(
                    self.as_ptr(),
                    0,
                    attribute,
                    values.as_ptr().cast(),
                    std::mem::size_of_val(values),
                    null_mut(),
                    null(),
                )
            } == 0
            {
                return Err(BootstrapError::Startup);
            }
            Ok(())
        }
    }

    impl Drop for AttributeList {
        fn drop(&mut self) {
            if self.initialized {
                unsafe {
                    DeleteProcThreadAttributeList(self.as_ptr());
                }
            }
        }
    }

    pub(super) struct OwnedDaemon {
        process: Handle,
        job: Handle,
        readiness: Option<ReadinessReceiver>,
        _package_locks: Vec<File>,
        cleaned: bool,
    }

    #[cfg(test)]
    pub(super) struct ProcessObserver(Handle);

    #[cfg(test)]
    impl ProcessObserver {
        pub(super) fn wait_terminated(&self, timeout: Duration) -> bool {
            let milliseconds = timeout.as_millis().min(u128::from(u32::MAX)) as u32;
            unsafe { WaitForSingleObject(self.0.0, milliseconds) == WAIT_OBJECT_0 }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum WaitOutcome {
        Signaled,
        Timeout,
        Failed,
    }

    pub(super) trait CleanupApi {
        fn terminate_job(&mut self) -> bool;
        fn wait_process(&mut self, milliseconds: u32) -> WaitOutcome;
        fn terminate_process(&mut self) -> bool;
    }

    pub(super) struct CleanupOutcome {
        pub(super) reaped: bool,
        pub(super) result: Result<(), BootstrapError>,
    }

    pub(super) fn cleanup_process(api: &mut impl CleanupApi) -> CleanupOutcome {
        let job_terminated = api.terminate_job();
        match api.wait_process(5_000) {
            WaitOutcome::Signaled => CleanupOutcome {
                reaped: true,
                result: job_terminated.then_some(()).ok_or(BootstrapError::Startup),
            },
            WaitOutcome::Failed => CleanupOutcome {
                reaped: false,
                result: Err(BootstrapError::Startup),
            },
            WaitOutcome::Timeout if !api.terminate_process() => CleanupOutcome {
                reaped: false,
                result: Err(BootstrapError::Startup),
            },
            WaitOutcome::Timeout => match api.wait_process(INFINITE) {
                WaitOutcome::Signaled => CleanupOutcome {
                    reaped: true,
                    result: job_terminated.then_some(()).ok_or(BootstrapError::Startup),
                },
                WaitOutcome::Timeout | WaitOutcome::Failed => CleanupOutcome {
                    reaped: false,
                    result: Err(BootstrapError::Startup),
                },
            },
        }
    }

    struct NativeCleanupApi {
        job: HANDLE,
        process: HANDLE,
    }

    impl CleanupApi for NativeCleanupApi {
        fn terminate_job(&mut self) -> bool {
            unsafe { TerminateJobObject(self.job, 1) != 0 }
        }

        fn wait_process(&mut self, milliseconds: u32) -> WaitOutcome {
            match unsafe { WaitForSingleObject(self.process, milliseconds) } {
                WAIT_OBJECT_0 => WaitOutcome::Signaled,
                WAIT_TIMEOUT => WaitOutcome::Timeout,
                _ => WaitOutcome::Failed,
            }
        }

        fn terminate_process(&mut self) -> bool {
            unsafe { TerminateProcess(self.process, 1) != 0 }
        }
    }

    impl OwnedDaemon {
        pub(super) fn has_exited(&mut self) -> Result<bool, BootstrapError> {
            match unsafe { WaitForSingleObject(self.process.0, 0) } {
                WAIT_OBJECT_0 => Ok(true),
                WAIT_TIMEOUT => Ok(false),
                _ => Err(BootstrapError::Startup),
            }
        }

        pub(super) fn readiness(
            &mut self,
            deadline: Instant,
        ) -> Result<SocketAddr, BootstrapError> {
            let receiver = self.readiness.take().ok_or(BootstrapError::Readiness)?;
            await_readiness_signal(receiver, deadline, || self.has_exited())
        }

        #[cfg(test)]
        pub(super) fn observe_process(&self) -> Result<ProcessObserver, BootstrapError> {
            let current = unsafe { GetCurrentProcess() };
            let mut duplicate = null_mut();
            if unsafe {
                DuplicateHandle(
                    current,
                    self.process.0,
                    current,
                    &mut duplicate,
                    0,
                    0,
                    DUPLICATE_SAME_ACCESS,
                )
            } == 0
            {
                return Err(BootstrapError::Startup);
            }
            Ok(ProcessObserver(Handle::new(duplicate)?))
        }

        pub(super) fn terminate_and_reap(&mut self) -> Result<(), BootstrapError> {
            if self.cleaned {
                return Ok(());
            }
            let mut api = NativeCleanupApi {
                job: self.job.0,
                process: self.process.0,
            };
            let outcome = cleanup_process(&mut api);
            if outcome.reaped {
                self.cleaned = true;
            }
            outcome.result
        }
    }

    impl Drop for OwnedDaemon {
        fn drop(&mut self) {
            let _ = self.terminate_and_reap();
        }
    }

    pub(super) fn spawn(paths: PackagePaths, token: &str) -> Result<OwnedDaemon, BootstrapError> {
        spawn_with_args(paths, token, &[])
    }

    #[cfg(test)]
    pub(super) fn spawn_for_test(
        paths: PackagePaths,
        token: &str,
        args: &[OsString],
    ) -> Result<OwnedDaemon, BootstrapError> {
        spawn_with_args(paths, token, args)
    }

    fn spawn_with_args(
        paths: PackagePaths,
        token: &str,
        args: &[OsString],
    ) -> Result<OwnedDaemon, BootstrapError> {
        let job = Handle::new(unsafe { CreateJobObjectW(null(), null()) })?;
        if unsafe { SetHandleInformation(job.0, HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(BootstrapError::Startup);
        }
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(BootstrapError::Startup);
        }

        let mut security = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: null_mut(),
            bInheritHandle: 1,
        };
        let mut stdout_read = null_mut();
        let mut stdout_write = null_mut();
        if unsafe { CreatePipe(&mut stdout_read, &mut stdout_write, &raw mut security, 0) } == 0 {
            return Err(BootstrapError::Startup);
        }
        let stdout_read = Handle::new(stdout_read)?;
        let stdout_write = Handle::new(stdout_write)?;
        if unsafe { SetHandleInformation(stdout_read.0, HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(BootstrapError::Startup);
        }

        let nul_name: Vec<u16> = OsStr::new("NUL").encode_wide().chain(Some(0)).collect();
        let nul = Handle::new(unsafe {
            CreateFileW(
                nul_name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                &raw mut security,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        })?;

        let inherited = [stdout_write.0, nul.0];
        // Attribute-list values are borrowed until CreateProcessW returns.
        let jobs = [job.0];
        let mut attributes = AttributeList::new(2)?;
        attributes.update(PROC_THREAD_ATTRIBUTE_JOB_LIST as usize, &jobs)?;
        attributes.update(PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize, &inherited)?;

        let application = wide_nul(paths.daemon.as_os_str());
        let mut command_line = quote_windows_argument(paths.daemon.as_os_str());
        for argument in args {
            command_line.push(b' ' as u16);
            command_line.extend(quote_windows_argument(argument));
        }
        command_line.push(0);
        let current_directory = wide_nul(paths.package_root.as_os_str());
        let mut environment = environment_block(&paths, token);
        let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = nul.0;
        startup.StartupInfo.hStdOutput = stdout_write.0;
        startup.StartupInfo.hStdError = nul.0;
        startup.lpAttributeList = attributes.as_ptr();
        let mut information: PROCESS_INFORMATION = unsafe { zeroed() };
        let created = unsafe {
            CreateProcessW(
                application.as_ptr(),
                command_line.as_mut_ptr(),
                null(),
                null(),
                1,
                CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
                environment.as_mut_ptr().cast(),
                current_directory.as_ptr(),
                (&raw const startup.StartupInfo),
                &mut information,
            )
        };
        if created == 0 {
            eprintln!(
                "bootstrap process-create error: {}",
                std::io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or_default()
            );
            return Err(BootstrapError::Startup);
        }
        // A successful CreateProcessW contractually returns both handles.
        let process = Handle(information.hProcess);
        let _thread = Handle(information.hThread);
        drop(stdout_write);
        drop(nul);
        drop(attributes);

        let raw_read = stdout_read.into_raw();
        let pipe = unsafe { File::from_raw_handle(raw_read) };
        let receiver = spawn_readiness_reader(pipe)?;
        let owned = OwnedDaemon {
            process,
            job,
            readiness: Some(receiver),
            _package_locks: paths.locks,
            cleaned: false,
        };
        Ok(owned)
    }

    fn wide_nul(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(Some(0)).collect()
    }

    fn quote_windows_argument(value: &OsStr) -> Vec<u16> {
        let mut result = vec![b'"' as u16];
        let mut backslashes = 0;
        for character in value.encode_wide() {
            if character == b'\\' as u16 {
                backslashes += 1;
            } else {
                if character == b'"' as u16 {
                    result.extend(std::iter::repeat_n(b'\\' as u16, backslashes + 1));
                }
                result.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
                backslashes = 0;
                result.push(character);
            }
        }
        result.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
        result.push(b'"' as u16);
        result
    }

    fn environment_block(paths: &PackagePaths, token: &str) -> Vec<u16> {
        let mut values: Vec<(OsString, OsString)> = std::env::vars_os()
            .filter(|(name, _)| {
                name.to_str().is_none_or(|name| {
                    !DAEMON_ENVIRONMENT_KEYS
                        .iter()
                        .any(|candidate| name.eq_ignore_ascii_case(candidate))
                })
            })
            .collect();
        values.extend(daemon_environment(paths, token));
        values.sort_by(|left, right| {
            left.0
                .to_string_lossy()
                .to_ascii_uppercase()
                .cmp(&right.0.to_string_lossy().to_ascii_uppercase())
        });
        let mut block = Vec::new();
        for (name, value) in values {
            block.extend(name.encode_wide());
            block.push(b'=' as u16);
            block.extend(value.encode_wide());
            block.push(0);
        }
        block.push(0);
        block
    }
}

#[cfg(not(windows))]
mod unix_bootstrap {
    use super::*;
    use rustix::process::{Pid, Signal, getpid, getppid, kill_process_group};
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};

    const TERMINATION_GRACE: Duration = Duration::from_secs(2);
    const REAP_POLL: Duration = Duration::from_millis(20);

    pub(super) struct OwnedDaemon {
        child: Child,
        readiness: Option<ReadinessReceiver>,
        _package_locks: Vec<File>,
        cleaned: bool,
    }

    impl OwnedDaemon {
        pub(super) fn has_exited(&mut self) -> Result<bool, BootstrapError> {
            self.child
                .try_wait()
                .map(|status| status.is_some())
                .map_err(|_| BootstrapError::Startup)
        }

        pub(super) fn readiness(
            &mut self,
            deadline: Instant,
        ) -> Result<SocketAddr, BootstrapError> {
            let receiver = self.readiness.take().ok_or(BootstrapError::Readiness)?;
            await_readiness_signal(receiver, deadline, || self.has_exited())
        }

        #[cfg(test)]
        pub(super) fn pid(&self) -> i32 {
            self.child.id() as i32
        }

        pub(super) fn terminate_and_reap(&mut self) -> Result<(), BootstrapError> {
            if self.cleaned {
                return Ok(());
            }
            let group = Pid::from_child(&self.child);
            let _ = kill_process_group(group, Signal::TERM);
            let deadline = Instant::now() + TERMINATION_GRACE;
            let reaped = loop {
                match self.child.try_wait() {
                    Ok(Some(_)) => break true,
                    Ok(None) if Instant::now() < deadline => std::thread::sleep(REAP_POLL),
                    Ok(None) => break false,
                    Err(_) => return Err(BootstrapError::Startup),
                }
            };
            let _ = kill_process_group(group, Signal::KILL);
            if !reaped {
                self.child.wait().map_err(|_| BootstrapError::Startup)?;
            }
            self.cleaned = true;
            Ok(())
        }
    }

    impl Drop for OwnedDaemon {
        fn drop(&mut self) {
            let _ = self.terminate_and_reap();
        }
    }

    pub(super) fn spawn(paths: PackagePaths, token: &str) -> Result<OwnedDaemon, BootstrapError> {
        spawn_with_args(paths, token, &[])
    }

    #[cfg(test)]
    pub(super) fn spawn_for_test(
        paths: PackagePaths,
        token: &str,
        args: &[OsString],
    ) -> Result<OwnedDaemon, BootstrapError> {
        spawn_with_args(paths, token, args)
    }

    fn spawn_with_args(
        paths: PackagePaths,
        token: &str,
        args: &[OsString],
    ) -> Result<OwnedDaemon, BootstrapError> {
        let parent = getpid();
        let mut command = Command::new(&paths.daemon);
        command
            .args(args)
            .current_dir(&paths.package_root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0);
        for key in DAEMON_ENVIRONMENT_KEYS {
            command.env_remove(key);
        }
        command.envs(daemon_environment(&paths, token));
        unsafe {
            command.pre_exec(move || {
                // macOS has no parent-death signal; there the daemon outlives a crashed
                // desktop until its own idle shutdown.
                #[cfg(target_os = "linux")]
                rustix::process::set_parent_process_death_signal(Some(Signal::TERM))?;
                if getppid() != Some(parent) {
                    return Err(std::io::Error::other("desktop exited before daemon start"));
                }
                Ok(())
            });
        }
        let mut child = command.spawn().map_err(|error| {
            eprintln!(
                "bootstrap process-create error: {}",
                error.raw_os_error().unwrap_or_default()
            );
            BootstrapError::Startup
        })?;
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(BootstrapError::Startup);
        };
        let mut owned = OwnedDaemon {
            child,
            readiness: None,
            _package_locks: paths.locks,
            cleaned: false,
        };
        owned.readiness = Some(spawn_readiness_reader(stdout)?);
        Ok(owned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::OsString, fs, net::SocketAddr, path::Path};

    struct DisconnectProbe {
        delay: Duration,
        calls: std::sync::atomic::AtomicUsize,
        shutdowns: std::sync::atomic::AtomicUsize,
    }

    impl DisconnectProbe {
        fn new(delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                delay,
                calls: std::sync::atomic::AtomicUsize::new(0),
                shutdowns: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn unsupported<T>() -> Result<T, crate::daemon::DaemonError> {
            Err(crate::daemon::DaemonError::new("unsupported in probe"))
        }
    }

    impl DaemonClient for DisconnectProbe {
        fn status(&self) -> Result<DaemonStatus, crate::daemon::DaemonError> {
            Self::unsupported()
        }
        fn import_subscription(
            &self,
            _url: &str,
        ) -> Result<DaemonStatus, crate::daemon::DaemonError> {
            Self::unsupported()
        }
        fn connect(&self) -> Result<DaemonStatus, crate::daemon::DaemonError> {
            Self::unsupported()
        }
        fn shutdown(&self) -> Result<(), crate::daemon::DaemonError> {
            self.shutdowns
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        fn disconnect(&self) -> Result<DaemonStatus, crate::daemon::DaemonError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::thread::sleep(self.delay);
            // The probe cannot build a status; reaching here is what matters.
            Self::unsupported()
        }
        fn events(
            &self,
            _after: u64,
            _known_epoch: Option<&str>,
        ) -> Result<crate::daemon::DaemonEventBatch, crate::daemon::DaemonError> {
            Self::unsupported()
        }
        fn catalog(&self) -> Result<crate::daemon::DaemonCatalog, crate::daemon::DaemonError> {
            Self::unsupported()
        }
        fn select_node(
            &self,
            _group_id: &str,
            _revision: u64,
            _node_id: &str,
        ) -> Result<crate::daemon::DaemonCatalog, crate::daemon::DaemonError> {
            Self::unsupported()
        }
    }

    #[test]
    fn exit_disconnects_an_owned_daemon_but_not_an_external_one() {
        let probe = DisconnectProbe::new(Duration::ZERO);
        disconnect_before_shutdown(false, probe.clone(), Duration::from_secs(1));
        assert_eq!(probe.calls(), 0);
        disconnect_before_shutdown(true, probe.clone(), Duration::from_secs(1));
        assert_eq!(probe.calls(), 1);
        assert_eq!(
            probe.shutdowns.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "Quit asks the daemon to release the service's cores after disconnecting"
        );

        let external = DisconnectProbe::new(Duration::ZERO);
        drop(Bootstrap {
            client: external.clone(),
            _owned: None,
        });
        assert_eq!(external.calls(), 0);
    }

    #[test]
    fn exit_disconnect_is_bounded_when_the_daemon_hangs() {
        let probe = DisconnectProbe::new(Duration::from_secs(10));
        let started = Instant::now();
        assert!(!disconnect_before_shutdown(
            true,
            probe.clone(),
            Duration::from_millis(100)
        ));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    fn external(
        url: Option<&str>,
        token: Option<&str>,
    ) -> Result<StartupSelection, BootstrapError> {
        select_startup(
            url.map(OsString::from).as_deref(),
            token.map(OsString::from).as_deref(),
        )
    }

    #[test]
    fn only_two_present_nonempty_unicode_values_select_external_mode() {
        assert_eq!(external(None, None).unwrap(), StartupSelection::Packaged);
        assert!(matches!(
            external(Some("http://127.0.0.1:8787"), Some("token")).unwrap(),
            StartupSelection::External { .. }
        ));
        for pair in [
            (Some(""), None),
            (None, Some("")),
            (Some("url"), None),
            (None, Some("token")),
            (Some(""), Some("token")),
            (Some("url"), Some("")),
            (Some(""), Some("")),
        ] {
            assert!(external(pair.0, pair.1).is_err(), "pair: {pair:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn non_unicode_override_is_rejected() {
        use std::os::windows::ffi::OsStringExt;
        let invalid = OsString::from_wide(&[0xd800]);
        assert!(select_startup(Some(&invalid), Some(&OsString::from("token"))).is_err());
        assert!(select_startup(Some(&OsString::from("url")), Some(&invalid)).is_err());
    }

    #[test]
    fn token_is_32_random_bytes_encoded_as_lower_hex() {
        let first = generate_token().unwrap();
        let second = generate_token().unwrap();
        assert_eq!(first.len(), 64);
        assert!(
            first
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
        assert_ne!(first, second);
    }

    #[test]
    fn readiness_accepts_only_one_exact_bounded_ascii_loopback_line() {
        assert_eq!(
            parse_readiness(b"MULTICORE_READY 127.0.0.1:32100\n").unwrap(),
            "127.0.0.1:32100".parse::<SocketAddr>().unwrap()
        );
        for line in [
            b"MULTICORE_READY 127.0.0.1:0\n".as_slice(),
            b"MULTICORE_READY 127.0.0.2:80\n",
            b"MULTICORE_READY 192.0.2.1:80\n",
            b" MULTICORE_READY 127.0.0.1:1\n",
            b"MULTICORE_READY localhost:1\n",
            b"MULTICORE_READY 127.0.0.1:1\r\n",
            b"MULTICORE_READY 127.0.0.1:1\nextra",
            b"MULTICORE_READY 127.0.0.1:1",
            b"MULTICORE_READY 127.0.0.1:1\xff\n",
        ] {
            assert!(parse_readiness(line).is_err(), "line: {line:?}");
        }
        assert!(parse_readiness(&[b'A'; READINESS_LIMIT + 1]).is_err());
    }

    #[test]
    fn authenticated_status_retries_only_transient_failures() {
        let mut probes = 0;
        let result = await_authenticated_status(
            Instant::now() + Duration::from_secs(1),
            || Ok(false),
            |_| {
                probes += 1;
                if probes == 1 {
                    StatusProbe::Transient
                } else {
                    StatusProbe::Authenticated
                }
            },
            |_| {},
        );
        assert_eq!(result, Ok(()));
        assert_eq!(probes, 2);
    }

    #[test]
    fn authentication_rejection_and_child_exit_fail_fast() {
        let mut rejected_probes = 0;
        assert_eq!(
            await_authenticated_status(
                Instant::now() + Duration::from_secs(1),
                || Ok(false),
                |_| {
                    rejected_probes += 1;
                    StatusProbe::Rejected
                },
                |_| panic!("authentication rejection must not pause"),
            ),
            Err(BootstrapError::Authentication)
        );
        assert_eq!(rejected_probes, 1);

        let mut early_exit_probes = 0;
        assert_eq!(
            await_authenticated_status(
                Instant::now() + Duration::from_secs(1),
                || Ok(true),
                |_| {
                    early_exit_probes += 1;
                    StatusProbe::Authenticated
                },
                |_| {},
            ),
            Err(BootstrapError::Startup)
        );
        assert_eq!(early_exit_probes, 0);
    }

    #[test]
    fn transient_retries_share_one_total_monotonic_deadline() {
        let started = Instant::now();
        let deadline = started + Duration::from_millis(30);
        let result = await_authenticated_status(
            deadline,
            || Ok(false),
            |_| StatusProbe::Transient,
            std::thread::sleep,
        );
        assert_eq!(result, Err(BootstrapError::Startup));
        assert!(started.elapsed() < Duration::from_millis(250));
    }

    #[test]
    fn every_user_visible_error_is_fixed_and_sanitized() {
        for error in [
            BootstrapError::Configuration,
            BootstrapError::Package,
            BootstrapError::Random,
            BootstrapError::Startup,
            BootstrapError::Readiness,
            BootstrapError::Authentication,
        ] {
            let message = error.to_string();
            assert!(!message.contains("secret-marker"));
            assert!(!message.contains(r"C:\Users\Private"));
            assert!(
                message
                    .chars()
                    .any(|character| ('А'..='я').contains(&character))
            );
        }
    }

    /// COFF machine and optional-header magic of this build and of the other Windows flavour.
    #[cfg(windows)]
    const NATIVE: (u16, u16) = (
        multicore_release::WindowsArch::CURRENT.pe_machine(),
        multicore_release::WindowsArch::CURRENT.pe_optional_magic(),
    );
    #[cfg(windows)]
    const FOREIGN: (u16, u16) = match multicore_release::WindowsArch::CURRENT {
        multicore_release::WindowsArch::X64 => (0x014c, 0x10b),
        multicore_release::WindowsArch::X86 => (0x8664, 0x20b),
        // x64 images would run emulated on Windows on ARM, but an ARM64 build takes ARM64 only.
        multicore_release::WindowsArch::Arm64 => (0x8664, 0x20b),
    };

    #[cfg(windows)]
    fn write_test_pe(path: &Path, machine: u16, magic: u16, characteristics: u16) {
        let mut image = vec![0_u8; 0x200];
        image[0..2].copy_from_slice(b"MZ");
        image[0x3c..0x40].copy_from_slice(&(0x80_u32).to_le_bytes());
        image[0x80..0x84].copy_from_slice(b"PE\0\0");
        image[0x84..0x86].copy_from_slice(&machine.to_le_bytes());
        image[0x86..0x88].copy_from_slice(&1_u16.to_le_bytes());
        image[0x94..0x96].copy_from_slice(&0xf0_u16.to_le_bytes());
        image[0x96..0x98].copy_from_slice(&characteristics.to_le_bytes());
        image[0x98..0x9a].copy_from_slice(&magic.to_le_bytes());
        image[0xd4..0xd8].copy_from_slice(&0x1b0_u32.to_le_bytes());
        fs::write(path, image).unwrap();
    }

    #[cfg(windows)]
    fn mutate_test_pe(path: &Path, range: std::ops::Range<usize>, bytes: &[u8]) {
        let mut image = fs::read(path).unwrap();
        image[range].copy_from_slice(bytes);
        fs::write(path, image).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn pe_validation_rejects_incoherent_or_unbounded_headers_and_sections() {
        const FIXED: u16 = multicore_release::WindowsArch::CURRENT.pe_optional_header_fixed_size();
        const SHORT_OPTIONAL_HEADER: u16 = FIXED - 1;
        // NumberOfRvaAndSizes closes the fixed optional header, which starts at 0x98.
        const DATA_DIRECTORY_COUNT: usize = 0x98 + FIXED as usize - 4;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("fixture.exe");
        let cases: &[(&str, std::ops::Range<usize>, &[u8])] = &[
            ("zero sections", 0x86..0x88, &0_u16.to_le_bytes()),
            ("not executable", 0x96..0x98, &0x20_u16.to_le_bytes()),
            (
                "short optional header",
                0x94..0x96,
                &(SHORT_OPTIONAL_HEADER).to_le_bytes(),
            ),
            (
                "optional header outside file",
                0x94..0x96,
                &0x1000_u16.to_le_bytes(),
            ),
            (
                "section table outside file",
                0x86..0x88,
                &96_u16.to_le_bytes(),
            ),
            (
                "headers before section table",
                0xd4..0xd8,
                &0x100_u32.to_le_bytes(),
            ),
            (
                "headers outside file",
                0xd4..0xd8,
                &0x1000_u32.to_le_bytes(),
            ),
            (
                "too many data directories",
                DATA_DIRECTORY_COUNT..DATA_DIRECTORY_COUNT + 4,
                &17_u32.to_le_bytes(),
            ),
        ];
        for (name, range, mutation) in cases {
            write_test_pe(&path, NATIVE.0, NATIVE.1, 0x0022);
            mutate_test_pe(&path, range.clone(), mutation);
            let mut file = File::open(&path).unwrap();
            assert!(validate_native_pe(&mut file).is_err(), "case: {name}");
        }

        write_test_pe(&path, NATIVE.0, NATIVE.1, 0x0022);
        mutate_test_pe(&path, 0x198..0x19c, &0x80_u32.to_le_bytes());
        mutate_test_pe(&path, 0x19c..0x1a0, &0x1f0_u32.to_le_bytes());
        let mut file = File::open(&path).unwrap();
        assert!(
            validate_native_pe(&mut file).is_err(),
            "section raw range must remain within the file"
        );

        let current_exe = std::env::current_exe().unwrap();
        let mut current = File::open(current_exe).unwrap();
        validate_native_pe(&mut current).expect("the native test PE must remain accepted");
    }

    #[cfg(windows)]
    #[test]
    fn packaged_paths_support_spaces_and_require_contained_amd64_pe32_plus_non_dll_files() {
        let temp = tempfile::tempdir().unwrap();
        let package = temp.path().join("portable package with spaces");
        let runtime = package.join("runtime");
        let cores = package.join("cores");
        fs::create_dir_all(&runtime).unwrap();
        fs::create_dir_all(&cores).unwrap();
        let desktop = package.join("MultiCore.exe");
        write_test_pe(&desktop, NATIVE.0, NATIVE.1, 0x0022);
        for path in [
            runtime.join("multicore-daemon.exe"),
            runtime.join("multicore-updater.exe"),
            cores.join("xray.exe"),
            cores.join("mihomo.exe"),
        ] {
            write_test_pe(&path, NATIVE.0, NATIVE.1, 0x0022);
        }
        let paths = derive_and_validate_paths(
            &desktop,
            temp.path().join("Local App Data").join("MultiCore"),
        )
        .unwrap();
        assert_eq!(paths.package_root, package);
        assert_eq!(
            paths.data_dir,
            temp.path().join("Local App Data").join("MultiCore")
        );
        let xray = paths.xray.clone();
        drop(paths);

        let updater = runtime.join("multicore-updater.exe");
        fs::remove_file(&updater).unwrap();
        assert!(derive_and_validate_paths(&desktop, temp.path()).is_err());
        write_test_pe(&updater, NATIVE.0, NATIVE.1, 0x0022);

        write_test_pe(&xray, FOREIGN.0, FOREIGN.1, 0x0022);
        assert!(derive_and_validate_paths(&desktop, temp.path()).is_err());
        write_test_pe(&xray, NATIVE.0, NATIVE.1, 0x2022);
        assert!(derive_and_validate_paths(&desktop, temp.path()).is_err());
        fs::remove_file(&xray).unwrap();
        fs::create_dir(&xray).unwrap();
        assert!(derive_and_validate_paths(&desktop, temp.path()).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn packaged_validation_rejects_reparse_points_and_package_escape() {
        use std::os::windows::fs::symlink_file;
        let temp = tempfile::tempdir().unwrap();
        let package = temp.path().join("package");
        fs::create_dir_all(package.join("runtime")).unwrap();
        fs::create_dir_all(package.join("cores")).unwrap();
        let desktop = package.join("MultiCore.exe");
        write_test_pe(&desktop, NATIVE.0, NATIVE.1, 0x0022);
        let outside = temp.path().join("outside.exe");
        write_test_pe(&outside, NATIVE.0, NATIVE.1, 0x0022);
        if symlink_file(&outside, package.join("runtime/multicore-daemon.exe")).is_err() {
            // Creating symlinks requires Developer Mode or elevation on some Windows hosts.
            return;
        }
        write_test_pe(&package.join("cores/xray.exe"), NATIVE.0, NATIVE.1, 0x0022);
        write_test_pe(
            &package.join("cores/mihomo.exe"),
            NATIVE.0,
            NATIVE.1,
            0x0022,
        );
        assert!(derive_and_validate_paths(&desktop, temp.path()).is_err());
    }

    #[cfg(windows)]
    fn spawned_test_paths() -> PackagePaths {
        let daemon = std::env::current_exe().unwrap();
        let package_root = daemon.parent().unwrap().to_path_buf();
        PackagePaths {
            package_root,
            daemon,
            xray: PathBuf::from(r"C:\fixture\xray.exe"),
            mihomo: PathBuf::from(r"C:\fixture\mihomo.exe"),
            data_dir: std::env::temp_dir().join("multicore-bootstrap-child"),
            locks: Vec::new(),
        }
    }

    #[cfg(windows)]
    #[test]
    fn native_spawn_detects_early_exit_and_owned_cleanup_reaps_a_live_child() {
        let early_args = [
            OsString::from("--ignored"),
            OsString::from("--exact"),
            OsString::from("bootstrap::tests::bootstrap_exiting_child_helper"),
        ];
        let mut early =
            windows_bootstrap::spawn_for_test(spawned_test_paths(), "test-only-token", &early_args)
                .unwrap();
        assert!(
            early
                .readiness(Instant::now() + Duration::from_secs(2))
                .is_err()
        );
        early.terminate_and_reap().unwrap();
        assert!(early.has_exited().unwrap());

        let live_args = [
            OsString::from("--ignored"),
            OsString::from("--exact"),
            OsString::from("bootstrap::tests::bootstrap_sleeping_child_helper"),
        ];
        let mut live =
            windows_bootstrap::spawn_for_test(spawned_test_paths(), "test-only-token", &live_args)
                .unwrap();
        live.terminate_and_reap().unwrap();
        assert!(live.has_exited().unwrap());
    }

    #[cfg(windows)]
    fn quiet_sleeping_child_paths() -> (PackagePaths, Vec<OsString>) {
        let system_root = std::env::var_os("SystemRoot").unwrap();
        let daemon = PathBuf::from(system_root)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        let package_root = daemon.parent().unwrap().to_path_buf();
        (
            PackagePaths {
                package_root,
                daemon,
                xray: PathBuf::from(r"C:\fixture\xray.exe"),
                mihomo: PathBuf::from(r"C:\fixture\mihomo.exe"),
                data_dir: std::env::temp_dir().join("multicore-bootstrap-child"),
                locks: Vec::new(),
            },
            [
                OsString::from("-NoProfile"),
                OsString::from("-NonInteractive"),
                OsString::from("-Command"),
                OsString::from("Start-Sleep -Seconds 60"),
            ]
            .into(),
        )
    }

    #[cfg(windows)]
    #[test]
    fn owned_process_is_automatically_reaped_on_normal_drop_and_startup_failures() {
        let (paths, args) = quiet_sleeping_child_paths();
        let normal = windows_bootstrap::spawn_for_test(paths, "test-only-token", &args).unwrap();
        let normal_observer = normal.observe_process().unwrap();
        drop(normal);
        assert!(normal_observer.wait_terminated(Duration::from_secs(2)));

        let rejection_args = [
            OsString::from("--ignored"),
            OsString::from("--exact"),
            OsString::from("bootstrap::tests::bootstrap_sleeping_child_helper"),
        ];
        let rejection = windows_bootstrap::spawn_for_test(
            spawned_test_paths(),
            "test-only-token",
            &rejection_args,
        )
        .unwrap();
        let rejection_observer = rejection.observe_process().unwrap();
        assert_eq!(
            await_owned_readiness(rejection, Instant::now() + Duration::from_secs(2)).map(|_| ()),
            Err(BootstrapError::Readiness)
        );
        assert!(rejection_observer.wait_terminated(Duration::from_secs(2)));

        let (paths, args) = quiet_sleeping_child_paths();
        let timeout = windows_bootstrap::spawn_for_test(paths, "test-only-token", &args).unwrap();
        let timeout_observer = timeout.observe_process().unwrap();
        assert_eq!(
            await_owned_readiness(timeout, Instant::now() + Duration::from_millis(30)).map(|_| ()),
            Err(BootstrapError::Startup)
        );
        assert!(timeout_observer.wait_terminated(Duration::from_secs(2)));

        let (paths, args) = quiet_sleeping_child_paths();
        let authentication =
            windows_bootstrap::spawn_for_test(paths, "test-only-token", &args).unwrap();
        let authentication_observer = authentication.observe_process().unwrap();
        assert_eq!(
            await_owned_status_with(
                authentication,
                Instant::now() + Duration::from_secs(1),
                |_| StatusProbe::Rejected,
                |_| {},
            )
            .map(|_| ()),
            Err(BootstrapError::Authentication)
        );
        assert!(authentication_observer.wait_terminated(Duration::from_secs(2)));
    }

    #[cfg(windows)]
    struct FakeCleanupApi {
        terminate_job: bool,
        waits: std::collections::VecDeque<windows_bootstrap::WaitOutcome>,
        terminate_process: bool,
        calls: Vec<&'static str>,
    }

    #[cfg(windows)]
    impl windows_bootstrap::CleanupApi for FakeCleanupApi {
        fn terminate_job(&mut self) -> bool {
            self.calls.push("terminate_job");
            self.terminate_job
        }

        fn wait_process(&mut self, _milliseconds: u32) -> windows_bootstrap::WaitOutcome {
            self.calls.push("wait_process");
            self.waits.pop_front().unwrap()
        }

        fn terminate_process(&mut self) -> bool {
            self.calls.push("terminate_process");
            self.terminate_process
        }
    }

    #[cfg(windows)]
    #[test]
    fn cleanup_decision_checks_every_native_result_and_marks_only_confirmed_reaping() {
        use windows_bootstrap::WaitOutcome::{Failed, Signaled, Timeout};

        let cases = [
            (
                "job termination and first wait confirmed reaping",
                true,
                vec![Signaled],
                true,
                true,
                Ok(()),
                vec!["terminate_job", "wait_process"],
            ),
            (
                "job termination failed but process is signaled",
                false,
                vec![Signaled],
                true,
                true,
                Err(BootstrapError::Startup),
                vec!["terminate_job", "wait_process"],
            ),
            (
                "first wait failed",
                true,
                vec![Failed],
                true,
                false,
                Err(BootstrapError::Startup),
                vec!["terminate_job", "wait_process"],
            ),
            (
                "fallback process termination failed",
                true,
                vec![Timeout],
                false,
                false,
                Err(BootstrapError::Startup),
                vec!["terminate_job", "wait_process", "terminate_process"],
            ),
            (
                "fallback wait failed",
                true,
                vec![Timeout, Failed],
                true,
                false,
                Err(BootstrapError::Startup),
                vec![
                    "terminate_job",
                    "wait_process",
                    "terminate_process",
                    "wait_process",
                ],
            ),
            (
                "fallback confirmed reaping",
                true,
                vec![Timeout, Signaled],
                true,
                true,
                Ok(()),
                vec![
                    "terminate_job",
                    "wait_process",
                    "terminate_process",
                    "wait_process",
                ],
            ),
            (
                "job failure remains reported after fallback reaping",
                false,
                vec![Timeout, Signaled],
                true,
                true,
                Err(BootstrapError::Startup),
                vec![
                    "terminate_job",
                    "wait_process",
                    "terminate_process",
                    "wait_process",
                ],
            ),
        ];

        for (name, terminate_job, waits, terminate_process, reaped, result, calls) in cases {
            let mut api = FakeCleanupApi {
                terminate_job,
                waits: waits.into(),
                terminate_process,
                calls: Vec::new(),
            };
            let outcome = windows_bootstrap::cleanup_process(&mut api);
            assert_eq!(outcome.reaped, reaped, "case: {name}");
            assert_eq!(outcome.result, result, "case: {name}");
            assert_eq!(api.calls, calls, "case: {name}");
        }
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "isolated helper invoked by native_spawn_detects_early_exit_and_owned_cleanup_reaps_a_live_child"]
    fn bootstrap_exiting_child_helper() {}

    #[cfg(windows)]
    #[test]
    #[ignore = "isolated helper invoked by native_spawn_detects_early_exit_and_owned_cleanup_reaps_a_live_child"]
    fn bootstrap_sleeping_child_helper() {
        std::thread::sleep(Duration::from_secs(60));
    }

    #[cfg(not(windows))]
    use multicore_platform::elf::test_support::TestElf;

    #[cfg(not(windows))]
    fn linux_package(root: &Path) -> (PathBuf, Vec<PathBuf>) {
        let package = root.join("package with spaces");
        let runtime = package.join("runtime");
        let cores = package.join("cores");
        fs::create_dir_all(&runtime).unwrap();
        fs::create_dir_all(&cores).unwrap();
        let desktop = package.join("multicore");
        TestElf::position_independent().write(&desktop);
        let members = vec![
            runtime.join("multicore-daemon"),
            runtime.join("multicore-core-host"),
            cores.join("xray"),
            cores.join("mihomo"),
        ];
        TestElf::position_independent().write(&members[0]);
        TestElf::position_independent().write(&members[1]);
        TestElf::executable().write(&members[2]);
        TestElf::executable().write(&members[3]);
        (desktop, members)
    }

    #[cfg(not(windows))]
    #[test]
    fn linux_package_layout_requires_every_contained_elf_executable() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let (desktop, members) = linux_package(temp.path());
        let data = temp.path().join("data").join("multicore");
        let paths = derive_and_validate_paths(&desktop, &data).unwrap();
        assert_eq!(paths.package_root, desktop.parent().unwrap());
        assert_eq!(paths.daemon, members[0]);
        assert_eq!(paths.xray, members[2]);
        assert_eq!(paths.mihomo, members[3]);
        assert_eq!(paths.data_dir, data);
        drop(paths);

        for member in &members {
            let original = fs::read(member).unwrap();
            fs::remove_file(member).unwrap();
            assert!(derive_and_validate_paths(&desktop, &data).is_err());
            fs::create_dir(member).unwrap();
            assert!(derive_and_validate_paths(&desktop, &data).is_err());
            fs::remove_dir(member).unwrap();
            fs::write(member, &original).unwrap();
            fs::set_permissions(member, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(derive_and_validate_paths(&desktop, &data).is_err());
            fs::set_permissions(member, fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(derive_and_validate_paths(&desktop, &data).is_ok());
    }

    #[cfg(not(windows))]
    #[test]
    fn linux_package_layout_rejects_symlinks_and_package_escape() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let (desktop, members) = linux_package(temp.path());
        let data = temp.path().join("data");
        let outside = temp.path().join("outside");
        TestElf::position_independent().write(&outside);
        fs::remove_file(&members[0]).unwrap();
        symlink(&outside, &members[0]).unwrap();
        assert!(derive_and_validate_paths(&desktop, &data).is_err());
        fs::remove_file(&members[0]).unwrap();
        TestElf::position_independent().write(&members[0]);

        let cores = desktop.parent().unwrap().join("cores");
        let moved = temp.path().join("moved-cores");
        fs::rename(&cores, &moved).unwrap();
        symlink(&moved, &cores).unwrap();
        assert!(derive_and_validate_paths(&desktop, &data).is_err());
    }

    #[cfg(not(windows))]
    fn shell_daemon_paths(root: &Path) -> PackagePaths {
        PackagePaths {
            package_root: root.to_path_buf(),
            daemon: PathBuf::from("/bin/sh"),
            xray: root.join("cores").join("xray"),
            mihomo: root.join("cores").join("mihomo"),
            data_dir: root.join("data"),
            locks: Vec::new(),
        }
    }

    #[cfg(not(windows))]
    fn shell_arguments(script: &str, extra: &Path) -> Vec<OsString> {
        vec![
            OsString::from("-c"),
            OsString::from(script),
            OsString::from("sh"),
            extra.as_os_str().to_owned(),
        ]
    }

    #[cfg(not(windows))]
    fn process_is_gone(pid: i32) -> bool {
        match fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => true,
            Ok(stat) => stat
                .rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
        }
    }

    #[cfg(not(windows))]
    fn eventually(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        condition()
    }

    #[cfg(not(windows))]
    #[test]
    fn linux_daemon_reports_readiness_and_owner_drop_terminates_its_group() {
        let temp = tempfile::tempdir().unwrap();
        let pid_file = temp.path().join("grandchild");
        let script = "sleep 60 & echo \"$!\" > \"$1\"; printf 'MULTICORE_READY 127.0.0.1:4567\\n'; exec sleep 60";
        let mut owned = unix_bootstrap::spawn_for_test(
            shell_daemon_paths(temp.path()),
            "token",
            &shell_arguments(script, &pid_file),
        )
        .unwrap();
        let address = owned
            .readiness(Instant::now() + Duration::from_secs(5))
            .unwrap();
        assert_eq!(address.port(), 4567);
        assert!(!owned.has_exited().unwrap());
        let daemon = owned.pid();
        let grandchild: i32 = fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        drop(owned);
        assert!(eventually(Duration::from_secs(3), || {
            process_is_gone(daemon) && process_is_gone(grandchild)
        }));
    }

    #[cfg(not(windows))]
    #[test]
    fn linux_daemon_ignoring_sigterm_is_killed_after_the_bounded_wait() {
        let temp = tempfile::tempdir().unwrap();
        let script =
            "trap '' TERM; printf 'MULTICORE_READY 127.0.0.1:4567\\n'; while :; do sleep 1; done";
        let mut owned = unix_bootstrap::spawn_for_test(
            shell_daemon_paths(temp.path()),
            "token",
            &shell_arguments(script, temp.path()),
        )
        .unwrap();
        owned
            .readiness(Instant::now() + Duration::from_secs(5))
            .unwrap();
        let daemon = owned.pid();
        let started = Instant::now();
        owned.terminate_and_reap().unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(process_is_gone(daemon));
    }

    #[cfg(not(windows))]
    #[test]
    fn linux_daemon_early_exit_fails_readiness() {
        let temp = tempfile::tempdir().unwrap();
        let mut owned = unix_bootstrap::spawn_for_test(
            shell_daemon_paths(temp.path()),
            "token",
            &shell_arguments("exit 3", temp.path()),
        )
        .unwrap();
        assert!(
            owned
                .readiness(Instant::now() + Duration::from_secs(5))
                .is_err()
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn linux_daemon_receives_the_bootstrap_environment() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("environment");
        let script = "printf '%s\\n' \"$MULTICORE_DAEMON_TOKEN\" \"$MULTICORE_DAEMON_READY_STDOUT\" \"$MULTICORE_DAEMON_ADDR\" \"$MULTICORE_DAEMON_DATA_DIR\" \"$MULTICORE_XRAY_BIN\" \"$MULTICORE_MIHOMO_BIN\" \"$MULTICORE_MIHOMO_CONTROLLER_ADDR\" \"$PWD\" > \"$1\"; printf 'MULTICORE_READY 127.0.0.1:4567\\n'; exec sleep 60";
        let paths = shell_daemon_paths(temp.path());
        let expected = [
            "secret-token".to_owned(),
            "1".to_owned(),
            "127.0.0.1:0".to_owned(),
            paths.data_dir.display().to_string(),
            paths.xray.display().to_string(),
            paths.mihomo.display().to_string(),
            "127.0.0.1:19090".to_owned(),
            temp.path().canonicalize().unwrap().display().to_string(),
        ];
        let mut owned = unix_bootstrap::spawn_for_test(
            paths,
            "secret-token",
            &shell_arguments(script, &output),
        )
        .unwrap();
        owned
            .readiness(Instant::now() + Duration::from_secs(5))
            .unwrap();
        let written = fs::read_to_string(&output).unwrap();
        assert_eq!(written.lines().collect::<Vec<_>>(), expected);
    }

    #[cfg(not(windows))]
    #[test]
    fn linux_daemon_dies_with_the_thread_that_spawned_it() {
        let temp = tempfile::tempdir().unwrap();
        let paths = shell_daemon_paths(temp.path());
        let arguments = shell_arguments("exec sleep 60", temp.path());
        let daemon = std::thread::spawn(move || {
            let owned = unix_bootstrap::spawn_for_test(paths, "token", &arguments).unwrap();
            let pid = owned.pid();
            std::mem::forget(owned);
            pid
        })
        .join()
        .unwrap();
        assert!(eventually(Duration::from_secs(3), || process_is_gone(
            daemon
        )));
    }
}
