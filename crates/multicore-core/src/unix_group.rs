use std::{
    ffi::OsString,
    future::Future,
    io,
    os::unix::process::CommandExt,
    pin::Pin,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

#[cfg(target_os = "linux")]
use rustix::io::Errno;
use rustix::process::{
    Pid, Signal, WaitId, WaitIdOptions, getpid, getppid, kill_process_group, waitid,
};
#[cfg(target_os = "linux")]
use rustix::process::{geteuid, set_parent_process_death_signal};
#[cfg(target_os = "linux")]
use rustix::thread::{
    CapabilitySet, CapabilitySets, capabilities, clear_ambient_capability_set,
    remove_capability_from_bounding_set, set_capabilities,
};

/// macOS has no Linux capabilities. The set only keeps the launcher API the same; cores start
/// with whatever the host process has, and nothing is dropped.
#[cfg(not(target_os = "linux"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapabilitySet(u64);

#[cfg(not(target_os = "linux"))]
impl CapabilitySet {
    pub const NET_BIND_SERVICE: Self = Self(1 << 10);
    pub const NET_ADMIN: Self = Self(1 << 12);
    pub const NET_RAW: Self = Self(1 << 13);
    pub const SYS_PTRACE: Self = Self(1 << 19);

    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

use crate::{
    CommandSpec, CoreLogBuffer, DiagnosticStream, Engine, ManagedChild, ProcessError,
    ProcessLauncher, RuntimeCheckState, RuntimePaths, TokioProcessLauncher,
    sidecar::{spawn_blocking_log_reader, wait_for_runtime_readiness},
};

const TERMINATION_GRACE: Duration = Duration::from_secs(2);
const EXIT_POLL: Duration = Duration::from_millis(20);
const ALLOWED_ENVIRONMENT: &[&str] = &[
    "PATH",
    "LANG",
    "LC_ALL",
    "TZ",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

/// TUN, routes and nftables (`NET_ADMIN`), raw sockets (`NET_RAW`) and DNS on port 53
/// (`NET_BIND_SERVICE`).
const NETWORK_CAPABILITIES: CapabilitySet = CapabilitySet::NET_ADMIN
    .union(CapabilitySet::NET_BIND_SERVICE)
    .union(CapabilitySet::NET_RAW);

/// The capabilities each core keeps when a launcher restricts them. Everything else is removed
/// from the bounding, permitted, effective and inheritable sets, and ambient is cleared.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CoreCapabilities {
    pub xray: CapabilitySet,
    pub mihomo: CapabilitySet,
}

impl CoreCapabilities {
    /// The always-ready service. Its own bounding set also holds `CAP_DAC_READ_SEARCH` (staged
    /// generations) and `CAP_SETPCAP` (this drop), which no core gets. Xray only needs the
    /// network capabilities. Mihomo also keeps `CAP_SYS_PTRACE`: `PROCESS-NAME`/`PROCESS-PATH`
    /// rules map a connection to its process through other users' `/proc/<pid>/fd`, which
    /// needs ptrace read access.
    pub const SERVICE: Self = Self {
        xray: NETWORK_CAPABILITIES,
        mihomo: NETWORK_CAPABILITIES.union(CapabilitySet::SYS_PTRACE),
    };

    fn for_engine(self, engine: Engine) -> CapabilitySet {
        match engine {
            Engine::Xray => self.xray,
            Engine::Mihomo => self.mihomo,
        }
    }
}
/// Capability numbers are below 64; numbers the kernel doesn't know fail with `EINVAL`.
#[cfg(target_os = "linux")]
const CAPABILITY_NUMBERS: u32 = 64;

type SpawnReply = tokio::sync::oneshot::Sender<io::Result<Child>>;
type SpawnRequest = (CommandSpec, Option<CapabilitySet>, SpawnReply);

#[derive(Clone)]
pub struct UnixGroupLauncher {
    spawner: Arc<Mutex<mpsc::Sender<SpawnRequest>>>,
    logs: Arc<CoreLogBuffer>,
    restrict_capabilities: Option<CoreCapabilities>,
    readiness: Option<TokioProcessLauncher>,
    readiness_timeout: Duration,
}

impl UnixGroupLauncher {
    pub fn new(logs: Arc<CoreLogBuffer>) -> io::Result<Self> {
        let (sender, requests) = mpsc::channel::<SpawnRequest>();
        std::thread::Builder::new()
            .name("multicore-core-spawner".into())
            .spawn(move || {
                for (command, keep, reply) in requests {
                    let _ = reply.send(spawn_core(&command, keep));
                }
            })?;
        Ok(Self {
            spawner: Arc::new(Mutex::new(sender)),
            logs,
            restrict_capabilities: None,
            readiness: None,
            readiness_timeout: crate::CORE_READINESS_TIMEOUT,
        })
    }

    pub fn for_runtime(
        paths: RuntimePaths,
        logs: Arc<CoreLogBuffer>,
        timeout: Duration,
    ) -> io::Result<Self> {
        let mut launcher = Self::new(logs.clone())?;
        launcher.readiness = Some(TokioProcessLauncher::for_runtime_with_logs(
            paths, timeout, logs,
        ));
        launcher.readiness_timeout = timeout;
        Ok(launcher)
    }

    /// Cores started by this launcher keep only their entry of `keep` (bounding, permitted,
    /// effective, inheritable; ambient cleared). Used by the always-ready service, whose own
    /// bounding set holds capabilities that must not reach Mihomo or Xray.
    #[must_use]
    pub fn with_restricted_capabilities(mut self, keep: CoreCapabilities) -> Self {
        self.restrict_capabilities = Some(keep);
        self
    }

    async fn launch_core(
        &self,
        command: CommandSpec,
    ) -> Result<UnixGroupManagedChild, ProcessError> {
        let engine = command.engine;
        let keep = self
            .restrict_capabilities
            .map(|keep| keep.for_engine(engine));
        let (reply, response) = tokio::sync::oneshot::channel();
        self.spawner
            .lock()
            .map_err(|_| ProcessError::LaunchFailed(engine))?
            .send((command, keep, reply))
            .map_err(|_| ProcessError::LaunchFailed(engine))?;
        let mut child = response
            .await
            .map_err(|_| ProcessError::LaunchFailed(engine))?
            .map_err(|_| ProcessError::LaunchFailed(engine))?;
        let group = Pid::from_child(&child);
        if let Some(stdout) = child.stdout.take() {
            spawn_blocking_log_reader(self.logs.clone(), engine, DiagnosticStream::Stdout, stdout);
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_blocking_log_reader(self.logs.clone(), engine, DiagnosticStream::Stderr, stderr);
        }
        Ok(UnixGroupManagedChild {
            engine,
            child,
            group,
            reaped: false,
        })
    }
}

fn sanitized_environment(
    variables: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    variables
        .into_iter()
        .filter(|(key, _)| {
            key.to_str()
                .is_some_and(|key| ALLOWED_ENVIRONMENT.contains(&key))
        })
        .collect()
}

/// Runs in the forked child before `exec`: only raw syscalls, no allocation.
///
/// Root keeps its bounding set across `exec` (permitted = bounding for uid 0), so the bounding
/// set is what must shrink; that needs `CAP_SETPCAP`, which is dropped last. An unprivileged
/// process (no permitted capabilities, not root) cannot shrink it and has nothing to lose, so
/// `EPERM` is accepted only there.
#[cfg(target_os = "linux")]
fn restrict_core_capabilities(keep: CapabilitySet) -> io::Result<()> {
    let current = capabilities(None)?;
    let privileged = !current.permitted.is_empty() || geteuid().is_root();
    let drop_from_bounding_set = |capability: CapabilitySet| -> io::Result<()> {
        match remove_capability_from_bounding_set(capability) {
            Ok(()) | Err(Errno::INVAL) => Ok(()),
            Err(Errno::PERM) if !privileged => Ok(()),
            Err(error) => Err(error.into()),
        }
    };
    for number in 0..CAPABILITY_NUMBERS {
        let capability = CapabilitySet::from_bits_retain(1 << number);
        if !keep.contains(capability) && capability != CapabilitySet::SETPCAP {
            drop_from_bounding_set(capability)?;
        }
    }
    if !keep.contains(CapabilitySet::SETPCAP) {
        drop_from_bounding_set(CapabilitySet::SETPCAP)?;
    }
    match clear_ambient_capability_set() {
        Ok(()) | Err(Errno::INVAL) => {}
        Err(error) => return Err(error.into()),
    }
    set_capabilities(
        None,
        CapabilitySets {
            effective: current.effective & keep,
            permitted: current.permitted & keep,
            inheritable: current.inheritable & keep,
        },
    )?;
    Ok(())
}

fn spawn_core(command: &CommandSpec, keep: Option<CapabilitySet>) -> io::Result<Child> {
    let parent = getpid();
    let mut process = Command::new(&command.program);
    process
        .args(&command.args)
        .env_clear()
        .envs(sanitized_environment(std::env::vars_os()))
        .envs(command.env.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    unsafe {
        process.pre_exec(move || {
            // macOS has no parent-death signal: there the host's `Drop` kills the group.
            #[cfg(target_os = "linux")]
            set_parent_process_death_signal(Some(Signal::KILL))?;
            if getppid() != Some(parent) {
                return Err(io::Error::other("core host exited before core start"));
            }
            #[cfg(target_os = "linux")]
            if let Some(keep) = keep {
                restrict_core_capabilities(keep)?;
            }
            #[cfg(not(target_os = "linux"))]
            let _ = keep;
            Ok(())
        });
    }
    process.spawn()
}

pub struct UnixGroupManagedChild {
    engine: Engine,
    child: Child,
    group: Pid,
    reaped: bool,
}

impl UnixGroupManagedChild {
    fn exited_without_reaping(&self) -> Result<bool, ProcessError> {
        if self.reaped {
            return Ok(true);
        }
        waitid(
            WaitId::Pid(self.group),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        )
        .map(|status| status.is_some())
        .map_err(|_| ProcessError::InspectFailed(self.engine))
    }

    fn kill_group_and_reap(&mut self) -> Result<(), ProcessError> {
        if self.reaped {
            return Ok(());
        }
        let _ = kill_process_group(self.group, Signal::KILL);
        self.child
            .wait()
            .map_err(|_| ProcessError::StopFailed(self.engine))?;
        self.reaped = true;
        Ok(())
    }
}

impl ManagedChild for UnixGroupManagedChild {
    fn has_exited(&mut self) -> Result<bool, ProcessError> {
        self.exited_without_reaping()
    }

    fn pid(&self) -> Option<u32> {
        Some(self.child.id())
    }

    fn stop<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), ProcessError>> + Send + 'a>> {
        Box::pin(async move {
            if self.reaped {
                return Ok(());
            }
            let _ = kill_process_group(self.group, Signal::TERM);
            let deadline = tokio::time::Instant::now() + TERMINATION_GRACE;
            while !self.exited_without_reaping()? && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(EXIT_POLL).await;
            }
            self.kill_group_and_reap()
        })
    }
}

