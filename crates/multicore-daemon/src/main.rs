use std::{
    env, io,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use multicore_core::local_proxy_port::LocalProxyPort;
use multicore_core::{
    DeviceIdentity, MihomoRuntimeControl, ProfileStore, ReqwestHttpClient, Snapshot,
    SubscriptionFetcher, TunnelProxy, default_ipv4_interface, stage_runtime_with_mihomo_control,
};
use multicore_daemon::connection_mode::{Backed, ModeController};
use multicore_daemon::elevated_controller::{production_factory, runtime_generation_from_path};
use multicore_daemon::service_client::{
    ControllerSlot, ProductionServiceConnector, ServiceMode, ServiceSession, ServiceTunController,
    SlotSelector, WarmInputs, WarmMiss, WarmStage, WarmStages,
};
use multicore_daemon::{
    Backend, BackendError, ConnectOptions, CoreBackend, PreparedController, bind_loopback,
    options_controller_factory, publish_readiness_if_enabled, try_router_with_service_log,
};
#[cfg(target_os = "linux")]
use multicore_daemon::{
    connection_mode::NoProxyController, elevated_controller::ProductionElevatedController,
};

/// The daemon's connection controller: TUN through the service or the elevated host, or
/// (Windows, macOS) the system-proxy mode.
#[cfg(any(windows, target_os = "macos"))]
type Controller = proxy_mode::Controller;
#[cfg(target_os = "linux")]
type Controller = ModeController<
    Backed<ServiceTunController<ProductionServiceConnector>, ProductionElevatedController>,
    NoProxyController,
>;
type Session = ServiceSession<ProductionServiceConnector>;

/// `multicore_i18n::daemon_messages` code of the notice.
const SERVICE_UNAVAILABLE_NOTICE: &str = "service.unavailable_notice";
/// How often the watcher looks at the service: also the upper bound for noticing that a
/// connection carried by the service is gone.
const WATCH_INTERVAL: Duration = Duration::from_secs(3);
const WATCH_BACKOFF_MAX: Duration = Duration::from_secs(60);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    multicore_core::ensure_crypto_provider_installed();
    let token = required_env("MULTICORE_DAEMON_TOKEN")?;
    let address: SocketAddr = env::var("MULTICORE_DAEMON_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8787".to_owned())
        .parse()?;
    let data_directory = PathBuf::from(required_env("MULTICORE_DAEMON_DATA_DIR")?);
    let device_identity = DeviceIdentity::load_or_create(&data_directory)?;
    let mihomo_controller_address: SocketAddr = env::var("MULTICORE_MIHOMO_CONTROLLER_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:19090".to_owned())
        .parse()?;
    if !mihomo_controller_address.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "MULTICORE_MIHOMO_CONTROLLER_ADDR must be an IP loopback SocketAddr",
        )
        .into());
    }
    let mihomo_controller_secret = random_hex(32)?;
    let runtime_control =
        MihomoRuntimeControl::new(mihomo_controller_address, mihomo_controller_secret.clone())?;

    // The selector and the dashboard proxy follow this slot: the service's filtered
    // controller proxy after a `Prepare`, or the daemon's own on-demand Mihomo while a
    // connection runs without the service.
    let controller_slot = Arc::new(ControllerSlot::default());
    let service: Arc<Session> = Arc::new(ServiceSession::new(
        ProductionServiceConnector::default(),
        controller_slot.clone(),
    ));
    let on_demand_slot = {
        let slot = controller_slot.clone();
        move || slot.set(mihomo_controller_address, &mihomo_controller_secret, None)
    };
    let selector = Arc::new(SlotSelector::new(controller_slot.clone()));

    // The vendored zashboard build ships as a `dashboard/` sibling of `cores/` and `runtime/`
    // at the package root (see packaging/*/versions.json). Mihomo never serves it (no
    // `external-ui`): the daemon's read-only dashboard proxy does, see `dashboard_proxy`.
    // MultiCore.app keeps it in `Contents/Resources`: a signed bundle allows only code in
    // `Contents/MacOS`, the package root.
    let dashboard_directory = env::current_exe().ok().and_then(|executable| {
        dashboard_directory_for(&executable).map(|package_root| {
            #[cfg(target_os = "macos")]
            let package_root = package_root
                .parent()
                .map_or(package_root.clone(), |contents| contents.join("Resources"));
            package_root.join("dashboard")
        })
    });

    let store = ProfileStore::open(
        data_directory.join("profiles"),
        Some(data_directory.join("snapshots")),
    )?;
    // Named by the system proxy while connected in proxy mode: downloads go through it first.
    let tunnel_proxy = TunnelProxy::default();
    let fetcher = SubscriptionFetcher::new(
        ReqwestHttpClient::new(device_identity)?.with_tunnel_proxy(tunnel_proxy.clone()),
    );
    let runtime_root = data_directory.join("runtime");
    // Mihomo's loopback mixed port (system proxy, and the local proxy other apps may use):
    // one saved port for every connection and restart. Chosen now so Settings can show it.
    let local_proxy_port = LocalProxyPort::open(&data_directory);
    let initial_local_proxy_port = local_proxy_port
        .current()
        .map_err(|error| eprintln!("local proxy port unavailable: {error}"))
        .ok();
    #[cfg(not(windows))]
    let _ = initial_local_proxy_port;
    // Per-connection core logs and their index (see `multicore_core::core_sessions`).
    let core_sessions_directory = data_directory
        .join("logs")
        .join(multicore_core::core_sessions::CORE_SESSIONS_DIRECTORY);
    #[cfg(any(windows, target_os = "macos"))]
    let system_proxy_marker = data_directory.join("system-proxy.json");
    #[cfg(any(windows, target_os = "macos"))]
    if let Err(error) =
        multicore_daemon::system_proxy::recover(&proxy_mode::Settings, &system_proxy_marker)
    {
        eprintln!("system proxy recovery failed: {error}");
    }
    #[cfg(any(windows, target_os = "macos"))]
    let proxy_mode = proxy_mode::ProxyMode {
        package_root: env::current_exe()
            .ok()
            .and_then(|executable| dashboard_directory_for(&executable)),
        dashboard_directory: None,
        log_path: data_directory.join("logs").join("latest-core.log"),
        journal_directory: core_sessions_directory.clone(),
        marker_path: system_proxy_marker.clone(),
        tunnel_proxy: tunnel_proxy.clone(),
    };
    let elevated_factory = Mutex::new(None);
    let warm_stages = WarmStages::default();
    // Serialises staging, so a Connect and a warm-up never stage the same generation twice.
    let staging = Mutex::new(());
    let factory_service = service.clone();
    let controller_factory =
        options_controller_factory(move |snapshot: &Snapshot, options: ConnectOptions| {
            let interface = default_ipv4_interface().map_err(|_| BackendError::new())?;
            // Re-checked on every Connect and warm-up: a service that started after the app
            // is used at once; on-demand elevation only when it is absent or unreachable now.
            let use_service = factory_service.probe_mode() == ServiceMode::Service;
            if (options.warm_up || options.service_only) && !use_service {
                // Warming and the automatic reconnect after a service loss never fall back to
                // an elevation prompt (or to per-user cores).
                return Err(BackendError::new());
            }
            // TUN off without the service: cores run as this user (Windows). macOS has no
            // service or TUN yet, so it always connects this way.
            #[cfg(windows)]
            let user_proxy_mode = !options.tun && !use_service;
            #[cfg(target_os = "macos")]
            let user_proxy_mode = !use_service;
            #[cfg(any(windows, target_os = "macos"))]
            if user_proxy_mode {
                let control = runtime_control
                    .clone()
                    .with_xray_outbound_interface(interface)
                    .map_err(|_| BackendError::new())?;
                on_demand_slot();
                return proxy_mode.prepare(
                    snapshot,
                    &runtime_root,
                    control,
                    local_proxy_port
                        .verified()
                        .map_err(|_| BackendError::new())?,
                );
            }
            if use_service {
                #[cfg(windows)]
                let controller = |generation, mixed_port| {
                    service_controller(
                        &proxy_mode,
                        &factory_service,
                        generation,
                        mixed_port,
                        options,
                    )
                };
                #[cfg(not(windows))]
                let controller = |generation, mixed_port| {
                    service_controller(&factory_service, generation, mixed_port, options)
                };
                let inputs = WarmInputs::new(snapshot, &interface);
                let key = inputs.key();
                let _staging = staging.lock().map_err(|_| BackendError::new())?;
                // A generation the service rejected (for example its mixed port was taken)
                // is dropped here and staged again, its port checked first.
                let miss = match warm_stages.reusable(&inputs, &factory_service) {
                    Ok(stage) => {
                        return Ok(PreparedController::reuse(controller(
                            stage.generation,
                            stage.mixed_port,
                        )?));
                    }
                    Err(miss) => miss,
                };
                let mixed_port = match miss {
                    WarmMiss::PrepareRejected => local_proxy_port.verified(),
                    _ => local_proxy_port.current(),
                }
                .map_err(|_| BackendError::new())?;
                let control = runtime_control
                    .clone()
                    .with_xray_outbound_interface(interface)
                    .and_then(|control| control.with_proxy_mode(mixed_port))
                    .map_err(|_| BackendError::new())?;
                let staged = stage_runtime_with_mihomo_control(snapshot, &runtime_root, &control)
                    .map_err(|_| BackendError::new())?;
                let generation = runtime_generation_from_path(&staged.paths().directory)
                    .map_err(|_| BackendError::new())?;
                warm_stages.remember(
                    WarmStage {
                        key,
                        generation,
                        mixed_port,
                    },
                    inputs,
                );
                return Ok(PreparedController::with_runtime(
                    controller(generation, mixed_port)?,
                    staged,
                )
                .restaged_because(miss.to_string()));
            }
            let control = runtime_control
                .clone()
                .with_xray_outbound_interface(interface)
                .map_err(|_| BackendError::new())?;
            let staged = stage_runtime_with_mihomo_control(snapshot, &runtime_root, &control)
                .map_err(|_| BackendError::new())?;
            let generation = runtime_generation_from_path(&staged.paths().directory)
                .map_err(|_| BackendError::new())?;
            let mut elevated_factory = elevated_factory.lock().map_err(|_| BackendError::new())?;
            if elevated_factory.is_none() {
                *elevated_factory = Some(production_factory().map_err(|_| BackendError::new())?);
            }
            let elevated_factory = elevated_factory.as_ref().ok_or_else(BackendError::new)?;
            let controller: Arc<Controller> = Arc::new(ModeController::Tun(Backed::OnDemand(
                elevated_factory
                    .controller(generation)
                    .map_err(|_| BackendError::new())?,
            )));
            on_demand_slot();
            Ok(PreparedController::with_runtime(controller, staged))
        });
    let events_log_path = data_directory.join("logs").join("events.log");
    let backend = Arc::new(CoreBackend::new_with_connect_options(
        store,
        fetcher,
        controller_factory,
        selector,
        Some(events_log_path),
    )?);
    // Always-ready mode is armed from the start; it only acts while the watcher finds the
    // service available.
    backend.enable_always_ready(service.clone()).await;
    backend.enable_session_log(core_sessions_directory).await;
    spawn_service_watch(backend.clone(), service.clone());
    spawn_group_icon_fetch(backend.clone()).await;

    // Read-only dashboard on its own loopback port with its own per-session token. The URL
    // embeds that token and must never be logged; it only leaves this process through the
    // authenticated `/v1/dashboard-url` route.
    let dashboard_url: Option<Arc<str>> = match dashboard_directory
        .as_ref()
        .filter(|directory| directory.join("index.html").is_file())
    {
        Some(directory) => {
            let token = random_hex(32)?;
            let listener = bind_loopback("127.0.0.1:0".parse()?).await?;
            let port = listener.local_addr()?.port();
            let router = multicore_daemon::dashboard_proxy::dashboard_router(
                directory.clone(),
                Arc::from(token.as_str()),
                controller_slot.clone(),
            );
            tokio::spawn(async move {
                let _ = axum::serve(listener, router).await;
            });
            Some(Arc::from(multicore_daemon::dashboard_proxy::dashboard_url(
                port, &token,
            )))
        }
        None => None,
    };

    let shared: Arc<dyn Backend> = backend.clone();
    // Service updates are Windows only (Linux packages are updated by the package manager).
    // The route asks the service on every call, so a service that started after the daemon
    // is used too; without one it answers `service_unavailable`.
    let service_updates =
        cfg!(windows).then(|| service.clone() as Arc<dyn multicore_daemon::ServiceUpdates>);
    // «Устранить неполадки»: detection and user-level fixes here, privileged fixes through
    // the service or the UAC-elevated core host.
    #[cfg(windows)]
    let troubleshooting: Arc<dyn multicore_daemon::troubleshoot::Troubleshooting> = Arc::new(
        multicore_daemon::troubleshoot::windows::SystemTroubleshooting {
            service: Some(service.clone() as Arc<dyn multicore_daemon::troubleshoot::ServiceFixes>),
            proxy_marker: system_proxy_marker,
            package_root: env::current_exe()
                .ok()
                .and_then(|executable| dashboard_directory_for(&executable)),
            // The on-demand Mihomo's controller (per-user TUN and proxy mode) and the saved
            // local proxy port every generation's mixed listener uses.
            extra_ports: std::iter::once(mihomo_controller_address.port())
                .chain(initial_local_proxy_port)
                .map(|port| multicore_core::troubleshoot::ListenPort {
                    port,
                    udp: false,
                    mihomo: true,
                })
                .collect(),
        },
    );
    #[cfg(not(windows))]
    let troubleshooting: Arc<dyn multicore_daemon::troubleshoot::Troubleshooting> =
        Arc::new(multicore_daemon::troubleshoot::SystemTroubleshooting);
    // «Сохранить лог…»: the service's own log on every platform; without a service the
    // route says so.
    let service_log = service.clone() as Arc<dyn multicore_daemon::ServiceLogSource>;
    let app = try_router_with_service_log(
        shared,
        token,
        dashboard_url,
        service_updates,
        Some(troubleshooting),
        Some(service_log),
    )?;
    let listener = bind_loopback(address).await?;
    let resolved_address = listener.local_addr()?;
    {
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        publish_readiness_if_enabled(
            &mut stdout,
            resolved_address,
            env::var_os("MULTICORE_DAEMON_READY_STDOUT").as_deref(),
        )?;
    }
    axum::serve(listener, app).await?;
    Ok(())
}

