use std::{
    ffi::{OsStr, OsString},
    fmt,
    fs::File,
    io::{Read, Seek, SeekFrom},
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use crate::daemon::{DaemonClient, DaemonStatus, HttpDaemonClient};

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
    #[cfg(not(windows))]
    Unsupported,
}

impl BootstrapError {
    pub(crate) fn user_message(&self) -> &'static str {
        match self {
            Self::Configuration => "Проверьте настройки подключения к фоновому сервису.",
            Self::Package => {
                "Файлы приложения повреждены или отсутствуют. Распакуйте пакет заново."
            }
            Self::Random => "Не удалось безопасно подготовить запуск фонового сервиса.",
            Self::Startup => "Не удалось запустить фоновый сервис.",
            Self::Readiness => "Фоновый сервис запустился некорректно.",
            Self::Authentication => "Фоновый сервис не прошёл проверку подлинности.",
            #[cfg(not(windows))]
            Self::Unsupported => "Автоматический запуск доступен только в Windows-пакете.",
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
    local_app_data: impl AsRef<Path>,
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
    let daemon = runtime_dir.join("multicore-daemon.exe");
    let updater = runtime_dir.join("multicore-updater.exe");
    let xray = cores_dir.join("xray.exe");
    let mihomo = cores_dir.join("mihomo.exe");
    locks.extend(
        [&daemon, &updater, &xray, &mihomo]
            .into_iter()
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
        data_dir: local_app_data.as_ref().join("MultiCore"),
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
    validate_amd64_pe32_plus(&mut file)?;
    Ok(file)
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

fn validate_amd64_pe32_plus(file: &mut File) -> Result<(), BootstrapError> {
    const MAX_PE_HEADER_BYTES: u64 = 1024 * 1024;
    const MIN_PE32_PLUS_OPTIONAL_HEADER: u16 = 112;
    const MAX_OPTIONAL_HEADER: u16 = 4096;
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
        || machine != 0x8664
        || section_count == 0
        || section_count > MAX_SECTIONS
        || characteristics & IMAGE_FILE_EXECUTABLE_IMAGE == 0
        || characteristics & IMAGE_FILE_DLL != 0
        || !(MIN_PE32_PLUS_OPTIONAL_HEADER..=MAX_OPTIONAL_HEADER).contains(&optional_size)
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
    let mut optional = [0_u8; MIN_PE32_PLUS_OPTIONAL_HEADER as usize];
    file.read_exact(&mut optional)
        .map_err(|_| BootstrapError::Package)?;
    let optional_magic = u16::from_le_bytes([optional[0], optional[1]]);
    let size_of_headers =
        u32::from_le_bytes(optional[60..64].try_into().expect("fixed slice")) as u64;
    let data_directory_count =
        u32::from_le_bytes(optional[108..112].try_into().expect("fixed slice"));
    let required_optional_size = 112_u64
        .checked_add(u64::from(data_directory_count) * 8)
        .ok_or(BootstrapError::Package)?;
    if optional_magic != 0x20b
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

#[cfg(not(windows))]
struct OwnedDaemon;

#[cfg(not(windows))]
fn start_packaged() -> Result<Bootstrap, BootstrapError> {
    Err(BootstrapError::Unsupported)
}

#[cfg(windows)]
use windows_bootstrap::OwnedDaemon;

#[cfg(windows)]
fn start_packaged() -> Result<Bootstrap, BootstrapError> {
    let current_exe = std::env::current_exe().map_err(|_| BootstrapError::Package)?;
    let local_app_data = std::env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .ok_or(BootstrapError::Package)?;
    let paths = derive_and_validate_paths(&current_exe, local_app_data)?;
    std::fs::create_dir_all(&paths.data_dir).map_err(|_| {
        eprintln!("bootstrap stage failed: data-directory");
        BootstrapError::Startup
    })?;
    let token = generate_token()?;
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let owned = windows_bootstrap::spawn(paths, &token).inspect_err(|_| {
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

#[cfg(windows)]
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

#[cfg(windows)]
fn await_owned_readiness(
    mut owned: OwnedDaemon,
    deadline: Instant,
) -> Result<(OwnedDaemon, SocketAddr), BootstrapError> {
    let address = owned.readiness(deadline)?;
    Ok((owned, address))
}

#[cfg(windows)]
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
        sync::mpsc,
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
        readiness: Option<mpsc::Receiver<Result<SocketAddr, BootstrapError>>>,
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
            loop {
                if self.has_exited()? {
                    return Err(BootstrapError::Startup);
                }
                let remaining = deadline
                    .checked_duration_since(Instant::now())
                    .ok_or(BootstrapError::Startup)?;
                match receiver.recv_timeout(Duration::from_millis(20).min(remaining)) {
                    Ok(result) => return result,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err(BootstrapError::Readiness);
                    }
                }
            }
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

        let (sender, receiver) = mpsc::sync_channel(1);
        let owned = OwnedDaemon {
            process,
            job,
            readiness: Some(receiver),
            _package_locks: paths.locks,
            cleaned: false,
        };
        let raw_read = stdout_read.into_raw();
        let pipe = unsafe { File::from_raw_handle(raw_read) };
        std::thread::Builder::new()
            .name("multicore-readiness".into())
            .spawn(move || {
                let mut pipe = pipe;
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
        const REPLACED: &[&str] = &[
            "MULTICORE_DAEMON_URL",
            "MULTICORE_DAEMON_TOKEN",
            "MULTICORE_DAEMON_READY_STDOUT",
            "MULTICORE_DAEMON_ADDR",
            "MULTICORE_DAEMON_DATA_DIR",
            "MULTICORE_XRAY_BIN",
            "MULTICORE_MIHOMO_BIN",
            "MULTICORE_MIHOMO_CONTROLLER_ADDR",
        ];
        let mut values: Vec<(OsString, OsString)> = std::env::vars_os()
            .filter(|(name, _)| {
                name.to_str().is_none_or(|name| {
                    !REPLACED
                        .iter()
                        .any(|candidate| name.eq_ignore_ascii_case(candidate))
                })
            })
            .collect();
        values.extend([
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
        ]);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::OsString, fs, net::SocketAddr, path::Path};

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

    fn mutate_test_pe(path: &Path, range: std::ops::Range<usize>, bytes: &[u8]) {
        let mut image = fs::read(path).unwrap();
        image[range].copy_from_slice(bytes);
        fs::write(path, image).unwrap();
    }

    #[test]
    fn pe_validation_rejects_incoherent_or_unbounded_headers_and_sections() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("fixture.exe");
        let cases: &[(&str, std::ops::Range<usize>, &[u8])] = &[
            ("zero sections", 0x86..0x88, &0_u16.to_le_bytes()),
            ("not executable", 0x96..0x98, &0x20_u16.to_le_bytes()),
            ("short optional header", 0x94..0x96, &111_u16.to_le_bytes()),
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
                0x104..0x108,
                &17_u32.to_le_bytes(),
            ),
        ];
        for (name, range, mutation) in cases {
            write_test_pe(&path, 0x8664, 0x20b, 0x0022);
            mutate_test_pe(&path, range.clone(), mutation);
            let mut file = File::open(&path).unwrap();
            assert!(validate_amd64_pe32_plus(&mut file).is_err(), "case: {name}");
        }

        write_test_pe(&path, 0x8664, 0x20b, 0x0022);
        mutate_test_pe(&path, 0x198..0x19c, &0x80_u32.to_le_bytes());
        mutate_test_pe(&path, 0x19c..0x1a0, &0x1f0_u32.to_le_bytes());
        let mut file = File::open(&path).unwrap();
        assert!(
            validate_amd64_pe32_plus(&mut file).is_err(),
            "section raw range must remain within the file"
        );

        let current_exe = std::env::current_exe().unwrap();
        let mut current = File::open(current_exe).unwrap();
        validate_amd64_pe32_plus(&mut current).expect("the native test PE must remain accepted");
    }

    #[test]
    fn packaged_paths_support_spaces_and_require_contained_amd64_pe32_plus_non_dll_files() {
        let temp = tempfile::tempdir().unwrap();
        let package = temp.path().join("portable package with spaces");
        let runtime = package.join("runtime");
        let cores = package.join("cores");
        fs::create_dir_all(&runtime).unwrap();
        fs::create_dir_all(&cores).unwrap();
        let desktop = package.join("MultiCore.exe");
        write_test_pe(&desktop, 0x8664, 0x20b, 0x0022);
        for path in [
            runtime.join("multicore-daemon.exe"),
            runtime.join("multicore-updater.exe"),
            cores.join("xray.exe"),
            cores.join("mihomo.exe"),
        ] {
            write_test_pe(&path, 0x8664, 0x20b, 0x0022);
        }
        let paths =
            derive_and_validate_paths(&desktop, temp.path().join("Local App Data")).unwrap();
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
        write_test_pe(&updater, 0x8664, 0x20b, 0x0022);

        write_test_pe(&xray, 0x014c, 0x10b, 0x0022);
        assert!(derive_and_validate_paths(&desktop, temp.path()).is_err());
        write_test_pe(&xray, 0x8664, 0x20b, 0x2022);
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
        write_test_pe(&desktop, 0x8664, 0x20b, 0x0022);
        let outside = temp.path().join("outside.exe");
        write_test_pe(&outside, 0x8664, 0x20b, 0x0022);
        if symlink_file(&outside, package.join("runtime/multicore-daemon.exe")).is_err() {
            // Creating symlinks requires Developer Mode or elevation on some Windows hosts.
            return;
        }
        write_test_pe(&package.join("cores/xray.exe"), 0x8664, 0x20b, 0x0022);
        write_test_pe(&package.join("cores/mihomo.exe"), 0x8664, 0x20b, 0x0022);
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
}
