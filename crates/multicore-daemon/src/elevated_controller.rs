use std::{
    fmt,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

use multicore_core::elevation_protocol::{
    Authentication, BrokerErrorCode, ElevatedDiagnosticStream, ElevatedEngine,
    ElevatedRuntimeState, ElevationCommand, ElevationResponse, SessionSecret,
    response_matches_command,
};
use multicore_core::{
    CoreLogRecord, DiagnosticStream, Engine, ProcessController, ProcessDiagnostics,
    RuntimeCheckState, StartFailureCause,
};
#[cfg(windows)]
use zeroize::Zeroizing;

pub const BROKER_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
pub const BROKER_IO_TIMEOUT: Duration = Duration::from_secs(5);
/// A start response arrives only after the host has waited for core readiness.
pub const BROKER_START_TIMEOUT: Duration =
    Duration::from_secs(multicore_core::CORE_READINESS_TIMEOUT.as_secs() + 8);
#[cfg(unix)]
pub const BROKER_AUTHORIZATION_TIMEOUT: Duration = Duration::from_secs(300);

#[cfg(windows)]
const DAEMON_EXECUTABLE: &str = "multicore-daemon.exe";
#[cfg(windows)]
const CORE_HOST_EXECUTABLE: &str = "multicore-core-host.exe";
#[cfg(not(windows))]
const DAEMON_EXECUTABLE: &str = "multicore-daemon";
#[cfg(not(windows))]
const CORE_HOST_EXECUTABLE: &str = "multicore-core-host";

#[derive(Debug, Eq, PartialEq)]
pub enum ElevationError {
    ElevationCancelled,
    TimedOut,
    Cancelled,
    PeerPidMismatch,
    AuthenticationFailed,
    AlreadyConnected,
    Disconnected,
    Protocol,
    LaunchFailed,
    Io,
    /// The host could not launch the core or the core exited during startup.
    CoreStartFailed,
    /// The host launched the core but it did not become ready in time.
    CoreReadinessTimedOut,
}

impl fmt::Display for ElevationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ElevationCancelled => "elevation was cancelled",
            Self::TimedOut => "elevation broker timed out",
            Self::Cancelled => "elevation broker was cancelled",
            Self::PeerPidMismatch => "elevation broker peer identity did not match",
            Self::AuthenticationFailed => "elevation broker authentication failed",
            Self::AlreadyConnected => "elevation broker already has a client",
            Self::Disconnected => "elevation broker disconnected",
            Self::Protocol => "elevation broker protocol failed",
            Self::LaunchFailed => "elevation broker launch failed",
            Self::Io => "elevation broker I/O failed",
            Self::CoreStartFailed => "elevated core failed to start",
            Self::CoreReadinessTimedOut => "elevated core did not become ready in time",
        })
    }
}

impl std::error::Error for ElevationError {}

impl ElevationError {
    /// The user-facing reason a start that failed with this error could not complete.
    #[must_use]
    pub fn start_failure_cause(&self) -> StartFailureCause {
        match self {
            Self::ElevationCancelled => StartFailureCause::ElevationCancelled,
            Self::LaunchFailed
            | Self::PeerPidMismatch
            | Self::AuthenticationFailed
            | Self::AlreadyConnected => StartFailureCause::ElevationFailed,
            Self::CoreStartFailed => StartFailureCause::CoreFailed,
            Self::CoreReadinessTimedOut => StartFailureCause::ReadinessTimedOut,
            Self::TimedOut | Self::Cancelled | Self::Disconnected | Self::Protocol | Self::Io => {
                StartFailureCause::BrokerFailed
            }
        }
    }
}

/// How long to wait for the host's response to `command`.
#[must_use]
pub fn broker_response_timeout(command: &ElevationCommand) -> Duration {
    match command {
        ElevationCommand::StartXray { .. } | ElevationCommand::StartMihomo { .. } => {
            BROKER_START_TIMEOUT
        }
        _ => BROKER_IO_TIMEOUT,
    }
}

pub trait BrokerTransport: Send {
    fn request(&self, command: &ElevationCommand) -> Result<ElevationResponse, ElevationError>;
}

pub trait BrokerConnector: Send + Sync + 'static {
    type Transport: BrokerTransport;
    fn connect(&self, executable: &Path) -> Result<Self::Transport, ElevationError>;
}

struct ElevatedBroker<C: BrokerConnector> {
    host_executable: PathBuf,
    connector: C,
    session: Mutex<Option<C::Transport>>,
}

pub struct ElevatedProcessController<C: BrokerConnector> {
    broker: Arc<ElevatedBroker<C>>,
    generation_id: u64,
}

pub struct ElevatedControllerFactory<C: BrokerConnector> {
    broker: Arc<ElevatedBroker<C>>,
}

impl<C: BrokerConnector> Clone for ElevatedProcessController<C> {
    fn clone(&self) -> Self {
        Self {
            broker: self.broker.clone(),
            generation_id: self.generation_id,
        }
    }
}

impl<C: BrokerConnector> ElevatedControllerFactory<C> {
    fn new(host_executable: PathBuf, connector: C) -> Self {
        Self {
            broker: Arc::new(ElevatedBroker {
                host_executable,
                connector,
                session: Mutex::new(None),
            }),
        }
    }

    pub fn controller(
        &self,
        generation_id: u64,
    ) -> Result<ElevatedProcessController<C>, ElevationError> {
        if generation_id == 0 {
            return Err(ElevationError::Protocol);
        }
        Ok(ElevatedProcessController {
            broker: self.broker.clone(),
            generation_id,
        })
    }

    pub fn shutdown(&self) {
        shutdown_broker(&self.broker);
    }
}

impl<C: BrokerConnector> Drop for ElevatedBroker<C> {
    fn drop(&mut self) {
        shutdown_broker(self);
    }
}

impl<C: BrokerConnector> ElevatedProcessController<C> {
    fn command_blocking(
        inner: &ElevatedBroker<C>,
        command: ElevationCommand,
        launch: bool,
    ) -> Result<ElevationResponse, ElevationError> {
        let mut session = inner
            .session
            .lock()
            .map_err(|_| ElevationError::Disconnected)?;
        if session.is_none() {
            if !launch {
                return Err(ElevationError::Disconnected);
            }
            *session = Some(inner.connector.connect(&inner.host_executable)?);
        }
        let result = session
            .as_ref()
            .expect("broker session exists")
            .request(&command);
        if result.is_err() {
            session.take();
        }
        result
    }

    pub fn shutdown(&self) {
        shutdown_broker(&self.broker);
    }
}

fn shutdown_broker<C: BrokerConnector>(broker: &ElevatedBroker<C>) {
    let Ok(mut session) = broker.session.lock() else {
        return;
    };
    if let Some(transport) = session.take() {
        let _ = transport.request(&ElevationCommand::Shutdown);
    }
}

impl<C: BrokerConnector> ProcessController for ElevatedProcessController<C> {
    type Error = ElevationError;

    fn start<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        let inner = self.broker.clone();
        let generation_id = self.generation_id;
        Box::pin(async move {
            let command = match engine {
                Engine::Xray => ElevationCommand::StartXray { generation_id },
                Engine::Mihomo => ElevationCommand::StartMihomo { generation_id },
            };
            let response =
                tokio::task::spawn_blocking(move || Self::command_blocking(&inner, command, true))
                    .await
                    .map_err(|_| ElevationError::Io)??;
            match response {
                ElevationResponse::Started { engine: actual }
                    if actual == elevated_engine(engine) =>
                {
                    Ok(())
                }
                ElevationResponse::Error {
                    code: BrokerErrorCode::TimedOut,
                } => Err(ElevationError::CoreReadinessTimedOut),
                ElevationResponse::Error { .. } => Err(ElevationError::CoreStartFailed),
                _ => Err(ElevationError::Protocol),
            }
        })
    }

    fn start_failure_cause(&self, error: &Self::Error) -> StartFailureCause {
        error.start_failure_cause()
    }

    fn stop<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        let inner = self.broker.clone();
        Box::pin(async move {
            let command = ElevationCommand::Stop {
                engine: elevated_engine(engine),
            };
            let response =
                tokio::task::spawn_blocking(move || Self::command_blocking(&inner, command, false))
                    .await
                    .map_err(|_| ElevationError::Io)?;
            let response = match response {
                Ok(value) => value,
                Err(ElevationError::Disconnected) => return Ok(()),
                Err(error) => return Err(error),
            };
            match response {
                ElevationResponse::Stopped { engine: actual }
                    if actual == elevated_engine(engine) =>
                {
                    Ok(())
                }
                _ => Err(ElevationError::Protocol),
            }
        })
    }

    fn diagnostics<'a>(&'a self) -> Pin<Box<dyn Future<Output = ProcessDiagnostics> + Send + 'a>> {
        let inner = self.broker.clone();
        Box::pin(async move {
            let response = tokio::task::spawn_blocking(move || {
                Self::command_blocking(&inner, ElevationCommand::Diagnostics, false)
            })
            .await;
            match response {
                Ok(Ok(response)) => diagnostics_from_response(response),
                _ => ProcessDiagnostics::default(),
            }
        })
    }
}

fn elevated_engine(engine: Engine) -> ElevatedEngine {
    match engine {
        Engine::Xray => ElevatedEngine::Xray,
        Engine::Mihomo => ElevatedEngine::Mihomo,
    }
}
fn runtime_state(state: ElevatedRuntimeState) -> RuntimeCheckState {
    match state {
        ElevatedRuntimeState::Stopped => RuntimeCheckState::Stopped,
        ElevatedRuntimeState::Starting => RuntimeCheckState::Starting,
        ElevatedRuntimeState::Ready => RuntimeCheckState::Ready,
        ElevatedRuntimeState::Failed => RuntimeCheckState::Failed,
        ElevatedRuntimeState::Unsupported => RuntimeCheckState::Unsupported,
    }
}