/// The always-ready watcher. Every [`WATCH_INTERVAL`] (or at once when the backend asks for a
/// warm-up) it refreshes what it knows about the service — `Status` over the channel while
/// the service is in use, a fresh probe otherwise, so a service that starts later is picked
/// up — and lets the backend act on it: warm the cores while Ready, tear a connection down
/// when the service lost it. Backs off while the service stays unusable.
fn spawn_service_watch<B>(backend: Arc<B>, service: Arc<Session>)
where
    B: ServiceWatch + Send + Sync + 'static,
{
    let warm = backend.warm_signal();
    tokio::spawn(async move {
        let mut delay = Duration::ZERO;
        let mut noticed = false;
        loop {
            tokio::select! {
                () = warm.notified() => {}
                () = tokio::time::sleep(delay) => {}
            }
            let session = service.clone();
            let mode = tokio::task::spawn_blocking(move || {
                if session.service_available() {
                    session.poll();
                    ServiceMode::Service
                } else {
                    session.probe_mode()
                }
            })
            .await
            .unwrap_or(ServiceMode::Unavailable);
            if mode == ServiceMode::Unavailable && !noticed {
                noticed = true;
                backend
                    .notice("service_unavailable", SERVICE_UNAVAILABLE_NOTICE)
                    .await;
            } else if mode == ServiceMode::Service && noticed {
                // The service is back: the elevation warning no longer applies.
                noticed = false;
                backend.clear_notice(SERVICE_UNAVAILABLE_NOTICE).await;
            }
            let healthy = backend.tick().await && mode == ServiceMode::Service;
            // A live connection is always checked at the short interval, whatever the
            // backoff, so a lost one is noticed within seconds.
            delay = if healthy || backend.connected().await {
                WATCH_INTERVAL
            } else {
                (delay * 2).clamp(WATCH_INTERVAL, WATCH_BACKOFF_MAX)
            };
        }
    });
}