impl Drop for UnixGroupManagedChild {
    fn drop(&mut self) {
        let _ = self.kill_group_and_reap();
    }
}

impl ProcessLauncher for UnixGroupLauncher {
    type Child = UnixGroupManagedChild;

    fn launch<'a>(
        &'a self,
        command: CommandSpec,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Child, ProcessError>> + Send + 'a>> {
        Box::pin(self.launch_core(command))
    }

    fn diagnostic_logs(&self) -> Vec<crate::CoreLogRecord> {
        self.logs.snapshot()
    }

    fn wait_until_ready<'a>(
        &'a self,
        command: &'a CommandSpec,
        child: &'a mut Self::Child,
    ) -> Pin<Box<dyn Future<Output = Result<(), ProcessError>> + Send + 'a>> {
        Box::pin(wait_for_runtime_readiness(
            self.readiness.as_ref(),
            self.readiness_timeout,
            command,
            child,
        ))
    }

    fn current_ready<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            match &self.readiness {
                Some(readiness) => readiness.current_ready(engine).await,
                None => true,
            }
        })
    }

    fn tun_check<'a>(&'a self) -> Pin<Box<dyn Future<Output = RuntimeCheckState> + Send + 'a>> {
        Box::pin(async move {
            match &self.readiness {
                Some(readiness) => readiness.tun_check().await,
                None => RuntimeCheckState::Unsupported,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        path::{Path, PathBuf},
        time::Instant,
    };

    fn shell(script: &str, extra: &Path) -> CommandSpec {
        CommandSpec {
            engine: Engine::Xray,
            program: PathBuf::from("/bin/sh"),
            args: vec![
                OsString::from("-c"),
                OsString::from(script),
                OsString::from("sh"),
                extra.as_os_str().to_owned(),
            ],
            env: Vec::new(),
        }
    }

    fn process_is_gone(pid: i32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => true,
            Ok(stat) => stat
                .rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
        }
    }

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

    fn read_pid(path: &Path) -> i32 {
        assert!(eventually(Duration::from_secs(5), || {
            std::fs::read_to_string(path).is_ok_and(|text| text.ends_with('\n'))
        }));
        std::fs::read_to_string(path)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    #[tokio::test]
    async fn stop_terminates_the_core_and_its_background_children() {
        let temp = tempfile::tempdir().unwrap();
        let pid_file = temp.path().join("grandchild");
        let launcher = UnixGroupLauncher::new(Arc::new(CoreLogBuffer::default())).unwrap();
        let mut child = launcher
            .launch(shell(
                "sleep 60 & echo \"$!\" > \"$1\"; exec sleep 60",
                &pid_file,
            ))
            .await
            .unwrap();
        let grandchild = read_pid(&pid_file);
        let core = child.child.id() as i32;
        assert!(!child.has_exited().unwrap());

        child.stop().await.unwrap();
        assert!(child.has_exited().unwrap());
        assert!(process_is_gone(core));
        assert!(eventually(Duration::from_secs(3), || process_is_gone(
            grandchild
        )));
    }

    #[tokio::test]
    async fn stop_kills_a_core_that_ignores_sigterm_after_the_grace_period() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("trapped");
        let launcher = UnixGroupLauncher::new(Arc::new(CoreLogBuffer::default())).unwrap();
        let mut child = launcher
            .launch(shell(
                "trap '' TERM; echo \"$$\" > \"$1\"; while :; do sleep 1; done",
                &marker,
            ))
            .await
            .unwrap();
        let core = child.child.id() as i32;
        assert_eq!(read_pid(&marker), core);
        let started = Instant::now();
        child.stop().await.unwrap();
        assert!(started.elapsed() >= TERMINATION_GRACE);
        assert!(started.elapsed() < TERMINATION_GRACE + Duration::from_secs(3));
        assert!(process_is_gone(core));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn dropping_the_launcher_kills_cores_through_the_parent_death_signal() {
        let temp = tempfile::tempdir().unwrap();
        let launcher = UnixGroupLauncher::new(Arc::new(CoreLogBuffer::default())).unwrap();
        let child = launcher
            .launch(shell("exec sleep 60", temp.path()))
            .await
            .unwrap();
        let core = child.child.id() as i32;
        drop(launcher);
        assert!(eventually(Duration::from_secs(3), || process_is_gone(core)));
        drop(child);
    }

    #[tokio::test]
    async fn dropping_a_child_kills_and_reaps_its_group() {
        let temp = tempfile::tempdir().unwrap();
        let pid_file = temp.path().join("grandchild");
        let launcher = UnixGroupLauncher::new(Arc::new(CoreLogBuffer::default())).unwrap();
        let child = launcher
            .launch(shell(
                "sleep 60 & echo \"$!\" > \"$1\"; exec sleep 60",
                &pid_file,
            ))
            .await
            .unwrap();
        let grandchild = read_pid(&pid_file);
        let core = child.child.id() as i32;
        drop(child);
        assert!(!Path::new(&format!("/proc/{core}")).exists());
        assert!(eventually(Duration::from_secs(3), || process_is_gone(
            grandchild
        )));
    }

    #[tokio::test]
    async fn core_output_is_captured_in_the_bounded_log_buffer() {
        let temp = tempfile::tempdir().unwrap();
        let logs = Arc::new(CoreLogBuffer::default());
        let launcher = UnixGroupLauncher::new(logs.clone()).unwrap();
        let mut child = launcher
            .launch(shell("echo started; echo failed >&2", temp.path()))
            .await
            .unwrap();
        assert!(eventually(Duration::from_secs(5), || logs.snapshot().len() == 2));
        let records = logs.snapshot();
        assert!(records.iter().any(|record| {
            record.stream == DiagnosticStream::Stdout && record.message == "started"
        }));
        assert!(records.iter().any(|record| {
            record.stream == DiagnosticStream::Stderr && record.message == "failed"
        }));
        child.stop().await.unwrap();
    }

    #[test]
    fn environment_keeps_only_the_allowlist() {
        let kept = sanitized_environment([
            (OsString::from("PATH"), OsString::from("/usr/bin")),
            (OsString::from("LANG"), OsString::from("C.UTF-8")),
            (
                OsString::from("MULTICORE_DAEMON_TOKEN"),
                OsString::from("secret"),
            ),
            (OsString::from("HOME"), OsString::from("/root")),
            (OsString::from("path"), OsString::from("/tmp")),
        ]);
        assert_eq!(
            kept,
            vec![
                (OsString::from("PATH"), OsString::from("/usr/bin")),
                (OsString::from("LANG"), OsString::from("C.UTF-8")),
            ]
        );
    }

    #[cfg(target_os = "linux")]
    const PROBE_ENV: &str = "MULTICORE_CAPABILITY_PROBE";

    /// `Cap*:` lines of `/proc/self/status` as seen by a core started through `launcher`.
    #[cfg(target_os = "linux")]
    async fn core_capabilities(launcher: &UnixGroupLauncher, engine: Engine) -> Vec<(String, u64)> {
        let temp = tempfile::tempdir().unwrap();
        let report = temp.path().join("caps");
        let mut command = shell(
            "grep '^Cap' /proc/self/status > \"$1.tmp\"; mv \"$1.tmp\" \"$1\"",
            &report,
        );
        command.engine = engine;
        let mut child = launcher.launch(command).await.unwrap();
        assert!(eventually(Duration::from_secs(5), || report.exists()));
        child.stop().await.unwrap();
        std::fs::read_to_string(&report)
            .unwrap()
            .lines()
            .map(|line| {
                let (name, value) = line.split_once(':').unwrap();
                (
                    name.to_owned(),
                    u64::from_str_radix(value.trim(), 16).unwrap(),
                )
            })
            .collect()
    }

    #[cfg(target_os = "linux")]
    fn set(capabilities: &[(String, u64)], name: &str) -> u64 {
        capabilities
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| *value)
            .unwrap_or_else(|| panic!("{name} missing"))
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn restricted_cores_hold_no_capability_beyond_their_own_set() {
        let launcher = UnixGroupLauncher::new(Arc::new(CoreLogBuffer::default()))
            .unwrap()
            .with_restricted_capabilities(CoreCapabilities::SERVICE);
        for (engine, keep) in [
            (Engine::Xray, CoreCapabilities::SERVICE.xray),
            (Engine::Mihomo, CoreCapabilities::SERVICE.mihomo),
        ] {
            let capabilities = core_capabilities(&launcher, engine).await;
            for name in ["CapInh", "CapPrm", "CapEff", "CapAmb"] {
                assert_eq!(
                    set(&capabilities, name) & !keep.bits(),
                    0,
                    "{engine:?} {name}: {capabilities:?}"
                );
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_service_gives_xray_the_network_capabilities_and_mihomo_also_ptrace() {
        let network =
            CapabilitySet::NET_ADMIN | CapabilitySet::NET_BIND_SERVICE | CapabilitySet::NET_RAW;
        assert_eq!(CoreCapabilities::SERVICE.xray, network);
        assert_eq!(
            CoreCapabilities::SERVICE.mihomo,
            network | CapabilitySet::SYS_PTRACE
        );
    }

    /// Re-runs `restricted_core_capabilities_as_namespace_root` as root of a new user
    /// namespace, where the launcher holds `CAP_SETPCAP` like the systemd service does.
    #[cfg(target_os = "linux")]
    #[test]
    fn restricted_cores_keep_only_their_own_capabilities_as_root() {
        let namespaces = std::process::Command::new("unshare")
            .args(["--user", "--map-root-user", "true"])
            .status();
        if !namespaces.is_ok_and(|status| status.success()) {
            eprintln!("skipped: unprivileged user namespaces are unavailable");
            return;
        }
        let output = std::process::Command::new("unshare")
            .args(["--user", "--map-root-user"])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "unix_group::tests::restricted_core_capabilities_as_namespace_root",
                "--ignored",
                "--test-threads=1",
            ])
            .env(PROBE_ENV, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "run by restricted_cores_keep_only_their_own_capabilities_as_root"]
    async fn restricted_core_capabilities_as_namespace_root() {
        if std::env::var_os(PROBE_ENV).is_none() {
            return;
        }
        assert!(geteuid().is_root());
        let unrestricted = core_capabilities(
            &UnixGroupLauncher::new(Arc::new(CoreLogBuffer::default())).unwrap(),
            Engine::Mihomo,
        )
        .await;
        let held = CapabilitySet::SYS_PTRACE.bits()
            | CapabilitySet::DAC_READ_SEARCH.bits()
            | CapabilitySet::SETPCAP.bits();
        assert_eq!(set(&unrestricted, "CapEff") & held, held, "control run");

        let launcher = UnixGroupLauncher::new(Arc::new(CoreLogBuffer::default()))
            .unwrap()
            .with_restricted_capabilities(CoreCapabilities::SERVICE);
        for (engine, keep) in [
            (Engine::Xray, CoreCapabilities::SERVICE.xray),
            (Engine::Mihomo, CoreCapabilities::SERVICE.mihomo),
        ] {
            let restricted = core_capabilities(&launcher, engine).await;
            let keep = keep.bits();
            for name in ["CapBnd", "CapPrm", "CapEff"] {
                assert_eq!(
                    set(&restricted, name),
                    keep,
                    "{engine:?} {name}: {restricted:?}"
                );
            }
            assert_eq!(set(&restricted, "CapInh"), 0, "{engine:?}: {restricted:?}");
            assert_eq!(set(&restricted, "CapAmb"), 0, "{engine:?}: {restricted:?}");
        }
        let mihomo = core_capabilities(&launcher, Engine::Mihomo).await;
        let effective = set(&mihomo, "CapEff");
        assert_ne!(effective & CapabilitySet::SYS_PTRACE.bits(), 0);
        assert_eq!(effective & CapabilitySet::DAC_READ_SEARCH.bits(), 0);
        assert_eq!(effective & CapabilitySet::SETPCAP.bits(), 0);
    }
}