fn diagnostics_from_response(response: ElevationResponse) -> ProcessDiagnostics {
    match response {
        ElevationResponse::ProcessDiagnostics {
            xray,
            mihomo,
            tun,
            logs,
        } => ProcessDiagnostics {
            xray: runtime_state(xray),
            mihomo: runtime_state(mihomo),
            tun: runtime_state(tun),
            logs: logs
                .into_iter()
                .map(|record| CoreLogRecord {
                    id: record.id,
                    timestamp_ms: record.timestamp_ms,
                    engine: match record.engine {
                        ElevatedEngine::Xray => Engine::Xray,
                        ElevatedEngine::Mihomo => Engine::Mihomo,
                    },
                    stream: match record.stream {
                        ElevatedDiagnosticStream::Stdout => DiagnosticStream::Stdout,
                        ElevatedDiagnosticStream::Stderr => DiagnosticStream::Stderr,
                    },
                    message: record.message,
                })
                .collect(),
        },
        ElevationResponse::Diagnostics {
            xray_running,
            mihomo_running,
        } => ProcessDiagnostics {
            xray: if xray_running {
                RuntimeCheckState::Ready
            } else {
                RuntimeCheckState::Stopped
            },
            mihomo: if mihomo_running {
                RuntimeCheckState::Ready
            } else {
                RuntimeCheckState::Stopped
            },
            tun: RuntimeCheckState::Unsupported,
            logs: Vec::new(),
        },
        _ => ProcessDiagnostics::default(),
    }
}

pub fn runtime_generation_from_path(path: &Path) -> Result<u64, ElevationError> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(ElevationError::Protocol)?;
    let digits = name
        .strip_prefix("runtime-")
        .filter(|digits| digits.len() == 20 && digits.bytes().all(|byte| byte.is_ascii_digit()))
        .ok_or(ElevationError::Protocol)?;
    digits
        .parse::<u64>()
        .ok()
        .filter(|value| *value != 0)
        .ok_or(ElevationError::Protocol)
}

pub fn packaged_core_host_from(daemon_executable: &Path) -> Result<PathBuf, ElevationError> {
    let daemon =
        std::fs::canonicalize(daemon_executable).map_err(|_| ElevationError::LaunchFailed)?;
    let runtime = daemon.parent().ok_or(ElevationError::LaunchFailed)?;
    if runtime.file_name().and_then(|name| name.to_str()) != Some("runtime")
        || daemon.file_name().and_then(|name| name.to_str()) != Some(DAEMON_EXECUTABLE)
    {
        return Err(ElevationError::LaunchFailed);
    }
    let host = runtime.join(CORE_HOST_EXECUTABLE);
    let host = std::fs::canonicalize(host).map_err(|_| ElevationError::LaunchFailed)?;
    if host.parent() != Some(runtime)
        || host.file_name().and_then(|name| name.to_str()) != Some(CORE_HOST_EXECUTABLE)
    {
        return Err(ElevationError::LaunchFailed);
    }
    Ok(host)
}

pub trait ElevatedHostLauncher: Send + Sync {
    type Host;

    fn launch(
        &self,
        executable: &Path,
        pipe_name: &str,
        protocol_version: u16,
        server_pid: u32,
    ) -> Result<Self::Host, ElevationError>;
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionState {
    Listening,
    PeerVerified,
    Authenticated,
    Closed,
}

#[cfg(test)]
#[derive(Debug)]
struct BrokerSession {
    expected_client_pid: u32,
    secret: SessionSecret,
    state: SessionState,
}

#[cfg(test)]
impl BrokerSession {
    #[must_use]
    pub fn new(expected_client_pid: u32, secret: SessionSecret) -> Self {
        Self {
            expected_client_pid,
            secret,
            state: SessionState::Listening,
        }
    }

    pub fn verify_peer(&mut self, actual_pid: u32) -> Result<(), ElevationError> {
        if self.state != SessionState::Listening {
            self.state = SessionState::Closed;
            return Err(ElevationError::AlreadyConnected);
        }
        if actual_pid != self.expected_client_pid {
            self.state = SessionState::Closed;
            return Err(ElevationError::PeerPidMismatch);
        }
        self.state = SessionState::PeerVerified;
        Ok(())
    }

    pub fn authenticate(&mut self, proof: &Authentication) -> Result<(), ElevationError> {
        if self.state != SessionState::PeerVerified || !self.secret.verifies(proof) {
            self.state = SessionState::Closed;
            return Err(ElevationError::AuthenticationFailed);
        }
        self.state = SessionState::Authenticated;
        Ok(())
    }

    #[must_use]
    pub fn authentication(&self) -> Authentication {
        self.secret.authentication()
    }

    pub fn command(&self, command: &ElevationCommand) -> ElevationResponse {
        if self.state != SessionState::Authenticated {
            return ElevationResponse::Error {
                code: BrokerErrorCode::AuthenticationFailed,
            };
        }
        match command {
            ElevationCommand::Diagnostics => ElevationResponse::Diagnostics {
                xray_running: false,
                mihomo_running: false,
            },
            ElevationCommand::Shutdown => ElevationResponse::ShuttingDown,
            ElevationCommand::Stop { engine } => ElevationResponse::Stopped { engine: *engine },
            ElevationCommand::StartXray { .. } | ElevationCommand::StartMihomo { .. } => {
                ElevationResponse::Error {
                    code: BrokerErrorCode::NotReady,
                }
            }
        }
    }

    pub fn eof(&mut self) -> Result<(), ElevationError> {
        self.state = SessionState::Closed;
        Err(ElevationError::Disconnected)
    }

    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.state == SessionState::Closed
    }
}

#[must_use]
pub fn map_shell_execute_error(raw_error: u32) -> ElevationError {
    if raw_error == 1223 {
        ElevationError::ElevationCancelled
    } else {
        ElevationError::LaunchFailed
    }
}

