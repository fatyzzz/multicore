use std::{
    collections::{HashMap, VecDeque},
    ffi::OsString,
    fs::{self, File, OpenOptions},
    future::Future,
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    net::TcpStream,
    process::Child,
    sync::Mutex,
    time::{Duration, Instant, sleep},
};

use crate::{Engine, ProcessController};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreLogRecord {
    pub id: u64,
    pub timestamp_ms: u64,
    pub engine: Engine,
    pub stream: DiagnosticStream,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RuntimeCheckState {
    #[default]
    Stopped,
    Starting,
    Ready,
    Failed,
    Unsupported,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessDiagnostics {
    pub xray: RuntimeCheckState,
    pub mihomo: RuntimeCheckState,
    pub tun: RuntimeCheckState,
    pub logs: Vec<CoreLogRecord>,
}

#[derive(Debug)]
struct CoreLogState {
    records: VecDeque<CoreLogRecord>,
}

#[derive(Debug)]
pub struct CoreLogBuffer {
    state: StdMutex<CoreLogState>,
    persistent: Option<StdMutex<PersistentLogSink>>,
    next_id: AtomicU64,
    max_records: usize,
    max_chars: usize,
}

const MAX_PERSISTENT_LOG_BYTES: u64 = 1_048_576;

#[derive(Debug)]
struct PersistentLogSink {
    file: File,
    bytes_written: u64,
    disabled: bool,
}

impl PersistentLogSink {
    fn create(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?;
        Ok(Self {
            file,
            bytes_written: 0,
            disabled: false,
        })
    }

    fn write(&mut self, record: &CoreLogRecord) {
        if self.disabled {
            return;
        }
        let engine = match record.engine {
            Engine::Xray => "xray",
            Engine::Mihomo => "mihomo",
        };
        let stream = match record.stream {
            DiagnosticStream::Stdout => "stdout",
            DiagnosticStream::Stderr => "stderr",
        };
        let message: String = record
            .message
            .chars()
            .map(|character| match character {
                '\r' | '\n' | '\t' => ' ',
                other => other,
            })
            .collect();
        let line = format!("{}\t{engine}\t{stream}\t{message}\n", record.timestamp_ms);
        let bytes = line.as_bytes();
        let result = (|| -> std::io::Result<()> {
            if self.bytes_written.saturating_add(bytes.len() as u64) > MAX_PERSISTENT_LOG_BYTES {
                self.file.set_len(0)?;
                self.file.seek(SeekFrom::Start(0))?;
                self.bytes_written = 0;
            }
            self.file.write_all(bytes)?;
            self.file.flush()?;
            self.bytes_written = self.bytes_written.saturating_add(bytes.len() as u64);
            Ok(())
        })();
        if result.is_err() {
            self.disabled = true;
        }
    }
}

impl Default for CoreLogBuffer {
    fn default() -> Self {
        Self::with_limits(256, 2_048)
    }
}

impl CoreLogBuffer {
    pub fn with_limits(max_records: usize, max_chars: usize) -> Self {
        Self {
            state: StdMutex::new(CoreLogState {
                records: VecDeque::with_capacity(max_records),
            }),
            persistent: None,
            next_id: AtomicU64::new(1),
            max_records: max_records.max(1),
            max_chars: max_chars.max(1),
        }
    }

    pub fn persistent(path: impl AsRef<Path>) -> std::io::Result<Self> {
        Ok(Self {
            state: StdMutex::new(CoreLogState {
                records: VecDeque::with_capacity(256),
            }),
            persistent: Some(StdMutex::new(PersistentLogSink::create(path.as_ref())?)),
            next_id: AtomicU64::new(1),
            max_records: 256,
            max_chars: 2_048,
        })
    }

    pub fn persistent_or_memory(path: impl AsRef<Path>) -> Self {
        Self::persistent(path).unwrap_or_default()
    }

    pub fn push(&self, engine: Engine, stream: DiagnosticStream, message: &str) {
        let safe = crate::event::redact_text(message);
        let message = truncate_chars(&safe, self.max_chars);
        if message.trim().is_empty() {
            return;
        }
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        let record = CoreLogRecord {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            timestamp_ms,
            engine,
            stream,
            message,
        };
        let mut state = self.state.lock().expect("core log buffer lock poisoned");
        while state.records.len() >= self.max_records {
            state.records.pop_front();
        }
        state.records.push_back(record.clone());
        drop(state);
        if let Some(persistent) = &self.persistent {
            persistent
                .lock()
                .expect("persistent core log lock poisoned")
                .write(&record);
        }
    }

    pub fn snapshot(&self) -> Vec<CoreLogRecord> {
        self.state
            .lock()
            .expect("core log buffer lock poisoned")
            .records
            .iter()
            .cloned()
            .collect()
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    value.chars().take(max_chars).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePaths {
    pub xray_binary: PathBuf,
    pub mihomo_binary: PathBuf,
    pub xray_config: PathBuf,
    pub mihomo_config: PathBuf,
}

impl RuntimePaths {
    pub fn command(&self, engine: Engine) -> CommandSpec {
        match engine {
            Engine::Xray => CommandSpec {
                engine,
                program: self.xray_binary.clone(),
                args: vec![
                    OsString::from("run"),
                    OsString::from("-config"),
                    self.xray_config.clone().into_os_string(),
                ],
            },
            Engine::Mihomo => CommandSpec {
                engine,
                program: self.mihomo_binary.clone(),
                args: vec![
                    OsString::from("-f"),
                    self.mihomo_config.clone().into_os_string(),
                ],
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub engine: Engine,
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ProcessError {
    #[error("failed to launch {0:?}")]
    LaunchFailed(Engine),
    #[error("failed to inspect {0:?}")]
    InspectFailed(Engine),
    #[error("failed to stop {0:?}")]
    StopFailed(Engine),
    #[error("{0:?} did not become ready")]
    ReadinessFailed(Engine),
}

pub trait ManagedChild: Send {
    fn has_exited(&mut self) -> Result<bool, ProcessError>;

    fn stop<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), ProcessError>> + Send + 'a>>;
}

pub trait ProcessLauncher: Send + Sync {
    type Child: ManagedChild;

    fn launch<'a>(
        &'a self,
        command: CommandSpec,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Child, ProcessError>> + Send + 'a>>;

    fn wait_until_ready<'a>(
        &'a self,
        command: &'a CommandSpec,
        child: &'a mut Self::Child,
    ) -> Pin<Box<dyn Future<Output = Result<(), ProcessError>> + Send + 'a>> {
        Box::pin(async move {
            if child.has_exited()? {
                Err(ProcessError::ReadinessFailed(command.engine))
            } else {
                Ok(())
            }
        })
    }

    fn current_ready<'a>(
        &'a self,
        _engine: Engine,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async { true })
    }

    fn diagnostic_logs(&self) -> Vec<CoreLogRecord> {
        Vec::new()
    }

    fn tun_check<'a>(&'a self) -> Pin<Box<dyn Future<Output = RuntimeCheckState> + Send + 'a>> {
        Box::pin(async { RuntimeCheckState::Unsupported })
    }
}

#[derive(Debug, Clone, Default)]
pub struct TokioProcessLauncher {
    logs: Arc<CoreLogBuffer>,
    runtime: Option<RuntimeReadiness>,
}

#[derive(Debug, Clone)]
struct RuntimeReadiness {
    paths: RuntimePaths,
    timeout: Duration,
}

impl TokioProcessLauncher {
    pub fn for_runtime(paths: RuntimePaths, timeout: Duration) -> Self {
        Self::for_runtime_with_logs(paths, timeout, Arc::default())
    }

    pub fn for_runtime_with_logs(
        paths: RuntimePaths,
        timeout: Duration,
        logs: Arc<CoreLogBuffer>,
    ) -> Self {
        Self {
            logs,
            runtime: Some(RuntimeReadiness { paths, timeout }),
        }
    }

    pub fn logs(&self) -> Arc<CoreLogBuffer> {
        self.logs.clone()
    }

    async fn runtime_ready(&self, engine: Engine) -> bool {
        let Some(runtime) = &self.runtime else {
            return true;
        };
        match engine {
            Engine::Xray => {
                let Ok(addresses) = xray_loopback_inbounds(&runtime.paths.xray_config) else {
                    return false;
                };
                if addresses.is_empty() {
                    return false;
                }
                for address in addresses {
                    if TcpStream::connect(address).await.is_err() {
                        return false;
                    }
                }
                true
            }
            Engine::Mihomo => {
                let Ok((controller, device)) =
                    mihomo_readiness_targets(&runtime.paths.mihomo_config)
                else {
                    return false;
                };
                TcpStream::connect(controller).await.is_ok() && tun_device_is_up(&device).await
            }
        }
    }
}

pub struct TokioManagedChild {
    engine: Engine,
    child: Child,
}

impl ManagedChild for TokioManagedChild {
    fn has_exited(&mut self) -> Result<bool, ProcessError> {
        self.child
            .try_wait()
            .map(|status| status.is_some())
            .map_err(|_| ProcessError::InspectFailed(self.engine))
    }

    fn stop<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), ProcessError>> + Send + 'a>> {
        Box::pin(async move {
            if self.has_exited()? {
                return Ok(());
            }
            self.child
                .kill()
                .await
                .map_err(|_| ProcessError::StopFailed(self.engine))?;
            self.child
                .wait()
                .await
                .map_err(|_| ProcessError::StopFailed(self.engine))?;
            Ok(())
        })
    }
}

