use std::{fmt, path::Path, time::Duration};

#[cfg(test)]
use multicore_core::elevation_protocol::BrokerErrorCode;
use multicore_core::elevation_protocol::{
    Authentication, ElevationCommand, ElevationResponse, SessionSecret, response_matches_command,
};
use zeroize::Zeroizing;

pub const BROKER_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
pub const BROKER_IO_TIMEOUT: Duration = Duration::from_secs(5);

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
        })
    }
}

impl std::error::Error for ElevationError {}

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
                .and_then(|()| read_message(pipe.as_raw_handle(), BROKER_IO_TIMEOUT, cancelled))
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

    fn read_message(
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

    fn write_raw_message(
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

#[cfg(test)]
mod tests {
    use super::*;
    use multicore_core::elevation_protocol::{BrokerErrorCode, ElevatedEngine, SessionSecret};

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
}
