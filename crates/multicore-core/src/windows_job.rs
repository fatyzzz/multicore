use std::{
    ffi::{OsStr, OsString, c_void},
    future::Future,
    io::Read,
    mem::{size_of, zeroed},
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    pin::Pin,
    ptr,
    sync::Arc,
    time::Duration,
};

use windows_sys::Win32::{
    Foundation::{HANDLE, HANDLE_FLAG_INHERIT, STILL_ACTIVE, SetHandleInformation},
    Security::SECURITY_ATTRIBUTES,
    System::{
        JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        },
        Pipes::CreatePipe,
        Threading::{
            CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
            DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess,
            INFINITE, InitializeProcThreadAttributeList, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION, STARTF_USESTDHANDLES,
            STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
        },
    },
};

use crate::{
    CommandSpec, CoreLogBuffer, DiagnosticStream, Engine, ManagedChild, ProcessError,
    ProcessLauncher, RuntimeCheckState, RuntimePaths, TokioProcessLauncher,
    sidecar::BoundedLogDecoder,
};

/// Windows launcher whose single job is the lifetime owner of every launched core.
///
/// Creation uses `PROC_THREAD_ATTRIBUTE_JOB_LIST`, so child code never executes outside the job.
/// The package remains user-writable and unsigned; validation prevents path substitution but is not
/// a publisher trust boundary. Release signing remains required.
#[derive(Clone)]
pub struct WindowsJobLauncher {
    job: Arc<Job>,
    logs: Arc<CoreLogBuffer>,
    readiness: Option<TokioProcessLauncher>,
    readiness_timeout: Duration,
}

struct Job(OwnedHandle);

// Owned kernel handles can be moved/shared; access is only through thread-safe kernel operations.
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

impl WindowsJobLauncher {
    pub fn new(logs: Arc<CoreLogBuffer>) -> Result<Self, std::io::Error> {
        let raw = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if raw.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = unsafe {
            SetInformationJobObject(
                handle.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self {
            job: Arc::new(Job(handle)),
            logs,
            readiness: None,
            readiness_timeout: Duration::from_secs(12),
        })
    }

    pub fn for_runtime(
        paths: RuntimePaths,
        logs: Arc<CoreLogBuffer>,
        timeout: Duration,
    ) -> Result<Self, std::io::Error> {
        let mut launcher = Self::new(logs.clone())?;
        launcher.readiness = Some(TokioProcessLauncher::for_runtime_with_logs(
            paths, timeout, logs,
        ));
        launcher.readiness_timeout = timeout;
        Ok(launcher)
    }

    fn launch_sync(&self, command: &CommandSpec) -> Result<WindowsJobManagedChild, ProcessError> {
        let (stdout_read, stdout_write) =
            pipe().map_err(|_| ProcessError::LaunchFailed(command.engine))?;
        let (stderr_read, stderr_write) =
            pipe().map_err(|_| ProcessError::LaunchFailed(command.engine))?;
        let inherited = [stdout_write.as_raw_handle(), stderr_write.as_raw_handle()];
        let job = [self.job.0.as_raw_handle()];
        let mut attributes =
            AttributeList::new(2).map_err(|_| ProcessError::LaunchFailed(command.engine))?;
        attributes
            .set(
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                job.as_ptr().cast(),
                size_of::<HANDLE>(),
            )
            .map_err(|_| ProcessError::LaunchFailed(command.engine))?;
        attributes
            .set(
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                inherited.as_ptr().cast(),
                size_of_val(&inherited),
            )
            .map_err(|_| ProcessError::LaunchFailed(command.engine))?;

        let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = ptr::null_mut();
        startup.StartupInfo.hStdOutput = stdout_write.as_raw_handle();
        startup.StartupInfo.hStdError = stderr_write.as_raw_handle();
        startup.lpAttributeList = attributes.pointer();
        let mut info: PROCESS_INFORMATION = unsafe { zeroed() };
        let application = wide_nul(command.program.as_os_str());
        let mut command_line = wide_nul(&build_command_line(&command.program, &command.args));
        let environment = sanitized_environment();
        let created = unsafe {
            CreateProcessW(
                application.as_ptr(),
                command_line.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                1,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
                environment.as_ptr().cast::<c_void>(),
                ptr::null(),
                (&raw const startup.StartupInfo),
                &mut info,
            )
        };
        if created == 0 {
            return Err(ProcessError::LaunchFailed(command.engine));
        }
        let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess) };
        let thread = unsafe { OwnedHandle::from_raw_handle(info.hThread) };
        drop(thread);
        drop(stdout_write);
        drop(stderr_write);
        spawn_reader(
            self.logs.clone(),
            command.engine,
            DiagnosticStream::Stdout,
            stdout_read,
        );
        spawn_reader(
            self.logs.clone(),
            command.engine,
            DiagnosticStream::Stderr,
            stderr_read,
        );
        Ok(WindowsJobManagedChild {
            engine: command.engine,
            process,
        })
    }
}