impl ProcessLauncher for TokioProcessLauncher {
    type Child = TokioManagedChild;

    fn launch<'a>(
        &'a self,
        command: CommandSpec,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Child, ProcessError>> + Send + 'a>> {
        Box::pin(async move {
            let mut process = tokio::process::Command::new(&command.program);
            process
                .args(&command.args)
                .env_remove("MULTICORE_DAEMON_URL")
                .env_remove("MULTICORE_DAEMON_TOKEN")
                .env_remove("MULTICORE_DAEMON_READY_STDOUT")
                .env_remove("MULTICORE_DAEMON_ADDR")
                .env_remove("MULTICORE_DAEMON_DATA_DIR")
                .env_remove("MULTICORE_XRAY_BIN")
                .env_remove("MULTICORE_MIHOMO_BIN")
                .env_remove("MULTICORE_MIHOMO_CONTROLLER_ADDR")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            let mut child = process
                .spawn()
                .map_err(|_| ProcessError::LaunchFailed(command.engine))?;
            if let Some(stdout) = child.stdout.take() {
                spawn_log_reader(
                    self.logs.clone(),
                    command.engine,
                    DiagnosticStream::Stdout,
                    stdout,
                );
            }
            if let Some(stderr) = child.stderr.take() {
                spawn_log_reader(
                    self.logs.clone(),
                    command.engine,
                    DiagnosticStream::Stderr,
                    stderr,
                );
            }
            Ok(TokioManagedChild {
                engine: command.engine,
                child,
            })
        })
    }

    fn wait_until_ready<'a>(
        &'a self,
        command: &'a CommandSpec,
        child: &'a mut Self::Child,
    ) -> Pin<Box<dyn Future<Output = Result<(), ProcessError>> + Send + 'a>> {
        Box::pin(async move {
            let Some(runtime) = &self.runtime else {
                return if child.has_exited()? {
                    Err(ProcessError::ReadinessFailed(command.engine))
                } else {
                    Ok(())
                };
            };
            let deadline = Instant::now() + runtime.timeout;
            loop {
                if child.has_exited()? {
                    return Err(ProcessError::ReadinessFailed(command.engine));
                }
                if self.runtime_ready(command.engine).await {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(ProcessError::ReadinessFailed(command.engine));
                }
                sleep(Duration::from_millis(100)).await;
            }
        })
    }

    fn current_ready<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move { self.runtime_ready(engine).await })
    }

    fn diagnostic_logs(&self) -> Vec<CoreLogRecord> {
        self.logs.snapshot()
    }

    fn tun_check<'a>(&'a self) -> Pin<Box<dyn Future<Output = RuntimeCheckState> + Send + 'a>> {
        Box::pin(async move {
            let Some(runtime) = &self.runtime else {
                return RuntimeCheckState::Unsupported;
            };
            let Ok((_, device)) = mihomo_readiness_targets(&runtime.paths.mihomo_config) else {
                return RuntimeCheckState::Failed;
            };
            if tun_device_is_up(&device).await {
                RuntimeCheckState::Ready
            } else {
                RuntimeCheckState::Failed
            }
        })
    }
}