#[cfg(windows)]
pub mod windows {
    use super::*;
    use multicore_core::elevation_protocol::{
        ELEVATION_PROTOCOL_VERSION, MAX_ELEVATION_FRAME_BYTES, decode_frame, encode_frame,
    };
    use std::{
        ffi::OsStr,
        mem::{size_of, zeroed},
        os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
        },
        ptr,
        sync::{
            Arc, Condvar, Mutex, OnceLock,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::Instant,
    };
    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_MORE_DATA, ERROR_NO_DATA,
            ERROR_OPERATION_ABORTED, ERROR_PIPE_CONNECTED, GetLastError, HANDLE,
            INVALID_HANDLE_VALUE, LocalFree, WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            },
            GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
            TOKEN_USER, TokenUser,
        },
        Storage::FileSystem::{
            FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX, ReadFile,
            WriteFile,
        },
        System::{
            IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
            Pipes::{
                ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId,
                PIPE_READMODE_MESSAGE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE, PIPE_WAIT,
            },
            Threading::{
                CreateEventW, GetCurrentProcess, GetCurrentProcessId, GetProcessId,
                OpenProcessToken, WaitForSingleObject,
            },
        },
        UI::{
            Shell::{
                SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
            },
            WindowsAndMessaging::SW_HIDE,
        },
    };

    pub struct LaunchedHost {
        process: OwnedHandle,
        process_id: u32,
    }

    impl fmt::Debug for LaunchedHost {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter
                .debug_struct("LaunchedHost")
                .field("process_id", &self.process_id)
                .finish_non_exhaustive()
        }
    }

    pub struct ShellExecuteLauncher;

    #[derive(Clone, Copy, Debug, Default)]
    pub struct WindowsBrokerConnector;

    impl BrokerTransport for AuthenticatedPipe {
        fn request(&self, command: &ElevationCommand) -> Result<ElevationResponse, ElevationError> {
            let cancelled = AtomicBool::new(false);
            AuthenticatedPipe::request(self, command, &cancelled)
        }
    }

    impl BrokerConnector for WindowsBrokerConnector {
        type Transport = AuthenticatedPipe;

        fn connect(&self, executable: &Path) -> Result<Self::Transport, ElevationError> {
            let server = NamedPipeServer::create()?;
            let secret = SessionSecret::generate().map_err(|_| ElevationError::Io)?;
            let host = ShellExecuteLauncher.launch(
                executable,
                &server.name,
                ELEVATION_PROTOCOL_VERSION,
                NamedPipeServer::server_pid(),
            )?;
            let cancelled = AtomicBool::new(false);
            server.authenticate(host, &secret, &cancelled)
        }
    }

    impl ElevatedHostLauncher for ShellExecuteLauncher {
        type Host = LaunchedHost;

        fn launch(
            &self,
            executable: &Path,
            pipe_name: &str,
            protocol_version: u16,
            server_pid: u32,
        ) -> Result<LaunchedHost, ElevationError> {
            let Some(parent) = executable
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            else {
                return Err(ElevationError::LaunchFailed);
            };
            if pipe_name.contains(['\0', '"'])
                || server_pid == 0
                || os_str_contains_nul(executable.as_os_str())
                || os_str_contains_nul(parent.as_os_str())
            {
                return Err(ElevationError::LaunchFailed);
            }
            let executable_wide = wide(executable.as_os_str());
            let verb = wide(OsStr::new("runas"));
            let parameters = wide(OsStr::new(&format!(
                "--pipe \"{pipe_name}\" --protocol {protocol_version} --server-pid {server_pid}"
            )));
            let directory = wide(parent.as_os_str());
            let mut info: SHELLEXECUTEINFOW = unsafe { zeroed() };
            info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
            info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
            info.lpVerb = verb.as_ptr();
            info.lpFile = executable_wide.as_ptr();
            info.lpParameters = parameters.as_ptr();
            info.lpDirectory = directory.as_ptr();
            info.nShow = SW_HIDE;
            if unsafe { ShellExecuteExW(&mut info) } == 0 {
                let error = unsafe { GetLastError() };
                return Err(map_shell_execute_error(error));
            }
            if info.hProcess.is_null() {
                return Err(ElevationError::LaunchFailed);
            }
            let process_id = unsafe { GetProcessId(info.hProcess) };
            if process_id == 0 {
                unsafe { CloseHandle(info.hProcess) };
                return Err(ElevationError::LaunchFailed);
            }
            Ok(LaunchedHost {
                process: unsafe { OwnedHandle::from_raw_handle(info.hProcess) },
                process_id,
            })
        }
    }

    pub struct NamedPipeServer {
        handle: OwnedHandle,
        pub name: String,
        accepted: bool,
    }

    pub struct AuthenticatedPipe {
        handle: Mutex<Option<OwnedHandle>>,
        terminal: AtomicBool,
        _host_process: OwnedHandle,
    }

    impl NamedPipeServer {
        pub fn create() -> Result<Self, ElevationError> {
            let mut random = [0_u8; 24];
            getrandom::fill(&mut random).map_err(|_| ElevationError::Io)?;
            let suffix = random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let name = format!(r"\\.\pipe\MultiCore.Elevation.v1.{suffix}");
            let name_wide = wide(OsStr::new(&name));
            let descriptor = SecurityDescriptor::current_user_and_system()?;
            let security = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor.0,
                bInheritHandle: 0,
            };
            let handle = unsafe {
                CreateNamedPipeW(
                    name_wide.as_ptr(),
                    PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
                    PIPE_TYPE_MESSAGE
                        | PIPE_READMODE_MESSAGE
                        | PIPE_WAIT
                        | PIPE_REJECT_REMOTE_CLIENTS,
                    1,
                    MAX_ELEVATION_FRAME_BYTES as u32,
                    MAX_ELEVATION_FRAME_BYTES as u32,
                    0,
                    &security,
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(ElevationError::Io);
            }
            Ok(Self {
                handle: unsafe { OwnedHandle::from_raw_handle(handle) },
                name,
                accepted: false,
            })
        }

        pub fn authenticate(
            mut self,
            host: LaunchedHost,
            secret: &SessionSecret,
            cancelled: &AtomicBool,
        ) -> Result<AuthenticatedPipe, ElevationError> {
            if self.accepted {
                return Err(ElevationError::AlreadyConnected);
            }
            self.accepted = true;
            connect_overlapped(
                self.handle.as_raw_handle(),
                BROKER_CONNECT_TIMEOUT,
                cancelled,
            )?;
            verify_client_pid(self.handle.as_raw_handle(), host.process_id)?;
            write_message(
                self.handle.as_raw_handle(),
                &secret.authentication(),
                BROKER_IO_TIMEOUT,
                cancelled,
            )?;
            let frame = read_message(self.handle.as_raw_handle(), BROKER_IO_TIMEOUT, cancelled)?;
            let echoed: Authentication =
                decode_frame(&frame).map_err(|_| ElevationError::Protocol)?;
            if !secret.verifies(&echoed) {
                return Err(ElevationError::AuthenticationFailed);
            }
            Ok(AuthenticatedPipe {
                handle: Mutex::new(Some(self.handle)),
                terminal: AtomicBool::new(false),
                _host_process: host.process,
            })
        }

        #[must_use]
        pub fn server_pid() -> u32 {
            unsafe { GetCurrentProcessId() }
        }
    }

    impl AuthenticatedPipe {
        pub fn request(
            &self,
            command: &ElevationCommand,
            cancelled: &AtomicBool,
        ) -> Result<ElevationResponse, ElevationError> {
            if self.terminal.load(Ordering::Acquire) {
                return Err(ElevationError::Disconnected);
            }
            let Ok(mut handle) = self.handle.lock() else {
                self.terminal.store(true, Ordering::Release);
                return Err(ElevationError::Disconnected);
            };
            let Some(pipe) = handle.as_ref() else {
                self.terminal.store(true, Ordering::Release);
                return Err(ElevationError::Disconnected);
            };
            let result = write_message(pipe.as_raw_handle(), command, BROKER_IO_TIMEOUT, cancelled)
                .and_then(|()| {
                    read_message(
                        pipe.as_raw_handle(),
                        broker_response_timeout(command),
                        cancelled,
                    )
                })
                .and_then(|frame| decode_frame(&frame).map_err(|_| ElevationError::Protocol))
                .and_then(|response| {
                    if response_matches_command(command, &response) {
                        Ok(response)
                    } else {
                        Err(ElevationError::Protocol)
                    }
                });
            if result.is_err() {
                handle.take();
                self.terminal.store(true, Ordering::Release);
            }
            result
        }

        #[must_use]
        pub fn is_terminal(&self) -> bool {
            self.terminal.load(Ordering::Acquire)
        }
    }

    struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

    impl SecurityDescriptor {
        fn current_user_and_system() -> Result<Self, ElevationError> {
            let sid = current_user_sid_string()?;
            // FILE_GENERIC_READ | FILE_GENERIC_WRITE | SYNCHRONIZE; no generic-all ACE.
            let sddl = wide(OsStr::new(&format!(
                "D:P(A;;0x0012019f;;;SY)(A;;0x0012019f;;;{sid})"
            )));
            let mut descriptor = ptr::null_mut();
            if unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    1,
                    &mut descriptor,
                    ptr::null_mut(),
                )
            } == 0
            {
                return Err(ElevationError::Io);
            }
            Ok(Self(descriptor))
        }
    }

    impl Drop for SecurityDescriptor {
        fn drop(&mut self) {
            unsafe { LocalFree(self.0.cast()) };
        }
    }

    fn current_user_sid_string() -> Result<String, ElevationError> {
        let mut token = ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(ElevationError::Io);
        }
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let mut needed = 0;
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                ptr::null_mut(),
                0,
                &mut needed,
            )
        };
        if needed == 0 {
            return Err(ElevationError::Io);
        }
        let words = (needed as usize).div_ceil(size_of::<usize>());
        let mut buffer = vec![0_usize; words];
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        } == 0
        {
            return Err(ElevationError::Io);
        }
        let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
        let mut sid_string = ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid_string) } == 0 {
            return Err(ElevationError::Io);
        }
        let mut length = 0;
        while unsafe { *sid_string.add(length) } != 0 {
            length += 1;
        }
        let result = String::from_utf16(unsafe { std::slice::from_raw_parts(sid_string, length) })
            .map_err(|_| ElevationError::Io);
        unsafe { LocalFree(sid_string.cast()) };
        result
    }

    fn connect_overlapped(
        handle: HANDLE,
        timeout: Duration,
        cancelled: &AtomicBool,
    ) -> Result<(), ElevationError> {
        let mut pending = PendingIo::new()?;
        let started = unsafe { ConnectNamedPipe(handle, pending.overlapped_mut()) };
        if started != 0 {
            return Ok(());
        }
        let error = unsafe { GetLastError() };
        if error == ERROR_PIPE_CONNECTED {
            return Ok(());
        }
        if error != ERROR_IO_PENDING {
            return Err(ElevationError::Io);
        }
        wait_pending(handle, &mut pending, timeout, cancelled).map(|_| ())
    }

    fn verify_client_pid(handle: HANDLE, expected: u32) -> Result<(), ElevationError> {
        let mut actual = 0;
        if unsafe { GetNamedPipeClientProcessId(handle, &mut actual) } == 0 {
            return Err(ElevationError::Io);
        }
        if actual != expected {
            return Err(ElevationError::PeerPidMismatch);
        }
        Ok(())
    }

    pub(crate) fn read_message(
        handle: HANDLE,
        timeout: Duration,
        cancelled: &AtomicBool,
    ) -> Result<Zeroizing<Vec<u8>>, ElevationError> {
        let mut pending = PendingIo::with_buffer(vec![0_u8; MAX_ELEVATION_FRAME_BYTES])?;
        let mut transferred = 0;
        let started = unsafe {
            ReadFile(
                handle,
                pending.buffer_mut().as_mut_ptr(),
                MAX_ELEVATION_FRAME_BYTES as u32,
                &mut transferred,
                pending.overlapped_mut(),
            )
        };
        if started == 0 {
            let error = unsafe { GetLastError() };
            if error == ERROR_MORE_DATA {
                return Err(ElevationError::Protocol);
            }
            if error == ERROR_BROKEN_PIPE || error == ERROR_NO_DATA {
                return Err(ElevationError::Disconnected);
            }
            if error != ERROR_IO_PENDING {
                return Err(ElevationError::Io);
            }
            transferred = wait_pending(handle, &mut pending, timeout, cancelled)?;
        }
        if transferred == 0 {
            return Err(ElevationError::Disconnected);
        }
        let mut buffer = pending.take_buffer();
        buffer.truncate(transferred as usize);
        Ok(buffer)
    }

    fn write_message<T: serde::Serialize>(
        handle: HANDLE,
        value: &T,
        timeout: Duration,
        cancelled: &AtomicBool,
    ) -> Result<(), ElevationError> {
        let frame = encode_frame(value).map_err(|_| ElevationError::Protocol)?;
        write_raw_message(handle, frame, timeout, cancelled)
    }

    pub(crate) fn write_raw_message(
        handle: HANDLE,
        frame: Vec<u8>,
        timeout: Duration,
        cancelled: &AtomicBool,
    ) -> Result<(), ElevationError> {
        let frame_len = frame.len();
        let mut pending = PendingIo::with_buffer(frame)?;
        let mut transferred = 0;
        let started = unsafe {
            WriteFile(
                handle,
                pending.buffer().as_ptr(),
                frame_len as u32,
                &mut transferred,
                pending.overlapped_mut(),
            )
        };
        if started == 0 {
            let error = unsafe { GetLastError() };
            if error == ERROR_BROKEN_PIPE || error == ERROR_NO_DATA {
                return Err(ElevationError::Disconnected);
            }
            if error != ERROR_IO_PENDING {
                return Err(ElevationError::Io);
            }
            transferred = wait_pending(handle, &mut pending, timeout, cancelled)?;
        }
        if transferred as usize != frame_len {
            return Err(ElevationError::Io);
        }
        Ok(())
    }

    fn wait_pending(
        handle: HANDLE,
        pending: &mut PendingIo,
        timeout: Duration,
        cancelled: &AtomicBool,
    ) -> Result<u32, ElevationError> {
        let deadline = Instant::now() + timeout;
        loop {
            if cancelled.load(Ordering::Acquire) {
                pending.cancel_and_drain(handle);
                return Err(ElevationError::Cancelled);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                pending.cancel_and_drain(handle);
                return Err(ElevationError::TimedOut);
            }
            let wait_ms = remaining.as_millis().clamp(1, 50) as u32;
            match unsafe { WaitForSingleObject(pending.event_handle(), wait_ms) } {
                WAIT_OBJECT_0 => {
                    let mut transferred = 0;
                    if unsafe {
                        GetOverlappedResult(handle, pending.overlapped_mut(), &mut transferred, 0)
                    } == 0
                    {
                        let error = unsafe { GetLastError() };
                        return if error == ERROR_OPERATION_ABORTED {
                            Err(ElevationError::Cancelled)
                        } else if error == ERROR_MORE_DATA {
                            Err(ElevationError::Protocol)
                        } else if error == ERROR_BROKEN_PIPE || error == ERROR_NO_DATA {
                            Err(ElevationError::Disconnected)
                        } else {
                            Err(ElevationError::Io)
                        };
                    }
                    return Ok(transferred);
                }
                WAIT_TIMEOUT => {}
                _ => {
                    pending.cancel_and_drain(handle);
                    return Err(ElevationError::Io);
                }
            }
        }
    }

    struct PendingIo {
        overlapped: Option<Box<OVERLAPPED>>,
        event: Option<OwnedHandle>,
        buffer: Option<Zeroizing<Vec<u8>>>,
    }

    impl PendingIo {
        fn new() -> Result<Self, ElevationError> {
            Self::with_buffer(Vec::new())
        }

        fn with_buffer(buffer: Vec<u8>) -> Result<Self, ElevationError> {
            let buffer = Zeroizing::new(buffer);
            let handle = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
            if handle.is_null() {
                return Err(ElevationError::Io);
            }
            let event = unsafe { OwnedHandle::from_raw_handle(handle) };
            let mut overlapped: Box<OVERLAPPED> = Box::new(unsafe { zeroed() });
            overlapped.hEvent = event.as_raw_handle();
            Ok(Self {
                overlapped: Some(overlapped),
                event: Some(event),
                buffer: Some(buffer),
            })
        }

        fn overlapped_mut(&mut self) -> &mut OVERLAPPED {
            self.overlapped.as_deref_mut().expect("pending I/O is live")
        }

        fn event_handle(&self) -> HANDLE {
            self.event
                .as_ref()
                .expect("pending I/O event is live")
                .as_raw_handle()
        }

        fn buffer(&self) -> &[u8] {
            self.buffer.as_deref().expect("pending I/O buffer is live")
        }

        fn buffer_mut(&mut self) -> &mut [u8] {
            self.buffer
                .as_deref_mut()
                .expect("pending I/O buffer is live")
        }

        fn take_buffer(&mut self) -> Zeroizing<Vec<u8>> {
            self.buffer.take().expect("pending I/O buffer is live")
        }

        fn cancel_and_drain(&mut self, handle: HANDLE) {
            unsafe { CancelIoEx(handle, self.overlapped_mut()) };
            let drained =
                unsafe { WaitForSingleObject(self.event_handle(), 1_000) } == WAIT_OBJECT_0;
            if drained {
                let mut ignored = 0;
                unsafe {
                    GetOverlappedResult(handle, self.overlapped_mut(), &mut ignored, 0);
                }
            } else {
                cancellation_reaper().retain(RetainedOperation {
                    _overlapped: self.overlapped.take().expect("pending I/O is live"),
                    event: self.event.take().expect("pending I/O event is live"),
                    _buffer: self.buffer.take().expect("pending I/O buffer is live"),
                });
            }
        }
    }

    struct RetainedOperation {
        _overlapped: Box<OVERLAPPED>,
        event: OwnedHandle,
        _buffer: Zeroizing<Vec<u8>>,
    }

    // SAFETY: the reaper never dereferences OVERLAPPED or its buffer; it only keeps their stable
    // allocations alive until the kernel signals the owned event.
    unsafe impl Send for RetainedOperation {}

    struct CancellationReaper {
        queue: Mutex<Vec<RetainedOperation>>,
        changed: Condvar,
    }

    impl CancellationReaper {
        fn retain(&self, operation: RetainedOperation) {
            let mut queue = self
                .queue
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            while queue.len() >= 64 {
                queue = self
                    .changed
                    .wait_timeout(queue, Duration::from_millis(50))
                    .unwrap_or_else(|poison| poison.into_inner())
                    .0;
            }
            queue.push(operation);
            self.changed.notify_all();
        }

        fn run(&self) {
            loop {
                let mut queue = self
                    .queue
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                while queue.is_empty() {
                    queue = self
                        .changed
                        .wait(queue)
                        .unwrap_or_else(|poison| poison.into_inner());
                }
                queue.retain(|operation| {
                    (unsafe { WaitForSingleObject(operation.event.as_raw_handle(), 0) })
                        != WAIT_OBJECT_0
                });
                self.changed.notify_all();
                drop(queue);
                thread::sleep(Duration::from_millis(10));
            }
        }

        #[cfg(test)]
        fn wait_empty(&self, timeout: Duration) -> bool {
            let deadline = Instant::now() + timeout;
            let mut queue = self
                .queue
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            while !queue.is_empty() && Instant::now() < deadline {
                queue = self
                    .changed
                    .wait_timeout(queue, Duration::from_millis(10))
                    .unwrap_or_else(|poison| poison.into_inner())
                    .0;
            }
            queue.is_empty()
        }
    }

    fn cancellation_reaper() -> &'static Arc<CancellationReaper> {
        static REAPER: OnceLock<Arc<CancellationReaper>> = OnceLock::new();
        REAPER.get_or_init(|| {
            let reaper = Arc::new(CancellationReaper {
                queue: Mutex::new(Vec::new()),
                changed: Condvar::new(),
            });
            let worker = Arc::clone(&reaper);
            if thread::Builder::new()
                .name("multicore-pipe-cancel-reaper".into())
                .spawn(move || worker.run())
                .is_err()
            {
                // Unwinding would free buffers still referenced by the kernel.
                std::process::abort();
            }
            reaper
        })
    }

    fn os_str_contains_nul(value: &OsStr) -> bool {
        value.encode_wide().any(|unit| unit == 0)
    }

    fn wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(Some(0)).collect()
    }

    #[allow(dead_code)]
    const _: u16 = ELEVATION_PROTOCOL_VERSION;

    #[cfg(test)]
    mod tests {
        use super::*;
        use multicore_core::elevation_protocol::{BrokerErrorCode, decode_command};
        use std::{
            fs::OpenOptions,
            os::windows::fs::OpenOptionsExt,
            sync::{Arc, mpsc},
            thread,
        };
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        fn open_test_client(name: &str) -> std::fs::File {
            let started = Instant::now();
            loop {
                match OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(
                        FILE_FLAG_OVERLAPPED
                            | windows_sys::Win32::Storage::FileSystem::SECURITY_SQOS_PRESENT
                            | windows_sys::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION,
                    )
                    .open(name)
                {
                    Ok(pipe) => return pipe,
                    Err(_) if started.elapsed() < Duration::from_secs(2) => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("client did not connect: {error}"),
                }
            }
        }

        fn current_process_host() -> LaunchedHost {
            let process_id = unsafe { GetCurrentProcessId() };
            let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
            assert!(!process.is_null());
            LaunchedHost {
                process: unsafe { OwnedHandle::from_raw_handle(process) },
                process_id,
            }
        }

        fn spawn_protocol_client(
            name: String,
            behavior: impl FnOnce(HANDLE, &AtomicBool) + Send + 'static,
        ) -> thread::JoinHandle<()> {
            thread::spawn(move || {
                let pipe = open_test_client(&name);
                let mut server_pid = 0;
                assert_ne!(
                    unsafe {
                        windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId(
                            pipe.as_raw_handle(),
                            &mut server_pid,
                        )
                    },
                    0
                );
                assert_eq!(server_pid, unsafe { GetCurrentProcessId() });
                let never_cancelled = AtomicBool::new(false);
                let auth = read_message(
                    pipe.as_raw_handle(),
                    Duration::from_secs(2),
                    &never_cancelled,
                )
                .unwrap();
                let proof: Authentication = decode_frame(&auth).unwrap();
                write_message(
                    pipe.as_raw_handle(),
                    &proof,
                    Duration::from_secs(2),
                    &never_cancelled,
                )
                .unwrap();
                behavior(pipe.as_raw_handle(), &never_cancelled);
            })
        }

        fn connected_server() -> (NamedPipeServer, mpsc::Sender<()>, thread::JoinHandle<()>) {
            let server = NamedPipeServer::create().unwrap();
            let name = server.name.clone();
            let (opened_tx, opened_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let client = thread::spawn(move || {
                let _pipe = open_test_client(&name);
                opened_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            });
            let cancelled = AtomicBool::new(false);
            connect_overlapped(
                server.handle.as_raw_handle(),
                Duration::from_secs(2),
                &cancelled,
            )
            .unwrap();
            opened_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            (server, release_tx, client)
        }

        #[test]
        fn real_pipe_rejects_wrong_pid_and_a_second_client() {
            let (server, release, client) = connected_server();
            let current_pid = unsafe { GetCurrentProcessId() };
            assert_eq!(
                verify_client_pid(server.handle.as_raw_handle(), current_pid.wrapping_add(1)),
                Err(ElevationError::PeerPidMismatch)
            );
            assert!(
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&server.name)
                    .is_err()
            );
            release.send(()).unwrap();
            client.join().unwrap();
        }

        #[test]
        fn real_pipe_connect_is_bounded_by_timeout_and_cancellation() {
            let server = NamedPipeServer::create().unwrap();
            let cancelled = AtomicBool::new(false);
            assert_eq!(
                connect_overlapped(
                    server.handle.as_raw_handle(),
                    Duration::from_millis(25),
                    &cancelled,
                ),
                Err(ElevationError::TimedOut)
            );

            let server = NamedPipeServer::create().unwrap();
            let cancelled = AtomicBool::new(true);
            assert_eq!(
                connect_overlapped(
                    server.handle.as_raw_handle(),
                    Duration::from_secs(1),
                    &cancelled,
                ),
                Err(ElevationError::Cancelled)
            );

            for _ in 0..16 {
                let server = NamedPipeServer::create().unwrap();
                let cancelled = AtomicBool::new(true);
                assert_eq!(
                    connect_overlapped(
                        server.handle.as_raw_handle(),
                        Duration::from_secs(1),
                        &cancelled,
                    ),
                    Err(ElevationError::Cancelled)
                );
            }
            for _ in 0..16 {
                let mut pending = PendingIo::with_buffer(vec![0x5a; 32]).unwrap();
                assert_ne!(
                    unsafe {
                        windows_sys::Win32::System::Threading::SetEvent(pending.event_handle())
                    },
                    0
                );
                cancellation_reaper().retain(RetainedOperation {
                    _overlapped: pending.overlapped.take().unwrap(),
                    event: pending.event.take().unwrap(),
                    _buffer: pending.buffer.take().unwrap(),
                });
            }
            assert!(cancellation_reaper().wait_empty(Duration::from_secs(2)));
        }

        #[test]
        fn pending_write_cancellation_retains_kernel_referenced_buffer() {
            let (server, release, client) = connected_server();
            let never_cancelled = AtomicBool::new(false);
            write_raw_message(
                server.handle.as_raw_handle(),
                vec![b'a'; MAX_ELEVATION_FRAME_BYTES - 1],
                Duration::from_secs(2),
                &never_cancelled,
            )
            .unwrap();
            let cancelled = AtomicBool::new(true);
            assert_eq!(
                write_raw_message(
                    server.handle.as_raw_handle(),
                    vec![b'b'; MAX_ELEVATION_FRAME_BYTES - 1],
                    Duration::from_secs(1),
                    &cancelled,
                ),
                Err(ElevationError::Cancelled)
            );
            release.send(()).unwrap();
            client.join().unwrap();
        }

        #[test]
        fn real_pipe_completes_mutual_auth_and_one_command() {
            let server = NamedPipeServer::create().unwrap();
            let client = spawn_protocol_client(server.name.clone(), |handle, cancelled| {
                let frame = read_message(handle, Duration::from_secs(2), cancelled).unwrap();
                assert_eq!(
                    decode_command(&frame).unwrap(),
                    ElevationCommand::StartXray { generation_id: 9 }
                );
                write_message(
                    handle,
                    &ElevationResponse::Error {
                        code: BrokerErrorCode::NotReady,
                    },
                    Duration::from_secs(2),
                    cancelled,
                )
                .unwrap();
            });
            let cancelled = AtomicBool::new(false);
            let secret = SessionSecret::from_bytes([7; 32]);
            let pipe = server
                .authenticate(current_process_host(), &secret, &cancelled)
                .unwrap();
            assert_eq!(
                pipe.request(
                    &ElevationCommand::StartXray { generation_id: 9 },
                    &cancelled
                ),
                Ok(ElevationResponse::Error {
                    code: BrokerErrorCode::NotReady
                })
            );
            assert!(!pipe.is_terminal());
            client.join().unwrap();
        }

        #[test]
        fn eof_and_cancel_poison_production_pipe_against_reuse() {
            let server = NamedPipeServer::create().unwrap();
            let client = spawn_protocol_client(server.name.clone(), |_handle, _| {});
            let cancelled = AtomicBool::new(false);
            let pipe = server
                .authenticate(
                    current_process_host(),
                    &SessionSecret::from_bytes([8; 32]),
                    &cancelled,
                )
                .unwrap();
            client.join().unwrap();
            assert_eq!(
                pipe.request(&ElevationCommand::Diagnostics, &cancelled),
                Err(ElevationError::Disconnected)
            );
            assert!(pipe.is_terminal());
            assert_eq!(
                pipe.request(&ElevationCommand::Diagnostics, &cancelled),
                Err(ElevationError::Disconnected)
            );

            let server = NamedPipeServer::create().unwrap();
            let client = spawn_protocol_client(server.name.clone(), |handle, cancelled| {
                let _ = read_message(handle, Duration::from_secs(2), cancelled).unwrap();
                thread::sleep(Duration::from_millis(250));
            });
            let cancellation = Arc::new(AtomicBool::new(false));
            let pipe = server
                .authenticate(
                    current_process_host(),
                    &SessionSecret::from_bytes([9; 32]),
                    &cancellation,
                )
                .unwrap();
            let trigger = Arc::clone(&cancellation);
            let canceller = thread::spawn(move || {
                thread::sleep(Duration::from_millis(25));
                trigger.store(true, Ordering::Release);
            });
            assert_eq!(
                pipe.request(&ElevationCommand::Diagnostics, &cancellation),
                Err(ElevationError::Cancelled)
            );
            assert!(pipe.is_terminal());
            canceller.join().unwrap();
            client.join().unwrap();
        }

        #[test]
        fn trailing_or_oversized_response_terminally_poison_transport() {
            let server = NamedPipeServer::create().unwrap();
            let client = spawn_protocol_client(server.name.clone(), |handle, cancelled| {
                let _ = read_message(handle, Duration::from_secs(2), cancelled).unwrap();
                let mut trailing = encode_frame(&ElevationResponse::Stopped {
                    engine: multicore_core::elevation_protocol::ElevatedEngine::Xray,
                })
                .unwrap();
                trailing.push(0);
                write_raw_message(handle, trailing, Duration::from_secs(2), cancelled).unwrap();
            });
            let cancelled = AtomicBool::new(false);
            let pipe = server
                .authenticate(
                    current_process_host(),
                    &SessionSecret::from_bytes([10; 32]),
                    &cancelled,
                )
                .unwrap();
            assert_eq!(
                pipe.request(&ElevationCommand::Diagnostics, &cancelled),
                Err(ElevationError::Protocol)
            );
            assert!(pipe.is_terminal());
            client.join().unwrap();

            let server = NamedPipeServer::create().unwrap();
            let client = spawn_protocol_client(server.name.clone(), |handle, cancelled| {
                let _ = read_message(handle, Duration::from_secs(2), cancelled).unwrap();
                write_raw_message(
                    handle,
                    vec![b'x'; MAX_ELEVATION_FRAME_BYTES + 1],
                    Duration::from_secs(2),
                    cancelled,
                )
                .unwrap();
            });
            let pipe = server
                .authenticate(
                    current_process_host(),
                    &SessionSecret::from_bytes([11; 32]),
                    &cancelled,
                )
                .unwrap();
            assert_eq!(
                pipe.request(&ElevationCommand::Diagnostics, &cancelled),
                Err(ElevationError::Protocol)
            );
            assert!(pipe.is_terminal());
            client.join().unwrap();
        }

        #[test]
        fn semantically_mismatched_response_terminally_poison_transport() {
            let server = NamedPipeServer::create().unwrap();
            let client = spawn_protocol_client(server.name.clone(), |handle, cancelled| {
                let _ = read_message(handle, Duration::from_secs(2), cancelled).unwrap();
                write_message(
                    handle,
                    &ElevationResponse::Diagnostics {
                        xray_running: false,
                        mihomo_running: false,
                    },
                    Duration::from_secs(2),
                    cancelled,
                )
                .unwrap();
            });
            let cancelled = AtomicBool::new(false);
            let pipe = server
                .authenticate(
                    current_process_host(),
                    &SessionSecret::from_bytes([12; 32]),
                    &cancelled,
                )
                .unwrap();
            assert_eq!(
                pipe.request(
                    &ElevationCommand::StartXray { generation_id: 1 },
                    &cancelled,
                ),
                Err(ElevationError::Protocol)
            );
            assert!(pipe.is_terminal());
            assert_eq!(
                pipe.request(&ElevationCommand::Diagnostics, &cancelled),
                Err(ElevationError::Disconnected)
            );
            client.join().unwrap();
        }

        #[test]
        fn launcher_input_rejects_embedded_nul_before_win32() {
            use std::os::windows::ffi::OsStringExt;
            let value = std::ffi::OsString::from_wide(&[b'C' as u16, b':' as u16, 0, b'x' as u16]);
            assert!(os_str_contains_nul(&value));
            assert!(!os_str_contains_nul(OsStr::new(r"C:\safe\host.exe")));
        }
    }
}