pub struct WindowsJobManagedChild {
    engine: Engine,
    process: OwnedHandle,
}
unsafe impl Send for WindowsJobManagedChild {}

impl ManagedChild for WindowsJobManagedChild {
    fn has_exited(&mut self) -> Result<bool, ProcessError> {
        let mut code = 0;
        if unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &mut code) } == 0 {
            return Err(ProcessError::InspectFailed(self.engine));
        }
        Ok(code != STILL_ACTIVE as u32)
    }
    fn stop<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), ProcessError>> + Send + 'a>> {
        Box::pin(async move {
            if self.has_exited()? {
                return Ok(());
            }
            if unsafe { TerminateProcess(self.process.as_raw_handle(), 1) } == 0 {
                return Err(ProcessError::StopFailed(self.engine));
            }
            unsafe {
                WaitForSingleObject(self.process.as_raw_handle(), INFINITE);
            }
            Ok(())
        })
    }
}

impl ProcessLauncher for WindowsJobLauncher {
    type Child = WindowsJobManagedChild;
    fn launch<'a>(
        &'a self,
        command: CommandSpec,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Child, ProcessError>> + Send + 'a>> {
        Box::pin(async move { self.launch_sync(&command) })
    }
    fn diagnostic_logs(&self) -> Vec<crate::CoreLogRecord> {
        self.logs.snapshot()
    }
    fn wait_until_ready<'a>(
        &'a self,
        command: &'a CommandSpec,
        child: &'a mut Self::Child,
    ) -> Pin<Box<dyn Future<Output = Result<(), ProcessError>> + Send + 'a>> {
        Box::pin(async move {
            let Some(readiness) = &self.readiness else {
                return if child.has_exited()? {
                    Err(ProcessError::ReadinessFailed(command.engine))
                } else {
                    Ok(())
                };
            };
            let deadline = tokio::time::Instant::now() + self.readiness_timeout;
            loop {
                if child.has_exited()? {
                    return Err(ProcessError::ReadinessFailed(command.engine));
                }
                if readiness.current_ready(command.engine).await {
                    return Ok(());
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(ProcessError::ReadinessFailed(command.engine));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
    }
    fn current_ready<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            match &self.readiness {
                Some(r) => r.current_ready(engine).await,
                None => true,
            }
        })
    }
    fn tun_check<'a>(&'a self) -> Pin<Box<dyn Future<Output = RuntimeCheckState> + Send + 'a>> {
        Box::pin(async move {
            match &self.readiness {
                Some(r) => r.tun_check().await,
                None => RuntimeCheckState::Unsupported,
            }
        })
    }
}