fn spawn_log_reader<R>(
    logs: Arc<CoreLogBuffer>,
    engine: Engine,
    stream: DiagnosticStream,
    reader: R,
) where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => logs.push(engine, stream, &line),
                Ok(None) => break,
                Err(_) => {
                    logs.push(
                        engine,
                        DiagnosticStream::Stderr,
                        "core log stream closed unexpectedly",
                    );
                    break;
                }
            }
        }
    });
}

fn xray_loopback_inbounds(path: &std::path::Path) -> Result<Vec<std::net::SocketAddr>, ()> {
    let bytes = std::fs::read(path).map_err(|_| ())?;
    let document: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| ())?;
    let inbounds = document
        .get("inbounds")
        .and_then(serde_json::Value::as_array)
        .ok_or(())?;
    let mut addresses = Vec::new();
    for inbound in inbounds {
        let Some(port) = inbound.get("port").and_then(serde_json::Value::as_u64) else {
            continue;
        };
        let Ok(port) = u16::try_from(port) else {
            continue;
        };
        let listen = inbound
            .get("listen")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("127.0.0.1");
        let Ok(ip) = listen.parse::<std::net::IpAddr>() else {
            continue;
        };
        if ip.is_loopback() {
            addresses.push(std::net::SocketAddr::new(ip, port));
        }
    }
    Ok(addresses)
}