/// Downloads group icons whenever an installed snapshot names some the cache lacks (or holds
/// an old copy of): imports, refreshes and switches never wait for icon hosts.
async fn spawn_group_icon_fetch<C, P, F>(backend: Arc<CoreBackend<C, P, F>>)
where
    C: multicore_core::HttpClient + 'static,
    P: multicore_core::ProcessController + 'static,
    F: 'static,
    CoreBackend<C, P, F>: Send + Sync,
{
    let requests = backend.group_icon_requests().await;
    tokio::spawn(async move {
        loop {
            requests.notified().await;
            backend.fetch_group_icons().await;
            if backend.fetch_service_logos().await {
                // An import was still being stored: try its logo again shortly.
                tokio::time::sleep(Duration::from_secs(2)).await;
                requests.notify_one();
            }
        }
    });
}

/// The backend calls the watcher needs, behind a small trait so it needs no generic soup.
trait ServiceWatch {
    fn warm_signal(&self) -> Arc<tokio::sync::Notify>;
    fn tick(&self) -> impl std::future::Future<Output = bool> + Send;
    fn notice(&self, phase: &str, message: &str) -> impl std::future::Future<Output = ()> + Send;
    fn clear_notice(&self, message: &str) -> impl std::future::Future<Output = ()> + Send;
    fn connected(&self) -> impl std::future::Future<Output = bool> + Send;
}