#[cfg(windows)]
pub type ProductionElevatedControllerFactory =
    ElevatedControllerFactory<windows::WindowsBrokerConnector>;
#[cfg(windows)]
pub type ProductionElevatedController = ElevatedProcessController<windows::WindowsBrokerConnector>;

#[cfg(windows)]
pub fn production_factory() -> Result<ProductionElevatedControllerFactory, ElevationError> {
    let daemon = std::env::current_exe().map_err(|_| ElevationError::LaunchFailed)?;
    let host = packaged_core_host_from(&daemon)?;
    Ok(ElevatedControllerFactory::new(
        host,
        windows::WindowsBrokerConnector,
    ))
}

#[cfg(unix)]
pub mod unix {
    use super::*;
    use multicore_core::elevation_protocol::{
        ELEVATION_PROTOCOL_VERSION, decode_frame, read_stream_frame, write_stream_frame,
    };
    use std::{
        io::ErrorKind,
        os::unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
            process::ExitStatusExt,
        },
        process::{Child, Command, ExitStatus, Stdio},
        time::Instant,
    };

    const PKEXEC: &str = "/usr/bin/pkexec";
    const APPIMAGE_STAGE_PREFIX: &str = "appimage-";
    const APPIMAGE_MEMBERS: [(&str, &str); 3] = [
        ("runtime", CORE_HOST_EXECUTABLE),
        ("cores", "xray"),
        ("cores", "mihomo"),
    ];

    pub fn stage_appimage_host(host: &Path, cache_root: &Path) -> Result<PathBuf, ElevationError> {
        use sha2::{Digest, Sha256};
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

        let package = host
            .parent()
            .and_then(Path::parent)
            .ok_or(ElevationError::LaunchFailed)?;
        let mut hasher = Sha256::new();
        let mut members = Vec::with_capacity(APPIMAGE_MEMBERS.len());
        for (directory, name) in APPIMAGE_MEMBERS {
            let bytes = std::fs::read(package.join(directory).join(name))
                .map_err(|_| ElevationError::LaunchFailed)?;
            hasher.update(name.as_bytes());
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(&bytes);
            members.push((directory, name, bytes));
        }
        let digest = hasher.finalize();
        let stage_name = format!(
            "{APPIMAGE_STAGE_PREFIX}{}",
            digest[..16]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        let private_directory = |path: &Path| {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)
                .map_err(|_| ElevationError::Io)
        };
        private_directory(cache_root)?;
        let target = cache_root.join(&stage_name);
        let staged_host = target.join("runtime").join(CORE_HOST_EXECUTABLE);
        if !staged_host.is_file() {
            let temporary = cache_root.join(format!(".{stage_name}.{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&temporary);
            for (directory, name, bytes) in &members {
                private_directory(&temporary.join(directory))?;
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o755)
                    .open(temporary.join(directory).join(name))
                    .and_then(|mut file| file.write_all(bytes).and_then(|()| file.sync_all()))
                    .map_err(|_| ElevationError::Io)?;
            }
            if std::fs::rename(&temporary, &target).is_err() {
                let _ = std::fs::remove_dir_all(&temporary);
                if !staged_host.is_file() {
                    return Err(ElevationError::Io);
                }
            }
        }
        if let Ok(entries) = std::fs::read_dir(cache_root) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(APPIMAGE_STAGE_PREFIX) && name != stage_name {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        }
        Ok(staged_host)
    }
    const PKEXEC_DISMISSED: i32 = 126;
    const ACCEPT_POLL: Duration = Duration::from_millis(25);

    pub struct LaunchedHost {
        pub pid: u32,
        pub child: Option<Child>,
    }

    pub struct UnixBrokerTransport {
        stream: UnixStream,
        host: Mutex<Option<Child>>,
    }

    impl BrokerTransport for UnixBrokerTransport {
        fn request(&self, command: &ElevationCommand) -> Result<ElevationResponse, ElevationError> {
            let mut stream = &self.stream;
            self.stream
                .set_read_timeout(Some(broker_response_timeout(command)))
                .map_err(io_error)?;
            write_stream_frame(&mut stream, command).map_err(io_error)?;
            let frame = read_stream_frame(&mut stream).map_err(io_error)?;
            let response: ElevationResponse =
                decode_frame(&frame).map_err(|_| ElevationError::Protocol)?;
            if !response_matches_command(command, &response) {
                return Err(ElevationError::Protocol);
            }
            Ok(response)
        }
    }

    impl Drop for UnixBrokerTransport {
        fn drop(&mut self) {
            let _ = self.stream.shutdown(std::net::Shutdown::Both);
            if let Some(child) = self.host.lock().ok().and_then(|mut host| host.take()) {
                reap_in_background(child);
            }
        }
    }

    /// Waits for an exiting host off-thread so it never lingers as a zombie.
    fn reap_in_background(mut child: Child) {
        let _ = std::thread::Builder::new()
            .name("multicore-core-host-reaper".into())
            .spawn(move || {
                let _ = child.wait();
            });
    }

    /// Owns the launched host until a transport takes it; reaps it on every failed setup path.
    struct PendingHost(Option<Child>);

    impl Drop for PendingHost {
        fn drop(&mut self) {
            if let Some(child) = self.0.take() {
                reap_in_background(child);
            }
        }
    }

    #[derive(Clone, Copy, Debug, Default)]
    pub struct UnixBrokerConnector;

    impl BrokerConnector for UnixBrokerConnector {
        type Transport = UnixBrokerTransport;

        fn connect(&self, executable: &Path) -> Result<Self::Transport, ElevationError> {
            let runtime_dir =
                multicore_platform::dirs::user_runtime_dir().map_err(|_| ElevationError::Io)?;
            connect_with(
                &runtime_dir,
                |socket, server_pid| launch_with_pkexec(executable, socket, server_pid),
                0,
                BROKER_AUTHORIZATION_TIMEOUT,
            )
        }
    }

    fn launch_with_pkexec(
        executable: &Path,
        socket: &Path,
        server_pid: u32,
    ) -> Result<LaunchedHost, ElevationError> {
        let child = Command::new(PKEXEC)
            .arg(executable)
            .arg("--socket")
            .arg(socket)
            .arg("--protocol")
            .arg(ELEVATION_PROTOCOL_VERSION.to_string())
            .arg("--server-pid")
            .arg(server_pid.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| ElevationError::LaunchFailed)?;
        Ok(LaunchedHost {
            pid: child.id(),
            child: Some(child),
        })
    }

    struct SocketFile(PathBuf);

    impl Drop for SocketFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn io_error(error: std::io::Error) -> ElevationError {
        match error.kind() {
            ErrorKind::UnexpectedEof | ErrorKind::BrokenPipe | ErrorKind::ConnectionReset => {
                ElevationError::Disconnected
            }
            ErrorKind::WouldBlock | ErrorKind::TimedOut => ElevationError::TimedOut,
            ErrorKind::InvalidData => ElevationError::Protocol,
            _ => ElevationError::Io,
        }
    }

    pub(crate) fn launch_exit_error(status: ExitStatus) -> ElevationError {
        if status.code() == Some(PKEXEC_DISMISSED) {
            ElevationError::ElevationCancelled
        } else if status.signal().is_some() {
            ElevationError::Cancelled
        } else {
            ElevationError::LaunchFailed
        }
    }

    fn socket_name() -> Result<String, ElevationError> {
        let mut bytes = [0_u8; 24];
        getrandom::fill(&mut bytes).map_err(|_| ElevationError::Io)?;
        Ok(format!(
            "broker-{}.sock",
            bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ))
    }

    pub fn connect_with(
        runtime_dir: &Path,
        launch: impl FnOnce(&Path, u32) -> Result<LaunchedHost, ElevationError>,
        expected_peer_uid: u32,
        timeout: Duration,
    ) -> Result<UnixBrokerTransport, ElevationError> {
        let socket = SocketFile(runtime_dir.join(socket_name()?));
        let listener = UnixListener::bind(&socket.0).map_err(|_| ElevationError::Io)?;
        std::fs::set_permissions(&socket.0, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| ElevationError::Io)?;
        listener
            .set_nonblocking(true)
            .map_err(|_| ElevationError::Io)?;
        let secret = SessionSecret::generate().map_err(|_| ElevationError::Io)?;
        let mut host = launch(&socket.0, std::process::id())?;
        let host_pid = host.pid;
        let mut pending = PendingHost(host.child.take());
        let started = Instant::now();
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    if let Some(child) = pending.0.as_mut()
                        && let Some(status) = child.try_wait().map_err(|_| ElevationError::Io)?
                    {
                        pending.0 = None;
                        return Err(launch_exit_error(status));
                    }
                    if started.elapsed() >= timeout {
                        if let Some(child) = pending.0.as_mut()
                            && child.kill().is_ok()
                        {
                            let _ = child.wait();
                            pending.0 = None;
                        }
                        return Err(ElevationError::TimedOut);
                    }
                    std::thread::sleep(ACCEPT_POLL);
                }
                Err(_) => return Err(ElevationError::Io),
            }
        };
        drop(listener);
        drop(socket);
        stream
            .set_nonblocking(false)
            .map_err(|_| ElevationError::Io)?;
        stream
            .set_read_timeout(Some(BROKER_IO_TIMEOUT))
            .map_err(|_| ElevationError::Io)?;
        stream
            .set_write_timeout(Some(BROKER_IO_TIMEOUT))
            .map_err(|_| ElevationError::Io)?;
        let peer =
            multicore_platform::peer::peer_credentials(&stream).map_err(|_| ElevationError::Io)?;
        if peer.uid != expected_peer_uid || peer.pid != host_pid {
            return Err(ElevationError::PeerPidMismatch);
        }
        let mut writer = &stream;
        write_stream_frame(&mut writer, &secret.authentication()).map_err(io_error)?;
        let echoed = read_stream_frame(&mut writer).map_err(io_error)?;
        let echoed: Authentication =
            decode_frame(&echoed).map_err(|_| ElevationError::AuthenticationFailed)?;
        if !secret.verifies(&echoed) {
            return Err(ElevationError::AuthenticationFailed);
        }
        Ok(UnixBrokerTransport {
            stream,
            host: Mutex::new(pending.0.take()),
        })
    }
}