fn mihomo_readiness_targets(path: &std::path::Path) -> Result<(std::net::SocketAddr, String), ()> {
    let bytes = std::fs::read(path).map_err(|_| ())?;
    let document: serde_yaml_ng::Value = serde_yaml_ng::from_slice(&bytes).map_err(|_| ())?;
    let mapping = document.as_mapping().ok_or(())?;
    let controller = mapping
        .get(serde_yaml_ng::Value::String(
            "external-controller".to_owned(),
        ))
        .and_then(serde_yaml_ng::Value::as_str)
        .ok_or(())?
        .parse::<std::net::SocketAddr>()
        .map_err(|_| ())?;
    if !controller.ip().is_loopback() {
        return Err(());
    }
    let device = mapping
        .get(serde_yaml_ng::Value::String("tun".to_owned()))
        .and_then(serde_yaml_ng::Value::as_mapping)
        .and_then(|tun| tun.get(serde_yaml_ng::Value::String("device".to_owned())))
        .and_then(serde_yaml_ng::Value::as_str)
        .filter(|device| !device.trim().is_empty())
        .ok_or(())?;
    Ok((controller, device.to_owned()))
}

#[cfg(windows)]
async fn tun_device_is_up(device: &str) -> bool {
    if device != "MultiCore" {
        return false;
    }
    tokio::process::Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$adapter = Get-NetAdapter -Name 'MultiCore' -ErrorAction SilentlyContinue; if ($null -ne $adapter -and $adapter.Status -eq 'Up') { exit 0 } else { exit 1 }",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success())
}

#[cfg(not(windows))]
async fn tun_device_is_up(_device: &str) -> bool {
    true
}

pub struct SidecarProcessController<L: ProcessLauncher = TokioProcessLauncher> {
    paths: RuntimePaths,
    launcher: L,
    children: Mutex<HashMap<Engine, L::Child>>,
}

impl SidecarProcessController<TokioProcessLauncher> {
    pub fn new(paths: RuntimePaths) -> Self {
        let launcher = TokioProcessLauncher::for_runtime(paths.clone(), Duration::from_secs(12));
        Self::with_launcher(paths, launcher)
    }

    pub fn new_with_logs(paths: RuntimePaths, logs: Arc<CoreLogBuffer>) -> Self {
        let launcher = TokioProcessLauncher::for_runtime_with_logs(
            paths.clone(),
            Duration::from_secs(12),
            logs,
        );
        Self::with_launcher(paths, launcher)
    }
}

impl<L: ProcessLauncher> SidecarProcessController<L> {
    pub fn with_launcher(paths: RuntimePaths, launcher: L) -> Self {
        Self {
            paths,
            launcher,
            children: Mutex::new(HashMap::new()),
        }
    }
}

impl<L: ProcessLauncher> ProcessController for SidecarProcessController<L> {
    type Error = ProcessError;

    fn start<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async move {
            let mut children = self.children.lock().await;
            if let Some(child) = children.get_mut(&engine) {
                if !child.has_exited()? {
                    return Ok(());
                }
                children.remove(&engine);
            }

            let command = self.paths.command(engine);
            let mut child = self.launcher.launch(command.clone()).await?;
            if child.has_exited()? {
                return Err(ProcessError::LaunchFailed(engine));
            }
            if let Err(error) = self.launcher.wait_until_ready(&command, &mut child).await {
                let _ = child.stop().await;
                return Err(error);
            }
            children.insert(engine, child);
            Ok(())
        })
    }

    fn stop<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async move {
            let mut children = self.children.lock().await;
            let Some(mut child) = children.remove(&engine) else {
                return Ok(());
            };
            if let Err(error) = child.stop().await {
                children.insert(engine, child);
                return Err(error);
            }
            Ok(())
        })
    }

    fn diagnostics<'a>(&'a self) -> Pin<Box<dyn Future<Output = ProcessDiagnostics> + Send + 'a>> {
        Box::pin(async move {
            let mut children = self.children.lock().await;
            let mut xray_running = false;
            let mut mihomo_running = false;
            if let Some(child) = children.get_mut(&Engine::Xray) {
                xray_running = child.has_exited().is_ok_and(|exited| !exited);
            }
            if let Some(child) = children.get_mut(&Engine::Mihomo) {
                mihomo_running = child.has_exited().is_ok_and(|exited| !exited);
            }
            let xray = if xray_running {
                if self.launcher.current_ready(Engine::Xray).await {
                    RuntimeCheckState::Ready
                } else {
                    RuntimeCheckState::Failed
                }
            } else {
                RuntimeCheckState::Stopped
            };
            let mihomo = if mihomo_running {
                if self.launcher.current_ready(Engine::Mihomo).await {
                    RuntimeCheckState::Ready
                } else {
                    RuntimeCheckState::Failed
                }
            } else {
                RuntimeCheckState::Stopped
            };
            let tun = if mihomo_running {
                self.launcher.tun_check().await
            } else {
                RuntimeCheckState::Stopped
            };
            ProcessDiagnostics {
                xray,
                mihomo,
                tun,
                logs: self.launcher.diagnostic_logs(),
            }
        })
    }
}