impl<C, P, F> ServiceWatch for CoreBackend<C, P, F>
where
    C: multicore_core::HttpClient + 'static,
    P: multicore_core::ProcessController + 'static,
    F: 'static,
{
    fn warm_signal(&self) -> Arc<tokio::sync::Notify> {
        self.warm_requests()
    }

    async fn tick(&self) -> bool {
        self.watch_service().await
    }

    async fn notice(&self, phase: &str, code: &str) {
        self.record_notice(phase, multicore_daemon::DaemonText::code(code))
            .await;
    }

    async fn clear_notice(&self, code: &str) {
        CoreBackend::clear_notice(self, &multicore_daemon::DaemonText::code(code)).await;
    }

    async fn connected(&self) -> bool {
        self.is_connected().await
    }
}

#[cfg(not(windows))]
fn service_controller(
    service: &Arc<Session>,
    generation: u64,
    _mixed_port: u16,
    options: ConnectOptions,
) -> Result<Arc<Controller>, BackendError> {
    Ok(Arc::new(ModeController::Tun(Backed::Service(
        if options.warm_up {
            ServiceTunController::for_warm_up(service.clone(), generation)
        } else {
            ServiceTunController::new(service.clone(), generation)
        },
    ))))
}

#[cfg(windows)]
fn service_controller(
    proxy_mode: &proxy_mode::ProxyMode,
    service: &Arc<Session>,
    generation: u64,
    mixed_port: u16,
    options: ConnectOptions,
) -> Result<Arc<Controller>, BackendError> {
    if options.warm_up {
        // Warming only prepares, and never over a connection that owns the service.
        Ok(Arc::new(ModeController::Tun(Backed::Service(
            ServiceTunController::for_warm_up(service.clone(), generation),
        ))))
    } else if options.tun {
        Ok(Arc::new(ModeController::Tun(Backed::Service(
            ServiceTunController::new(service.clone(), generation),
        ))))
    } else {
        Ok(proxy_mode.service_controller(service, generation, mixed_port))
    }
}