fn pipe() -> std::io::Result<(OwnedHandle, OwnedHandle)> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 1,
    };
    let (mut read, mut write) = (ptr::null_mut(), ptr::null_mut());
    if unsafe { CreatePipe(&mut read, &mut write, &sa, 0) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let read = unsafe { OwnedHandle::from_raw_handle(read) };
    let write = unsafe { OwnedHandle::from_raw_handle(write) };
    if unsafe { SetHandleInformation(read.as_raw_handle(), HANDLE_FLAG_INHERIT, 0) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((read, write))
}

fn spawn_reader(
    logs: Arc<CoreLogBuffer>,
    engine: Engine,
    stream: DiagnosticStream,
    handle: OwnedHandle,
) {
    std::thread::spawn(move || {
        let mut file = std::fs::File::from(handle);
        let mut chunk = [0_u8; 4096];
        let mut decoder = BoundedLogDecoder::default();
        loop {
            match file.read(&mut chunk) {
                Ok(0) => {
                    decoder.finish(|line| logs.push(engine, stream, line));
                    break;
                }
                Ok(read) => decoder.feed(&chunk[..read], |line| logs.push(engine, stream, line)),
                Err(_) => break,
            }
        }
    });
}

struct AttributeList {
    storage: Vec<usize>,
}
impl AttributeList {
    fn new(count: u32) -> std::io::Result<Self> {
        let mut bytes = 0;
        unsafe {
            InitializeProcThreadAttributeList(ptr::null_mut(), count, 0, &mut bytes);
        }
        if bytes == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut storage = vec![0usize; bytes.div_ceil(size_of::<usize>())];
        if unsafe {
            InitializeProcThreadAttributeList(storage.as_mut_ptr().cast(), count, 0, &mut bytes)
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { storage })
    }
    fn pointer(&mut self) -> windows_sys::Win32::System::Threading::LPPROC_THREAD_ATTRIBUTE_LIST {
        self.storage.as_mut_ptr().cast()
    }
    fn set(&mut self, attribute: usize, value: *const c_void, bytes: usize) -> std::io::Result<()> {
        if unsafe {
            UpdateProcThreadAttribute(
                self.pointer(),
                0,
                attribute,
                value,
                bytes,
                ptr::null_mut(),
                ptr::null(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}
impl Drop for AttributeList {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.pointer());
        }
    }
}

fn wide_nul(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

fn build_command_line(program: &std::path::Path, args: &[OsString]) -> OsString {
    let mut result = quote_windows(program.as_os_str());
    for arg in args {
        result.push(" ");
        result.push(quote_windows(arg));
    }
    result
}

fn quote_windows(value: &OsStr) -> OsString {
    let text = value.to_string_lossy();
    if !text.is_empty() && !text.contains([' ', '\t', '"']) {
        return value.to_owned();
    }
    let mut out = String::from("\"");
    let mut slashes = 0;
    for ch in text.chars() {
        if ch == '\\' {
            slashes += 1;
            continue;
        }
        if ch == '"' {
            out.push_str(&"\\".repeat(slashes * 2 + 1));
            out.push('"');
        } else {
            out.push_str(&"\\".repeat(slashes));
            out.push(ch);
        }
        slashes = 0;
    }
    out.push_str(&"\\".repeat(slashes * 2));
    out.push('"');
    OsString::from(out)
}

fn sanitized_environment() -> Vec<u16> {
    const ALLOWED: &[&str] = &[
        "SYSTEMROOT",
        "WINDIR",
        "PATH",
        "PATHEXT",
        "TEMP",
        "TMP",
        "USERPROFILE",
        "LOCALAPPDATA",
        "APPDATA",
        "PROGRAMDATA",
    ];
    let mut entries = std::env::vars_os()
        .filter_map(|(key, value)| {
            let key_text = key.to_string_lossy();
            ALLOWED
                .iter()
                .any(|allowed| key_text.eq_ignore_ascii_case(allowed))
                .then(|| format!("{}={}", key_text, value.to_string_lossy()))
        })
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.to_ascii_uppercase());
    let mut block = Vec::new();
    for entry in entries {
        block.extend(OsStr::new(&entry).encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::JobObjects::IsProcessInJob;
    #[test]
    fn quotes_paths_with_spaces_and_trailing_backslashes() {
        let line = build_command_line(
            std::path::Path::new(r"C:\Program Files\core.exe"),
            &[OsString::from(r"C:\config dir\")],
        );
        assert_eq!(
            line.to_string_lossy(),
            r#""C:\Program Files\core.exe" "C:\config dir\\""#
        );
    }
    #[test]
    fn environment_excludes_daemon_credentials() {
        unsafe { std::env::set_var("MULTICORE_DAEMON_TOKEN", "never-inherit") };
        let block = String::from_utf16_lossy(&sanitized_environment());
        assert!(!block.contains("never-inherit"));
    }
    #[tokio::test]
    async fn child_is_created_inside_job_and_job_close_reaps_it() {
        let launcher = WindowsJobLauncher::new(Arc::new(CoreLogBuffer::default())).unwrap();
        let mut child = launcher
            .launch(CommandSpec {
                engine: Engine::Xray,
                program: std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe"),
                args: vec!["/D".into(), "/C".into(), "ping -n 30 127.0.0.1 >NUL".into()],
            })
            .await
            .unwrap();
        let mut inside = 0;
        assert_ne!(
            unsafe {
                IsProcessInJob(
                    child.process.as_raw_handle(),
                    launcher.job.0.as_raw_handle(),
                    &mut inside,
                )
            },
            0
        );
        assert_ne!(inside, 0);
        drop(launcher);
        for _ in 0..100 {
            if child.has_exited().unwrap() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("child survived closing its owning job");
    }
}