#[cfg(unix)]
pub type ProductionElevatedControllerFactory = ElevatedControllerFactory<unix::UnixBrokerConnector>;
#[cfg(unix)]
pub type ProductionElevatedController = ElevatedProcessController<unix::UnixBrokerConnector>;

#[cfg(unix)]
pub fn production_factory() -> Result<ProductionElevatedControllerFactory, ElevationError> {
    let daemon = std::env::current_exe().map_err(|_| ElevationError::LaunchFailed)?;
    let mut host = packaged_core_host_from(&daemon)?;
    if std::env::var_os("APPDIR")
        .map(PathBuf::from)
        .is_some_and(|appdir| appdir.is_absolute() && host.starts_with(appdir))
    {
        let cache = multicore_platform::dirs::user_cache_root()
            .map_err(|_| ElevationError::LaunchFailed)?;
        host = unix::stage_appimage_host(&host, &cache)?;
    }
    Ok(ElevatedControllerFactory::new(
        host,
        unix::UnixBrokerConnector,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use multicore_core::elevation_protocol::{BrokerErrorCode, ElevatedEngine, SessionSecret};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    struct FakeState {
        connects: AtomicUsize,
        fail_next_connect: AtomicBool,
        disconnect_next_request: AtomicBool,
        start_error: Mutex<Option<BrokerErrorCode>>,
        commands: Mutex<Vec<ElevationCommand>>,
    }
    #[derive(Clone)]
    struct FakeConnector(Arc<FakeState>);
    struct FakeTransport(Arc<FakeState>);
    impl BrokerConnector for FakeConnector {
        type Transport = FakeTransport;
        fn connect(&self, _: &Path) -> Result<Self::Transport, ElevationError> {
            self.0.connects.fetch_add(1, Ordering::Relaxed);
            if self.0.fail_next_connect.swap(false, Ordering::Relaxed) {
                return Err(ElevationError::ElevationCancelled);
            }
            Ok(FakeTransport(self.0.clone()))
        }
    }
    impl BrokerTransport for FakeTransport {
        fn request(&self, command: &ElevationCommand) -> Result<ElevationResponse, ElevationError> {
            if self
                .0
                .disconnect_next_request
                .swap(false, Ordering::Relaxed)
            {
                return Err(ElevationError::Disconnected);
            }
            self.0.commands.lock().unwrap().push(command.clone());
            if matches!(command, ElevationCommand::StartMihomo { .. })
                && let Some(code) = *self.0.start_error.lock().unwrap()
            {
                return Ok(ElevationResponse::Error { code });
            }
            Ok(match command {
                ElevationCommand::StartXray { .. } => ElevationResponse::Started {
                    engine: ElevatedEngine::Xray,
                },
                ElevationCommand::StartMihomo { .. } => ElevationResponse::Started {
                    engine: ElevatedEngine::Mihomo,
                },
                ElevationCommand::Stop { engine } => ElevationResponse::Stopped { engine: *engine },
                ElevationCommand::Diagnostics => ElevationResponse::ProcessDiagnostics {
                    xray: ElevatedRuntimeState::Ready,
                    mihomo: ElevatedRuntimeState::Ready,
                    tun: ElevatedRuntimeState::Ready,
                    logs: vec![multicore_core::elevation_protocol::ElevatedLogRecord {
                        id: 7,
                        timestamp_ms: 9,
                        engine: ElevatedEngine::Mihomo,
                        stream: ElevatedDiagnosticStream::Stderr,
                        message: "ready".into(),
                    }],
                },
                ElevationCommand::Shutdown => ElevationResponse::ShuttingDown,
            })
        }
    }

    fn fake_controller(state: Arc<FakeState>) -> ElevatedProcessController<FakeConnector> {
        ElevatedControllerFactory::new(PathBuf::from("fixed-host.exe"), FakeConnector(state))
            .controller(42)
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn supervisor_uses_one_lazy_host_and_preserves_order_across_reconnect() {
        let state = Arc::new(FakeState::default());
        let controller = Arc::new(fake_controller(state.clone()));
        let supervisor = multicore_core::Supervisor::new(controller.clone());
        assert_eq!(state.connects.load(Ordering::Relaxed), 0);
        supervisor.connect().await.unwrap();
        supervisor.disconnect().await.unwrap();
        supervisor.connect().await.unwrap();
        assert_eq!(state.connects.load(Ordering::Relaxed), 1);
        assert_eq!(
            *state.commands.lock().unwrap(),
            vec![
                ElevationCommand::StartXray { generation_id: 42 },
                ElevationCommand::StartMihomo { generation_id: 42 },
                ElevationCommand::Stop {
                    engine: ElevatedEngine::Mihomo
                },
                ElevationCommand::Stop {
                    engine: ElevatedEngine::Xray
                },
                ElevationCommand::StartXray { generation_id: 42 },
                ElevationCommand::StartMihomo { generation_id: 42 },
            ]
        );
        controller.shutdown();
        assert!(matches!(
            state.commands.lock().unwrap().last(),
            Some(ElevationCommand::Shutdown)
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn production_factory_reuses_one_host_across_generation_controllers() {
        let state = Arc::new(FakeState::default());
        let factory = ElevatedControllerFactory::new(
            PathBuf::from("fixed-host.exe"),
            FakeConnector(state.clone()),
        );
        let first = Arc::new(factory.controller(41).unwrap());
        multicore_core::Supervisor::new(first.clone())
            .connect()
            .await
            .unwrap();
        multicore_core::Supervisor::new(first)
            .disconnect()
            .await
            .unwrap();
        let second = Arc::new(factory.controller(42).unwrap());
        multicore_core::Supervisor::new(second)
            .connect()
            .await
            .unwrap();
        assert_eq!(state.connects.load(Ordering::Relaxed), 1);
        factory.shutdown();
        assert!(matches!(
            state.commands.lock().unwrap().last(),
            Some(ElevationCommand::Shutdown)
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn uac_cancel_and_host_eof_never_connect_and_remain_retryable() {
        let state = Arc::new(FakeState::default());
        state.fail_next_connect.store(true, Ordering::Relaxed);
        let controller = Arc::new(fake_controller(state.clone()));
        let supervisor = multicore_core::Supervisor::new(controller.clone());
        assert!(supervisor.connect().await.is_err());
        assert_eq!(state.connects.load(Ordering::Relaxed), 1);
        controller.start(Engine::Xray).await.unwrap();
        state.disconnect_next_request.store(true, Ordering::Relaxed);
        assert!(controller.start(Engine::Mihomo).await.is_err());
        controller.stop(Engine::Xray).await.unwrap();
        controller.start(Engine::Xray).await.unwrap();
        assert_eq!(state.connects.load(Ordering::Relaxed), 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn broker_diagnostics_translate_without_launching_a_new_host() {
        let state = Arc::new(FakeState::default());
        let controller = fake_controller(state.clone());
        assert_eq!(
            controller.diagnostics().await,
            ProcessDiagnostics::default()
        );
        controller.start(Engine::Xray).await.unwrap();
        let diagnostics = controller.diagnostics().await;
        assert_eq!(
            (diagnostics.xray, diagnostics.mihomo, diagnostics.tun),
            (
                RuntimeCheckState::Ready,
                RuntimeCheckState::Ready,
                RuntimeCheckState::Ready
            )
        );
        assert_eq!(diagnostics.logs[0].message, "ready");
        assert_eq!(state.connects.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn connect_failures_carry_the_elevation_or_core_cause() {
        let cancelled = Arc::new(FakeState::default());
        cancelled.fail_next_connect.store(true, Ordering::Relaxed);
        assert_eq!(
            multicore_core::Supervisor::new(Arc::new(fake_controller(cancelled)))
                .connect()
                .await,
            Err(multicore_core::SupervisorError::StartFailed {
                engine: Engine::Xray,
                rollback_failed: false,
                cause: StartFailureCause::ElevationCancelled,
            })
        );
        for (code, cause) in [
            (
                BrokerErrorCode::TimedOut,
                StartFailureCause::ReadinessTimedOut,
            ),
            (BrokerErrorCode::NotReady, StartFailureCause::CoreFailed),
        ] {
            let state = Arc::new(FakeState::default());
            *state.start_error.lock().unwrap() = Some(code);
            assert_eq!(
                multicore_core::Supervisor::new(Arc::new(fake_controller(state)))
                    .connect()
                    .await,
                Err(multicore_core::SupervisorError::StartFailed {
                    engine: Engine::Mihomo,
                    rollback_failed: false,
                    cause,
                })
            );
        }
        assert_eq!(
            ElevationError::Disconnected.start_failure_cause(),
            StartFailureCause::BrokerFailed
        );
        assert_eq!(
            ElevationError::LaunchFailed.start_failure_cause(),
            StartFailureCause::ElevationFailed
        );
    }

    #[test]
    fn start_commands_outlast_core_readiness_and_others_stay_short() {
        let margin = Duration::from_secs(5);
        for command in [
            ElevationCommand::StartXray { generation_id: 1 },
            ElevationCommand::StartMihomo { generation_id: 1 },
        ] {
            assert!(
                broker_response_timeout(&command)
                    >= multicore_core::CORE_READINESS_TIMEOUT + margin
            );
        }
        for command in [
            ElevationCommand::Stop {
                engine: ElevatedEngine::Xray,
            },
            ElevationCommand::Diagnostics,
            ElevationCommand::Shutdown,
        ] {
            assert_eq!(broker_response_timeout(&command), BROKER_IO_TIMEOUT);
        }
    }

    #[cfg(windows)]
    #[test]
    fn runtime_generation_requires_exact_fixed_width_name() {
        assert_eq!(
            runtime_generation_from_path(Path::new(r"C:\data\runtime-00000000000000000042")),
            Ok(42)
        );
        for invalid in [
            r"C:\data\runtime-42",
            r"C:\data\snapshot-00000000000000000042",
            r"C:\data\runtime-00000000000000000000",
        ] {
            assert!(runtime_generation_from_path(Path::new(invalid)).is_err());
        }
    }

    #[test]
    fn packaged_host_discovery_accepts_only_the_fixed_runtime_sibling() {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("runtime");
        std::fs::create_dir(&runtime).unwrap();
        let daemon = runtime.join(DAEMON_EXECUTABLE);
        let host = runtime.join(CORE_HOST_EXECUTABLE);
        std::fs::write(&daemon, b"daemon").unwrap();
        std::fs::write(&host, b"host").unwrap();
        assert_eq!(
            packaged_core_host_from(&daemon).unwrap(),
            std::fs::canonicalize(&host).unwrap()
        );
        assert!(packaged_core_host_from(&runtime.join("renamed-daemon.exe")).is_err());
        std::fs::remove_file(host).unwrap();
        assert!(packaged_core_host_from(&daemon).is_err());
    }

    #[test]
    fn wrong_pid_second_client_and_wrong_secret_fail_closed() {
        let mut wrong_pid = BrokerSession::new(7, SessionSecret::from_bytes([1; 32]));
        assert_eq!(
            wrong_pid.verify_peer(8),
            Err(ElevationError::PeerPidMismatch)
        );
        assert!(wrong_pid.is_closed());
        let mut second = BrokerSession::new(7, SessionSecret::from_bytes([1; 32]));
        second.verify_peer(7).unwrap();
        assert_eq!(second.verify_peer(7), Err(ElevationError::AlreadyConnected));
        let mut wrong_secret = BrokerSession::new(7, SessionSecret::from_bytes([1; 32]));
        wrong_secret.verify_peer(7).unwrap();
        assert_eq!(
            wrong_secret.authenticate(&SessionSecret::from_bytes([2; 32]).authentication()),
            Err(ElevationError::AuthenticationFailed)
        );
    }

    #[test]
    fn eof_closes_and_start_is_not_ready_until_task_three() {
        let mut session = BrokerSession::new(7, SessionSecret::from_bytes([1; 32]));
        session.verify_peer(7).unwrap();
        session.authenticate(&session.authentication()).unwrap();
        assert_eq!(
            session.command(&ElevationCommand::StartXray { generation_id: 4 }),
            ElevationResponse::Error {
                code: BrokerErrorCode::NotReady
            }
        );
        assert_eq!(
            session.command(&ElevationCommand::Stop {
                engine: ElevatedEngine::Xray
            }),
            ElevationResponse::Stopped {
                engine: ElevatedEngine::Xray
            }
        );
        assert_eq!(session.eof(), Err(ElevationError::Disconnected));
        assert!(session.is_closed());
    }

    #[test]
    fn uac_cancellation_is_distinct() {
        assert_eq!(
            map_shell_execute_error(1223),
            ElevationError::ElevationCancelled
        );
        assert_eq!(map_shell_execute_error(5), ElevationError::LaunchFailed);
    }

    #[cfg(unix)]
    mod unix_connector {
        use super::super::unix::{LaunchedHost, connect_with, launch_exit_error};
        use super::super::*;
        use multicore_core::elevation_protocol::{
            ElevatedEngine, decode_frame, read_stream_frame, write_stream_frame,
        };
        use std::{
            os::unix::{net::UnixStream, process::ExitStatusExt},
            path::PathBuf,
            process::ExitStatus,
            sync::mpsc,
            thread,
        };

        enum Host {
            Echo,
            WrongSecret,
            Respond(ElevationResponse),
        }

        fn fake_host(
            behaviour: Host,
            reported_pid: u32,
        ) -> (
            impl FnOnce(&Path, u32) -> Result<LaunchedHost, ElevationError>,
            mpsc::Receiver<PathBuf>,
        ) {
            let (sockets, observed) = mpsc::channel();
            let launch = move |socket: &Path, server_pid: u32| {
                assert_eq!(server_pid, std::process::id());
                let socket = socket.to_path_buf();
                let _ = sockets.send(socket.clone());
                thread::spawn(move || {
                    let mut stream = UnixStream::connect(&socket).unwrap();
                    let frame = read_stream_frame(&mut stream).unwrap();
                    let authentication: Authentication = decode_frame(&frame).unwrap();
                    match behaviour {
                        Host::WrongSecret => {
                            let other = SessionSecret::generate().unwrap();
                            let _ = write_stream_frame(&mut stream, &other.authentication());
                        }
                        Host::Echo | Host::Respond(_) => {
                            write_stream_frame(&mut stream, &authentication).unwrap();
                        }
                    }
                    if let Host::Respond(response) = behaviour
                        && read_stream_frame(&mut stream).is_ok()
                    {
                        write_stream_frame(&mut stream, &response).unwrap();
                    }
                    let _ = read_stream_frame(&mut stream);
                });
                Ok(LaunchedHost {
                    pid: reported_pid,
                    child: None,
                })
            };
            (launch, observed)
        }

        /// macOS caps a socket path at 104 bytes, which a socket under the per-user `TMPDIR`
        /// (`/var/folders/…/T/`) exceeds; `/tmp` keeps it short.
        fn runtime_tempdir() -> std::io::Result<tempfile::TempDir> {
            if cfg!(target_os = "macos") {
                tempfile::tempdir_in("/tmp")
            } else {
                tempfile::tempdir()
            }
        }

        fn uid() -> u32 {
            rustix::process::getuid().as_raw()
        }

        #[test]
        fn authenticated_session_serves_matching_responses_and_removes_the_socket() {
            let runtime = runtime_tempdir().unwrap();
            let (launch, sockets) = fake_host(
                Host::Respond(ElevationResponse::Stopped {
                    engine: ElevatedEngine::Xray,
                }),
                std::process::id(),
            );
            let transport =
                connect_with(runtime.path(), launch, uid(), Duration::from_secs(5)).unwrap();
            let socket = sockets.recv().unwrap();
            assert!(socket.starts_with(runtime.path()));
            let name = socket.file_name().unwrap().to_str().unwrap();
            assert!(name.starts_with("broker-") && name.ends_with(".sock"));
            assert_eq!(name.len(), "broker-".len() + 48 + ".sock".len());
            assert!(!socket.exists());
            assert_eq!(
                transport
                    .request(&ElevationCommand::Stop {
                        engine: ElevatedEngine::Xray
                    })
                    .unwrap(),
                ElevationResponse::Stopped {
                    engine: ElevatedEngine::Xray
                }
            );
        }

        #[test]
        fn mismatched_responses_are_protocol_errors() {
            let runtime = runtime_tempdir().unwrap();
            let (launch, _) = fake_host(
                Host::Respond(ElevationResponse::ShuttingDown),
                std::process::id(),
            );
            let transport =
                connect_with(runtime.path(), launch, uid(), Duration::from_secs(5)).unwrap();
            assert_eq!(
                transport
                    .request(&ElevationCommand::Stop {
                        engine: ElevatedEngine::Mihomo
                    })
                    .err(),
                Some(ElevationError::Protocol)
            );
        }

        #[test]
        fn wrong_secret_uid_or_pid_fail_closed() {
            let runtime = runtime_tempdir().unwrap();
            let (launch, _) = fake_host(Host::WrongSecret, std::process::id());
            assert_eq!(
                connect_with(runtime.path(), launch, uid(), Duration::from_secs(5)).err(),
                Some(ElevationError::AuthenticationFailed)
            );
            let (launch, _) = fake_host(Host::Echo, std::process::id());
            assert_eq!(
                connect_with(runtime.path(), launch, uid() + 1, Duration::from_secs(5)).err(),
                Some(ElevationError::PeerPidMismatch)
            );
            let (launch, _) = fake_host(Host::Echo, std::process::id() + 1);
            assert_eq!(
                connect_with(runtime.path(), launch, uid(), Duration::from_secs(5)).err(),
                Some(ElevationError::PeerPidMismatch)
            );
            assert_eq!(std::fs::read_dir(runtime.path()).unwrap().count(), 0);
        }

        #[test]
        fn early_launcher_exit_and_silence_are_reported_without_leaking_sockets() {
            let runtime = runtime_tempdir().unwrap();
            for (code, expected) in [
                (126, ElevationError::ElevationCancelled),
                (127, ElevationError::LaunchFailed),
                (1, ElevationError::LaunchFailed),
            ] {
                let result = connect_with(
                    runtime.path(),
                    |_, _| {
                        let child = std::process::Command::new("/bin/sh")
                            .arg("-c")
                            .arg(format!("exit {code}"))
                            .spawn()
                            .unwrap();
                        Ok(LaunchedHost {
                            pid: child.id(),
                            child: Some(child),
                        })
                    },
                    uid(),
                    Duration::from_secs(5),
                );
                assert_eq!(result.err(), Some(expected), "{code}");
            }
            let result = connect_with(
                runtime.path(),
                |_, _| {
                    Ok(LaunchedHost {
                        pid: std::process::id(),
                        child: None,
                    })
                },
                uid(),
                Duration::from_millis(200),
            );
            assert_eq!(result.err(), Some(ElevationError::TimedOut));
            assert_eq!(std::fs::read_dir(runtime.path()).unwrap().count(), 0);
        }

        #[test]
        fn authorization_timeout_kills_the_waiting_launcher() {
            let runtime = runtime_tempdir().unwrap();
            let launched = std::sync::Arc::new(std::sync::Mutex::new(None));
            let recorded = launched.clone();
            let result = connect_with(
                runtime.path(),
                move |_, _| {
                    let child = std::process::Command::new("/bin/sh")
                        .arg("-c")
                        .arg("exec sleep 60")
                        .spawn()
                        .unwrap();
                    *recorded.lock().unwrap() = Some(child.id());
                    Ok(LaunchedHost {
                        pid: child.id(),
                        child: Some(child),
                    })
                },
                uid(),
                Duration::from_millis(200),
            );
            assert_eq!(result.err(), Some(ElevationError::TimedOut));
            let pid = launched.lock().unwrap().unwrap();
            assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
        }

        #[test]
        fn failed_session_setup_reaps_the_launched_host() {
            let runtime = runtime_tempdir().unwrap();
            let launched = std::sync::Arc::new(std::sync::Mutex::new(None));
            let recorded = launched.clone();
            // The fake connects from this process, so the launched child's PID never matches.
            let (connect_from_self, _) = fake_host(Host::Echo, std::process::id());
            let result = connect_with(
                runtime.path(),
                move |socket, server_pid| {
                    let mut child = std::process::Command::new("/bin/sh")
                        .arg("-c")
                        .arg("exec sleep 0.2")
                        .spawn()
                        .unwrap();
                    *recorded.lock().unwrap() = Some(child.id());
                    if let Err(error) = connect_from_self(socket, server_pid) {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(error);
                    }
                    Ok(LaunchedHost {
                        pid: child.id(),
                        child: Some(child),
                    })
                },
                uid(),
                Duration::from_secs(5),
            );
            assert_eq!(result.err(), Some(ElevationError::PeerPidMismatch));
            let pid = launched.lock().unwrap().unwrap();
            let entry = PathBuf::from(format!("/proc/{pid}"));
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while entry.exists() && std::time::Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            assert!(!entry.exists(), "launched host was left as a zombie");
        }

        fn appimage_package(root: &Path, host: &[u8]) -> PathBuf {
            use std::os::unix::fs::PermissionsExt;
            let runtime = root.join("mount").join("usr/lib/multicore/runtime");
            let cores = root.join("mount").join("usr/lib/multicore/cores");
            std::fs::create_dir_all(&runtime).unwrap();
            std::fs::create_dir_all(&cores).unwrap();
            for (path, bytes) in [
                (runtime.join("multicore-core-host"), host),
                (cores.join("xray"), &b"xray"[..]),
                (cores.join("mihomo"), &b"mihomo"[..]),
            ] {
                std::fs::write(&path, bytes).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            runtime.join("multicore-core-host")
        }

        #[test]
        fn appimage_host_is_staged_once_per_build_outside_the_mount() {
            use super::super::unix::stage_appimage_host;
            use std::os::unix::fs::PermissionsExt;

            let temp = runtime_tempdir().unwrap();
            let cache = temp.path().join("cache");
            let host = appimage_package(temp.path(), b"host-one");
            let staged = stage_appimage_host(&host, &cache).unwrap();
            assert!(staged.starts_with(&cache));
            assert!(staged.ends_with("runtime/multicore-core-host"));
            let package = staged.parent().unwrap().parent().unwrap();
            assert_eq!(std::fs::read(&staged).unwrap(), b"host-one");
            assert_eq!(std::fs::read(package.join("cores/xray")).unwrap(), b"xray");
            assert_eq!(
                std::fs::read(package.join("cores/mihomo")).unwrap(),
                b"mihomo"
            );
            for path in [&staged, &package.join("cores/xray")] {
                let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o755);
            }
            let mode = std::fs::metadata(package).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
            assert_eq!(stage_appimage_host(&host, &cache).unwrap(), staged);

            std::fs::write(&host, b"host-two").unwrap();
            let rebuilt = stage_appimage_host(&host, &cache).unwrap();
            assert_ne!(rebuilt, staged);
            assert_eq!(std::fs::read(&rebuilt).unwrap(), b"host-two");
            assert!(!package.exists());
            assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 1);
        }

        #[test]
        fn signalled_launcher_is_reported_as_cancelled() {
            assert_eq!(
                launch_exit_error(ExitStatus::from_raw(9)),
                ElevationError::Cancelled
            );
        }
    }
}