fn random_hex(bytes: usize) -> Result<String, getrandom::Error> {
    let mut buffer = vec![0_u8; bytes];
    getrandom::fill(&mut buffer)?;
    Ok(buffer.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// The package root (parent of both `runtime/`, which this executable lives
/// in, and `cores/`), where an optional vendored `dashboard/` directory is
/// staged as a sibling of `cores/` - not as a sibling of this executable
/// itself, which is one level deeper. Matches
/// `apps/multicore-core-host/src/runtime_paths.rs`'s resolution of the same
/// directory.
/// System-proxy mode (TUN off): the daemon runs the cores itself, as the logged-in user (in a
/// kill-on-close job on Windows, in their own process group on macOS), and points the user's
/// proxy settings at Mihomo while it runs.
#[cfg(any(windows, target_os = "macos"))]
mod proxy_mode {
    use std::{path::PathBuf, sync::Arc};

    #[cfg(target_os = "macos")]
    use multicore_core::UnixGroupLauncher as Launcher;
    #[cfg(windows)]
    use multicore_core::WindowsJobLauncher as Launcher;
    use multicore_core::{
        CORE_READINESS_TIMEOUT, CoreLogBuffer, MihomoRuntimeControl, RuntimePaths,
        SidecarProcessController, Snapshot, stage_runtime_with_mihomo_control,
    };
    #[cfg(windows)]
    use multicore_daemon::service_client::{ServiceCoresController, ServiceSession};
    #[cfg(target_os = "macos")]
    pub(crate) use multicore_daemon::system_proxy::MacNetworkSettings as Settings;
    #[cfg(windows)]
    pub(crate) use multicore_daemon::system_proxy::WinInetSettings as Settings;
    use multicore_daemon::{
        BackendError, PreparedController,
        connection_mode::{Backed, ModeController, SystemProxyController},
        elevated_controller::ProductionElevatedController,
        service_client::{ProductionServiceConnector, ServiceTunController},
        system_proxy::SystemProxy,
    };

    #[cfg(windows)]
    const XRAY: &str = "xray.exe";
    #[cfg(windows)]
    const MIHOMO: &str = "mihomo.exe";
    #[cfg(target_os = "macos")]
    const XRAY: &str = "xray";
    #[cfg(target_os = "macos")]
    const MIHOMO: &str = "mihomo";

    pub(crate) type LocalController = SystemProxyController<SidecarProcessController<Launcher>>;
    #[cfg(windows)]
    pub(crate) type ServiceProxyController =
        SystemProxyController<ServiceCoresController<ProductionServiceConnector>>;
    #[cfg(windows)]
    pub(crate) type Controller = ModeController<
        Backed<ServiceTunController<ProductionServiceConnector>, ProductionElevatedController>,
        Backed<ServiceProxyController, LocalController>,
    >;
    /// macOS has no always-ready service: the proxy mode is always the daemon's own cores.
    #[cfg(target_os = "macos")]
    pub(crate) type Controller = ModeController<
        Backed<ServiceTunController<ProductionServiceConnector>, ProductionElevatedController>,
        LocalController,
    >;

    pub(crate) struct ProxyMode {
        pub(crate) package_root: Option<PathBuf>,
        pub(crate) dashboard_directory: Option<PathBuf>,
        pub(crate) log_path: PathBuf,
        /// The user's core journal; this daemon owns the directory and prunes it.
        pub(crate) journal_directory: PathBuf,
        pub(crate) marker_path: PathBuf,
        pub(crate) tunnel_proxy: multicore_core::TunnelProxy,
    }

    impl ProxyMode {
        /// Proxy mode on top of the service's always-running Mihomo: the daemon only points
        /// the user's proxy settings at its loopback mixed port.
        #[cfg(windows)]
        pub(crate) fn service_controller(
            &self,
            service: &Arc<ServiceSession<ProductionServiceConnector>>,
            generation: u64,
            mixed_port: u16,
        ) -> Arc<Controller> {
            let proxy = SystemProxy::new(Box::new(Settings), self.marker_path.clone(), mixed_port)
                .publishing_to(self.tunnel_proxy.clone());
            Arc::new(ModeController::Proxy(Backed::Service(
                SystemProxyController::new(
                    ServiceCoresController::new(service.clone(), generation),
                    Arc::new(proxy),
                ),
            )))
        }

        pub(crate) fn prepare(
            &self,
            snapshot: &Snapshot,
            runtime_root: &std::path::Path,
            control: MihomoRuntimeControl,
            port: u16,
        ) -> Result<PreparedController<Controller>, BackendError> {
            let control = control
                .with_proxy_mode(port)
                .map_err(|_| BackendError::new())?;
            let staged = stage_runtime_with_mihomo_control(snapshot, runtime_root, &control)
                .map_err(|_| BackendError::new())?;
            let cores = self
                .package_root
                .as_ref()
                .ok_or_else(BackendError::new)?
                .join("cores");
            let paths = RuntimePaths {
                xray_binary: cores.join(XRAY),
                mihomo_binary: cores.join(MIHOMO),
                xray_config: staged.paths().xray_config.clone(),
                mihomo_config: staged.paths().mihomo_config.clone(),
                dashboard_directory: self
                    .dashboard_directory
                    .clone()
                    .filter(|directory| directory.join("index.html").is_file()),
                mihomo_home_directory: None,
            };
            let logs = Arc::new(
                CoreLogBuffer::persistent_or_memory(&self.log_path)
                    .with_journal(&self.journal_directory, true),
            );
            let launcher = Launcher::for_runtime(paths.clone(), logs, CORE_READINESS_TIMEOUT)
                .map_err(|_| BackendError::new())?;
            let proxy = SystemProxy::new(Box::new(Settings), self.marker_path.clone(), port)
                .publishing_to(self.tunnel_proxy.clone());
            let local = SystemProxyController::new(
                SidecarProcessController::with_launcher(paths, launcher),
                Arc::new(proxy),
            );
            #[cfg(windows)]
            let controller = Arc::new(ModeController::Proxy(Backed::OnDemand(local)));
            #[cfg(target_os = "macos")]
            let controller = Arc::new(ModeController::Proxy(local));
            Ok(PreparedController::with_runtime(controller, staged))
        }
    }
}

fn dashboard_directory_for(executable: &std::path::Path) -> Option<std::path::PathBuf> {
    executable
        .parent()
        .and_then(|runtime_dir| runtime_dir.parent())
        .map(std::path::Path::to_path_buf)
}

fn required_env(name: &str) -> io::Result<String> {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("задайте переменную окружения {name} перед запуском daemon"),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::dashboard_directory_for;

    const MAIN_SOURCE: &str = include_str!("main.rs");

    /// Source without whitespace, so the contracts survive rustfmt.
    fn compact(source: &str) -> String {
        source.split_whitespace().collect()
    }

    #[test]
    fn dashboard_directory_is_a_sibling_of_cores_not_of_this_executable() {
        // multicore-daemon.exe lives in `<package>/runtime/`; the vendored
        // dashboard is staged at `<package>/dashboard/`, the same package
        // root apps/multicore-core-host/src/runtime_paths.rs resolves `cores/`
        // from. Regression coverage for a real bug: an earlier version only
        // went up one `.parent()`, landing on `<package>/runtime/dashboard`,
        // which never exists, so the dashboard silently never worked despite
        // package-windows-release.ps1 staging it correctly.
        #[cfg(windows)]
        let executable = std::path::Path::new(
            r"C:\Users\Example\AppData\Local\Programs\MultiCore\current\runtime\multicore-daemon.exe",
        );
        #[cfg(not(windows))]
        let executable = std::path::Path::new("/usr/lib/multicore/runtime/multicore-daemon");

        let package_root = dashboard_directory_for(executable).expect("package root resolves");
        assert_eq!(package_root, executable.parent().unwrap().parent().unwrap());
        assert_ne!(
            package_root.join("dashboard"),
            executable.parent().unwrap().join("dashboard"),
            "must not resolve to a sibling of the executable itself"
        );
    }

    #[test]
    fn live_dns_does_not_resolve_and_pin_endpoint_ips_during_connect() {
        let production = MAIN_SOURCE.split("#[cfg(test)]").next().unwrap();
        assert!(!production.contains("to_socket_addrs"));
        assert!(!production.contains("with_xray_resolved_hosts"));
    }

    #[test]
    fn the_daemon_serves_the_read_only_dashboard_and_mihomo_serves_no_ui() {
        let production = MAIN_SOURCE.split("#[cfg(test)]").next().unwrap();
        assert!(!production.contains("with_external_ui_directory"));
        assert!(!production.contains("/ui/#/setup"));
        let compact = compact(production);
        assert!(compact.contains("dashboard_proxy::dashboard_router("));
        assert!(compact.contains("controller_slot.clone()"));
        assert!(compact.contains("dashboard_directory:None"));
    }

    #[cfg(windows)]
    #[test]
    fn proxy_mode_uses_the_service_cores_when_the_service_is_present() {
        let production = MAIN_SOURCE.split("#[cfg(test)]").next().unwrap();
        let (_, proxy_mode) = production.split_once("mod proxy_mode {").unwrap();
        let proxy_mode_compact = compact(proxy_mode);
        let production = compact(production);
        assert!(proxy_mode_compact.contains("ServiceCoresController::new("));
        assert!(proxy_mode_compact.contains(
            "SystemProxy::new(Box::new(WinInetSettings),self.marker_path.clone(),mixed_port"
        ));
        assert!(production.contains("if!options.tun&&!use_service{"));
        assert!(production.contains("proxy_mode.service_controller("));
    }

    #[test]
    fn every_mixed_port_is_the_saved_local_proxy_port() {
        let production = MAIN_SOURCE.split("#[cfg(test)]").next().unwrap();
        let compact = compact(production);
        // Chosen and saved at start, so Settings can show it before the first connection.
        assert!(compact.contains("LocalProxyPort::open(&data_directory)"));
        // The service path keeps the saved port (its previous generation may hold it) and
        // checks it only after the service rejected a generation (it may be another app's).
        assert!(compact.contains("WarmMiss::PrepareRejected=>local_proxy_port.verified()"));
        assert!(compact.contains("_=>local_proxy_port.current()"));
        // Proxy mode without the service runs no core of ours on it: checked at once.
        assert!(compact.contains(
            "proxy_mode.prepare(snapshot,&runtime_root,control,local_proxy_port.verified()"
        ));
        // No random per-connection port is left.
        assert!(!production.contains("free_loopback_port"));
    }

    #[test]
    fn production_daemon_prefers_the_service_and_falls_back_to_elevation() {
        let production = MAIN_SOURCE.split("#[cfg(test)]").next().unwrap();
        let (tun_path, proxy_mode) = production
            .split_once("mod proxy_mode {")
            .expect("proxy mode module");
        let tun_path_compact = compact(tun_path);
        // The service is re-probed by every Connect and warm-up (not once per start), and the
        // warm-up (or the automatic reconnect after a service loss) never falls back to an
        // elevation prompt.
        let probe = tun_path_compact
            .find("factory_service.probe_mode()")
            .unwrap();
        assert!(probe < tun_path_compact.find("production_factory()").unwrap());
        assert!(
            tun_path_compact.contains("if(options.warm_up||options.service_only)&&!use_service{")
        );
        assert!(tun_path_compact.contains(".enable_always_ready("));
        assert!(tun_path_compact.contains("spawn_service_watch("));
        assert!(tun_path_compact.contains("SERVICE_UNAVAILABLE_NOTICE"));
        // Warm generations are keyed by the payloads and the physical interface; a rejected
        // one is re-staged.
        assert!(tun_path_compact.contains("WarmInputs::new(snapshot,&interface)"));
        assert!(tun_path_compact.contains("warm_stages.reusable(&inputs,&factory_service)"));
        assert!(tun_path_compact.contains("PreparedController::reuse("));
        // The on-demand path is unchanged: elevated host per generation.
        assert!(tun_path.contains("elevated_factory"));
        assert!(tun_path.contains(".controller(generation)"));
        assert!(tun_path.contains("runtime_generation_from_path"));
        for forbidden in ["SidecarProcessController", "RuntimePaths"] {
            assert!(
                !tun_path.contains(forbidden),
                "the TUN path must not launch cores itself: {forbidden}"
            );
        }
        assert!(proxy_mode.contains("SystemProxyController::new("));
        assert!(proxy_mode.contains(".join(\"cores\")"));
        for forbidden in ["MULTICORE_XRAY_BIN", "MULTICORE_MIHOMO_BIN"] {
            assert!(!production.contains(forbidden), "{forbidden}");
        }
        assert!(
            tun_path.find("system_proxy::recover(").unwrap()
                < tun_path.find("axum::serve(").unwrap()
        );
    }
}
