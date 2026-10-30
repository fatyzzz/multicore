//! The always-ready service seen from the daemon: one persistent channel, the controller
//! endpoint it hands out, and `ProcessController`s that turn Connect / Disconnect into
//! `Prepare` + `SetTun` instead of starting and stopping processes.

use std::{
    fmt,
    future::Future,
    net::{Ipv4Addr, SocketAddr},
    pin::Pin,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use multicore_core::{
    CORE_READINESS_TIMEOUT, CoreLogRecord, DiagnosticStream, Engine, ProcessController,
    ProcessDiagnostics, RuntimeCheckState, Snapshot, StartFailureCause,
    elevation_protocol::{
        BrokerErrorCode, ElevatedDiagnosticStream, ElevatedEngine, ElevatedRuntimeState,
    },
    service_protocol::{
        MAX_SERVICE_LOG_TAIL_BYTES, ServiceCommand, ServiceLogFile, ServiceResponse,
        ServiceUpdateStage, service_response_matches,
    },
};
use sha2::{Digest, Sha256};

use crate::{LatencyProbeError, MihomoHttpSelector, MihomoSelector, SelectorError, TrafficTotals};

/// Where the current Mihomo controller listens. The secret never leaves this process except
/// towards Mihomo itself.
#[derive(Clone)]
pub struct ControllerTarget {
    pub address: SocketAddr,
    secret: Arc<str>,
    pub mixed_port: Option<u16>,
}

impl ControllerTarget {
    #[must_use]
    pub fn secret(&self) -> &str {
        &self.secret
    }
}

impl fmt::Debug for ControllerTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ControllerTarget")
            .field("address", &self.address)
            .field("secret", &"[REDACTED]")
            .field("mixed_port", &self.mixed_port)
            .finish()
    }
}

/// The controller endpoint shared by the selector, the dashboard proxy and the service
/// session. On-demand mode sets it once; the service replaces it on every `Prepare`.
#[derive(Default)]
pub struct ControllerSlot {
    target: RwLock<Option<ControllerTarget>>,
    version: AtomicU64,
}

impl ControllerSlot {
    pub fn set(&self, address: SocketAddr, secret: &str, mixed_port: Option<u16>) {
        if let Ok(mut target) = self.target.write() {
            *target = Some(ControllerTarget {
                address,
                secret: Arc::from(secret),
                mixed_port,
            });
        }
        self.version.fetch_add(1, Ordering::AcqRel);
    }

    #[must_use]
    pub fn get(&self) -> Option<ControllerTarget> {
        self.target.read().ok().and_then(|target| target.clone())
    }

    #[must_use]
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }
}

/// A `MihomoSelector` that always talks to the slot's current controller.
pub struct SlotSelector {
    slot: Arc<ControllerSlot>,
    cached: Mutex<Option<(u64, Arc<MihomoHttpSelector>)>>,
}

impl SlotSelector {
    #[must_use]
    pub fn new(slot: Arc<ControllerSlot>) -> Self {
        Self {
            slot,
            cached: Mutex::new(None),
        }
    }

    fn current(&self) -> Option<Arc<MihomoHttpSelector>> {
        let version = self.slot.version();
        let mut cached = self.cached.lock().ok()?;
        if let Some((cached_version, selector)) = cached.as_ref()
            && *cached_version == version
        {
            return Some(selector.clone());
        }
        let target = self.slot.get()?;
        let selector = Arc::new(MihomoHttpSelector::new(target.address, target.secret()).ok()?);
        *cached = Some((version, selector.clone()));
        Some(selector)
    }
}

#[async_trait]
impl MihomoSelector for SlotSelector {
    async fn select(&self, raw_group: &str, raw_node: &str) -> Result<(), SelectorError> {
        let selector = self.current().ok_or_else(SelectorError::new)?;
        selector.select(raw_group, raw_node).await
    }

    async fn probe_latency(&self, raw_name: &str) -> Result<u64, LatencyProbeError> {
        let selector = self.current().ok_or(LatencyProbeError::Unavailable)?;
        selector.probe_latency(raw_name).await
    }

    async fn traffic_totals(&self) -> Result<TrafficTotals, SelectorError> {
        let selector = self.current().ok_or_else(SelectorError::new)?;
        selector.traffic_totals().await
    }

    async fn proxy_now(&self, raw_name: &str) -> Result<Option<String>, SelectorError> {
        let selector = self.current().ok_or_else(SelectorError::new)?;
        selector.proxy_now(raw_name).await
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceError {
    /// No MultiCore service is registered on this machine.
    NotInstalled,
    /// The service is registered but not reachable right now.
    Unavailable,
    /// Something other than the registered service owns the channel.
    Impostor,
    /// Another interactive user's session holds the cores.
    Busy,
    /// A connection owns the service's cores: a warm-up must not replace its generation.
    InUse,
    Disconnected,
    TimedOut,
    Protocol,
    Rejected(BrokerErrorCode),
    /// The service refused to switch the TUN adapter on (`SetTun{on:true}` during Connect).
    TunRejected(BrokerErrorCode),
}

impl fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "MultiCore service: {self:?}")
    }
}

impl std::error::Error for ServiceError {}

impl ServiceError {
    #[must_use]
    pub fn start_failure_cause(self) -> StartFailureCause {
        match self {
            Self::Busy => StartFailureCause::ServiceBusy,
            Self::Rejected(BrokerErrorCode::TimedOut) => StartFailureCause::ReadinessTimedOut,
            Self::Rejected(_) => StartFailureCause::CoreFailed,
            Self::TunRejected(BrokerErrorCode::TimedOut) => StartFailureCause::TunTimedOut,
            Self::TunRejected(_) => StartFailureCause::TunFailed,
            _ => StartFailureCause::BrokerFailed,
        }
    }
}

pub trait ServiceTransport: Send {
    fn request(&self, command: &ServiceCommand) -> Result<ServiceResponse, ServiceError>;
}

pub trait ServiceConnector: Send + Sync + 'static {
    type Transport: ServiceTransport;
    fn connect(&self) -> Result<Self::Transport, ServiceError>;
}

/// How long to wait for the service's answer to `command`. A timed-out exchange drops the
/// channel (the framing is lost), and a dropped channel makes the service stop the cores, so
/// even the read-only commands get a generous budget.
#[must_use]
pub fn service_response_timeout(command: &ServiceCommand) -> Duration {
    match command {
        // Both cores' readiness, plus the TUN switch a re-prepare of a live tunnel performs.
        ServiceCommand::Prepare { .. } => {
            CORE_READINESS_TIMEOUT * 2
                + multicore_core::service_protocol::SET_TUN_RESPONSE_TIMEOUT
                + Duration::from_secs(10)
        }
        ServiceCommand::SetTun { .. } => multicore_core::service_protocol::SET_TUN_RESPONSE_TIMEOUT,
        ServiceCommand::Update { .. } | ServiceCommand::StageUpdate { .. } => {
            Duration::from_secs(15 * 60)
        }
        // The service answers both under its lock, where a crash restart of both cores (each
        // core's readiness, then the tunnel back on) may be running: a shorter budget dropped
        // the channel mid-restart, and the dropped channel made the service stop the very
        // cores it had just brought back.
        ServiceCommand::Status | ServiceCommand::Diagnostics => {
            CORE_READINESS_TIMEOUT * 2
                + multicore_core::service_protocol::SET_TUN_RESPONSE_TIMEOUT
                + Duration::from_secs(10)
        }
        // Each fix waits at most a minute (a tool, a service stop); a batch has at most eight.
        ServiceCommand::Troubleshoot { .. } => Duration::from_secs(10 * 60),
        // One small file read, but it may wait behind a `Prepare` holding the service's lock.
        ServiceCommand::ServiceLog { .. } | ServiceCommand::CoreJournal { .. } => {
            Duration::from_secs(30)
        }
        _ => Duration::from_secs(5),
    }
}

/// What a probe found: the service to use, nothing installed (or installed for another copy
/// of MultiCore), or an installed service that cannot be reached right now.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceMode {
    Service,
    Unavailable,
    Absent,
}

impl ServiceMode {
    #[must_use]
    pub fn from_probe(result: Result<(), ServiceError>) -> Self {
        match result {
            // Busy: the service is there; Prepare reports the other user.
            Ok(()) | Err(ServiceError::Busy) => Self::Service,
            Err(
                ServiceError::NotInstalled
                | ServiceError::Rejected(BrokerErrorCode::AuthenticationFailed),
            ) => Self::Absent,
            Err(_) => Self::Unavailable,
        }
    }
}

/// What the daemon currently carries through the service: nothing, the system proxy on the
/// service's mixed port (TUN off), or the tunnel.
const IN_USE_NONE: u8 = 0;
const IN_USE_PROXY: u8 = 1;
const IN_USE_TUN: u8 = 2;

/// Controller address, token and mixed port of a prepared generation.
type Endpoint = (SocketAddr, Arc<str>, Option<u16>);

pub struct ServiceSession<C: ServiceConnector> {
    connector: C,
    transport: Mutex<Option<C::Transport>>,
    prepared: Mutex<Option<u64>>,
    /// The endpoint of the prepared generation, re-published when `Prepare` is a no-op.
    endpoint: Mutex<Option<Endpoint>>,
    /// The generation whose last `Prepare` the service rejected; it must be re-staged.
    rejected: Mutex<Option<u64>>,
    healthy: AtomicBool,
    tun_on: AtomicBool,
    in_use: AtomicU8,
    /// The generation a Connect claimed before its `Prepare`; a warm-up never prepares while
    /// it is set. Changed only under the transport lock.
    owner: Mutex<Option<u64>>,
    available: AtomicBool,
    controller: Arc<ControllerSlot>,
    /// What the last failed exchange or unhealthy `Status` saw (see `loss_details`).
    last_trouble: Mutex<Option<serde_json::Value>>,
}

/// A command's name for the loss details (never its arguments).
fn command_label(command: &ServiceCommand) -> &'static str {
    match command {
        ServiceCommand::Prepare { .. } => "Prepare",
        ServiceCommand::SetTun { .. } => "SetTun",
        ServiceCommand::Status => "Status",
        ServiceCommand::Stop { .. } => "Stop",
        ServiceCommand::Diagnostics => "Diagnostics",
        ServiceCommand::ServiceLog { .. } => "ServiceLog",
        ServiceCommand::CoreJournal { .. } => "CoreJournal",
        ServiceCommand::Shutdown => "Shutdown",
        ServiceCommand::Update { .. } => "Update",
        ServiceCommand::StageUpdate { .. } => "StageUpdate",
        ServiceCommand::UpdateProgress => "UpdateProgress",
        ServiceCommand::Troubleshoot { .. } => "Troubleshoot",
    }
}

/// How far the service's running update got (`ServiceSession::update_progress`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ServiceUpdateProgress {
    pub stage: ServiceUpdateStage,
    pub received: u64,
    pub total: u64,
}

impl<C: ServiceConnector> ServiceSession<C> {
    pub fn new(connector: C, controller: Arc<ControllerSlot>) -> Self {
        Self {
            connector,
            transport: Mutex::new(None),
            prepared: Mutex::new(None),
            endpoint: Mutex::new(None),
            rejected: Mutex::new(None),
            healthy: AtomicBool::new(false),
            tun_on: AtomicBool::new(false),
            in_use: AtomicU8::new(IN_USE_NONE),
            owner: Mutex::new(None),
            available: AtomicBool::new(false),
            controller,
            last_trouble: Mutex::new(None),
        }
    }

    fn note_trouble(&self, trouble: serde_json::Value) {
        if let Ok(mut last) = self.last_trouble.lock() {
            *last = Some(trouble);
        }
    }

    /// Why `connection_lost` holds, for the loss event: the last failed exchange (which
    /// command, the error, how long it took) or unhealthy `Status` (each core's state, the
    /// failure code, the tunnel, the generation), and the session's flags now.
    #[must_use]
    pub fn loss_details(&self) -> serde_json::Value {
        let replaced = self
            .owner()
            .is_some_and(|owner| self.prepared_generation() != Some(owner));
        serde_json::json!({
            "last": self.last_trouble.lock().ok().and_then(|last| last.clone()),
            "in_use": match self.in_use.load(Ordering::Acquire) {
                IN_USE_PROXY => "proxy",
                IN_USE_TUN => "tun",
                _ => "none",
            },
            "cores_ready": self.cores_ready(),
            "tun_on": self.tun_on.load(Ordering::Acquire),
            "generation_replaced": replaced,
        })
    }

    fn prepared_generation(&self) -> Option<u64> {
        self.prepared.lock().ok().and_then(|prepared| *prepared)
    }

    fn forget(&self) {
        if let Ok(mut prepared) = self.prepared.lock() {
            *prepared = None;
        }
        self.healthy.store(false, Ordering::Release);
        self.tun_on.store(false, Ordering::Release);
    }

    /// Sends `command`, connecting first when `connect` allows it, and hands the answer to
    /// `record` while the channel is still held, so readiness bookkeeping never interleaves
    /// with another exchange. A failed exchange drops the channel; the service then stops the
    /// cores, so the prepared generation is forgotten.
    fn exchange<R>(
        &self,
        command: &ServiceCommand,
        connect: bool,
        record: impl FnOnce(&Self, &ServiceResponse) -> R,
    ) -> Result<R, ServiceError> {
        self.exchange_if(command, connect, |_| Ok(()), record)
    }

    /// `exchange`, sent only when `allowed` passes with the channel held.
    fn exchange_if<R>(
        &self,
        command: &ServiceCommand,
        connect: bool,
        allowed: impl FnOnce(&Self) -> Result<(), ServiceError>,
        record: impl FnOnce(&Self, &ServiceResponse) -> R,
    ) -> Result<R, ServiceError> {
        let mut transport = self
            .transport
            .lock()
            .map_err(|_| ServiceError::Disconnected)?;
        allowed(self)?;
        if transport.is_none() {
            if !connect {
                return Err(ServiceError::Disconnected);
            }
            self.forget();
            *transport = Some(self.connector.connect()?);
        }
        let started = Instant::now();
        let result = transport
            .as_ref()
            .expect("service transport exists")
            .request(command);
        let response = match result {
            Ok(response) => response,
            Err(error) => {
                transport.take();
                self.forget();
                self.note_trouble(serde_json::json!({
                    "cause": "channel",
                    "command": command_label(command),
                    "error": format!("{error:?}"),
                    "elapsed_ms": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                }));
                return Err(error);
            }
        };
        match response {
            ServiceResponse::Busy => Err(ServiceError::Busy),
            ServiceResponse::Error { code } => Err(ServiceError::Rejected(code)),
            response => Ok(record(self, &response)),
        }
    }

    fn request(
        &self,
        command: &ServiceCommand,
        connect: bool,
    ) -> Result<ServiceResponse, ServiceError> {
        self.exchange(command, connect, |_, response| response.clone())
    }

    fn has_channel(&self) -> bool {
        self.transport
            .lock()
            .map(|transport| transport.is_some())
            .unwrap_or(false)
    }

    pub fn probe(&self) -> Result<(), ServiceError> {
        let had_channel = self.has_channel();
        let result = match self.request(&ServiceCommand::Status, true) {
            // A channel that died since its last use says nothing about the service: retry on
            // a fresh one.
            Err(ServiceError::Disconnected | ServiceError::TimedOut) if had_channel => {
                self.request(&ServiceCommand::Status, true)
            }
            result => result,
        };
        match result? {
            ServiceResponse::Status { .. } => Ok(()),
            _ => Err(ServiceError::Protocol),
        }
    }

    /// Probes the service and remembers whether it can be used right now. Called on every
    /// Connect and by the background watcher, so a service that starts after the app is
    /// picked up without a restart.
    pub fn probe_mode(&self) -> ServiceMode {
        let mode = ServiceMode::from_probe(self.probe());
        self.available
            .store(mode == ServiceMode::Service, Ordering::Release);
        mode
    }

    /// The last probe found the service usable.
    #[must_use]
    pub fn service_available(&self) -> bool {
        self.available.load(Ordering::Acquire)
    }

    fn publish(&self, endpoint: &Endpoint) {
        self.controller.set(endpoint.0, &endpoint.1, endpoint.2);
    }

    pub fn prepare(&self, generation: u64) -> Result<(), ServiceError> {
        self.prepare_if(generation, false)
    }

    /// The warm-up's `Prepare`: refused with `InUse` while a connection owns the session, so
    /// a warm-up that lost a race with a Connect never replaces the connection's generation.
    pub fn prepare_idle(&self, generation: u64) -> Result<(), ServiceError> {
        self.prepare_if(generation, true)
    }

    fn owner(&self) -> Option<u64> {
        self.owner.lock().ok().and_then(|owner| *owner)
    }

    /// A Connect takes the session for `generation` before its `Prepare`. Taken under the
    /// transport lock, so it is ordered against a warm-up `Prepare` in flight.
    fn claim(&self, generation: u64) {
        let _channel = self.transport.lock();
        if let Ok(mut owner) = self.owner.lock() {
            *owner = Some(generation);
        }
    }

    fn release_claim(&self) {
        if let Ok(mut owner) = self.owner.lock() {
            *owner = None;
        }
    }

    fn prepare_if(&self, generation: u64, idle: bool) -> Result<(), ServiceError> {
        if idle && self.owner().is_some_and(|owner| owner != generation) {
            return Err(ServiceError::InUse);
        }
        if self.prepared_generation() == Some(generation) && self.healthy.load(Ordering::Acquire) {
            // The slot may have been pointed at an on-demand controller meanwhile.
            if let Some(endpoint) = self.endpoint.lock().ok().and_then(|value| value.clone()) {
                self.publish(&endpoint);
            }
            return Ok(());
        }
        let result = self.exchange_if(
            &ServiceCommand::Prepare {
                generation_id: generation,
            },
            true,
            |session| {
                if idle && session.owner().is_some_and(|owner| owner != generation) {
                    Err(ServiceError::InUse)
                } else {
                    Ok(())
                }
            },
            |session, response| match response {
                ServiceResponse::Prepared {
                    generation_id,
                    controller,
                } if *generation_id == generation => {
                    let endpoint = (
                        SocketAddr::from((Ipv4Addr::LOCALHOST, controller.port)),
                        Arc::<str>::from(controller.secret.as_str()),
                        // 0 is a TUN-form generation: the service opened no mixed port.
                        Some(controller.mixed_port).filter(|port| *port != 0),
                    );
                    session.publish(&endpoint);
                    if let Ok(mut stored) = session.endpoint.lock() {
                        *stored = Some(endpoint);
                    }
                    if let Ok(mut prepared) = session.prepared.lock() {
                        *prepared = Some(generation);
                    }
                    if let Ok(mut rejected) = session.rejected.lock() {
                        *rejected = None;
                    }
                    session.healthy.store(true, Ordering::Release);
                    Ok(())
                }
                _ => Err(ServiceError::Protocol),
            },
        );
        let result = result.and_then(|result| result);
        if let Err(ServiceError::Rejected(_) | ServiceError::Protocol | ServiceError::TimedOut) =
            result
            && let Ok(mut rejected) = self.rejected.lock()
        {
            *rejected = Some(generation);
        }
        result
    }

    /// The service rejected the last `Prepare` of `generation` (a port taken, a core that
    /// failed to start): reusing it would fail the same way.
    #[must_use]
    pub fn prepare_failed(&self, generation: u64) -> bool {
        self.rejected
            .lock()
            .map(|rejected| *rejected == Some(generation))
            .unwrap_or(false)
    }

    pub fn set_tun(&self, on: bool) -> Result<(), ServiceError> {
        self.exchange(
            &ServiceCommand::SetTun { on },
            false,
            |session, response| {
                match response {
                    ServiceResponse::TunState { on: actual } => {
                        session.tun_on.store(*actual, Ordering::Release);
                        if *actual == on {
                            Ok(())
                        } else if on {
                            // The service rolled the tunnel back: the adapter never came up.
                            Err(ServiceError::Rejected(BrokerErrorCode::TimedOut))
                        } else {
                            Err(ServiceError::Protocol)
                        }
                    }
                    _ => Err(ServiceError::Protocol),
                }
            },
        )?
    }

    /// Refreshes readiness from `Status` over an existing channel (never connects).
    pub fn poll(&self) {
        let result = self.exchange(&ServiceCommand::Status, false, |session, response| {
            if let ServiceResponse::Status {
                generation_id,
                xray,
                mihomo,
                tun_on,
                failure,
            } = response
            {
                session.tun_on.store(*tun_on, Ordering::Release);
                let healthy = failure.is_none()
                    && *xray == ElevatedRuntimeState::Ready
                    && *mihomo == ElevatedRuntimeState::Ready
                    && generation_id.is_some()
                    && *generation_id == session.prepared_generation();
                session.healthy.store(healthy, Ordering::Release);
                let tunnel_dropped =
                    !*tun_on && session.in_use.load(Ordering::Acquire) == IN_USE_TUN;
                if !healthy || tunnel_dropped {
                    session.note_trouble(serde_json::json!({
                        "cause": "status",
                        "generation": generation_id,
                        "prepared": session.prepared_generation(),
                        "xray": format!("{xray:?}"),
                        "mihomo": format!("{mihomo:?}"),
                        "tun_on": tun_on,
                        "failure": failure.map(|code| format!("{code:?}")),
                    }));
                }
            } else {
                session.healthy.store(false, Ordering::Release);
                session.note_trouble(serde_json::json!({
                    "cause": "status",
                    "error": "unexpected answer",
                }));
            }
        });
        if result.is_err() {
            self.healthy.store(false, Ordering::Release);
        }
    }

    #[must_use]
    pub fn cores_ready(&self) -> bool {
        self.healthy.load(Ordering::Acquire) && self.prepared_generation().is_some()
    }

    fn set_in_use(&self, mode: u8) {
        self.in_use.store(mode, Ordering::Release);
    }

    /// A connection carried by the service is gone: the cores stopped or the channel dropped
    /// (proxy mode), or the tunnel went down as well (TUN mode).
    #[must_use]
    pub fn connection_lost(&self) -> bool {
        // The service runs another generation than the one the connection was made on (its
        // mixed port, and its tunnel, are gone).
        let replaced = || {
            self.owner()
                .is_some_and(|owner| self.prepared_generation() != Some(owner))
        };
        match self.in_use.load(Ordering::Acquire) {
            IN_USE_PROXY => !self.cores_ready() || replaced(),
            IN_USE_TUN => !self.cores_ready() || replaced() || !self.tun_on.load(Ordering::Acquire),
            _ => false,
        }
    }

    pub fn shutdown(&self) {
        if let Ok(mut transport) = self.transport.lock()
            && let Some(transport) = transport.take()
        {
            let _ = transport.request(&ServiceCommand::Shutdown);
        }
        self.set_in_use(IN_USE_NONE);
        self.forget();
    }

    /// Asks the service to download, verify and install `version`. The exchange can take
    /// minutes, so it runs on a channel of its own: the connection's channel (selection,
    /// latency, disconnect) stays free, and a failed download leaves the cores running. The
    /// service downloads outside its loop lock and keeps answering `Status`, `SetTun` and
    /// `Diagnostics` on that channel meanwhile, so the watcher keeps polling as usual and a
    /// core crash during the download is still noticed.
    pub fn update(&self, version: &str) -> Result<(), ServiceError> {
        let transport = self.connector.connect()?;
        match transport.request(&ServiceCommand::Update {
            version: version.to_owned(),
        })? {
            ServiceResponse::UpdateStarted => Ok(()),
            ServiceResponse::Busy => Err(ServiceError::Busy),
            ServiceResponse::Error { code } => Err(ServiceError::Rejected(code)),
            _ => Err(ServiceError::Protocol),
        }
    }

    /// «Устранить неполадки»: the service runs these fixed fixes and reports one outcome
    /// each. On a channel of its own, like `update`: an older service rejects the unknown
    /// command (`Rejected(InvalidRequest)`) and closes only this channel.
    pub fn troubleshoot(
        &self,
        fixes: &[multicore_core::troubleshoot::PrivilegedFix],
    ) -> Result<Vec<multicore_core::troubleshoot::FixOutcome>, ServiceError> {
        let transport = self.connector.connect()?;
        match transport.request(&ServiceCommand::Troubleshoot {
            fixes: fixes.to_vec(),
        })? {
            ServiceResponse::Troubleshot { outcomes } if outcomes.len() == fixes.len() => {
                Ok(outcomes)
            }
            ServiceResponse::Busy => Err(ServiceError::Busy),
            ServiceResponse::Error { code } => Err(ServiceError::Rejected(code)),
            _ => Err(ServiceError::Protocol),
        }
    }

    /// Asks the service to download and verify `version` without stopping anything: the
    /// connected desktop stages first, disconnects only once this succeeded, then calls
    /// `update`, which hands the staged release over. Runs on a channel of its own like
    /// `update`, so the connection keeps working during the download.
    pub fn stage_update(&self, version: &str) -> Result<(), ServiceError> {
        let transport = self.connector.connect()?;
        match transport.request(&ServiceCommand::StageUpdate {
            version: version.to_owned(),
        })? {
            ServiceResponse::UpdateStaged { version: staged } if staged == version => Ok(()),
            ServiceResponse::Busy => Err(ServiceError::Busy),
            ServiceResponse::Error { code } => Err(ServiceError::Rejected(code)),
            _ => Err(ServiceError::Protocol),
        }
    }

    /// How far the service's running `stage_update`/`update` got, for the desktop's progress
    /// bar. On a channel of its own, like `troubleshoot`, while the update's channel waits: an
    /// older service rejects the unknown command (`Rejected(InvalidRequest)`) and closes only
    /// this channel.
    pub fn update_progress(&self) -> Result<ServiceUpdateProgress, ServiceError> {
        let transport = self.connector.connect()?;
        let command = ServiceCommand::UpdateProgress;
        match transport.request(&command)? {
            ServiceResponse::Busy => Err(ServiceError::Busy),
            ServiceResponse::Error { code } => Err(ServiceError::Rejected(code)),
            ref response if !service_response_matches(&command, response) => {
                Err(ServiceError::Protocol)
            }
            ServiceResponse::UpdateProgress {
                stage,
                received,
                total,
            } => Ok(ServiceUpdateProgress {
                stage,
                received,
                total,
            }),
            _ => Err(ServiceError::Protocol),
        }
    }

    /// «Сохранить лог…»: the newest redacted lines of the service's own log, `service.log`
    /// then its predecessor, at most [`MAX_SERVICE_LOG_TAIL_BYTES`] of text in all. On a
    /// channel of its own, like `troubleshoot`: an older service rejects the unknown command
    /// (`Rejected(InvalidRequest)`) and closes only that channel, and reading claims nothing.
    pub fn service_log(&self) -> Result<Vec<ServiceLogText>, ServiceError> {
        let transport = self.connector.connect()?;
        let mut budget = usize::try_from(MAX_SERVICE_LOG_TAIL_BYTES).unwrap_or(usize::MAX);
        let mut files = Vec::new();
        for file in [ServiceLogFile::Current, ServiceLogFile::Previous] {
            let mut pieces = Vec::new();
            let mut before = None;
            let mut complete = false;
            for _ in 0..MAX_SERVICE_LOG_PAGES {
                if budget == 0 {
                    break;
                }
                let command = ServiceCommand::ServiceLog { file, before };
                let response = transport.request(&command)?;
                let (start, end, text) = match response {
                    ServiceResponse::Busy => return Err(ServiceError::Busy),
                    ServiceResponse::Error { code } => return Err(ServiceError::Rejected(code)),
                    ref chunk if !service_response_matches(&command, chunk) => {
                        return Err(ServiceError::Protocol);
                    }
                    ServiceResponse::ServiceLog {
                        start, end, text, ..
                    } => (start, end, text),
                    _ => return Err(ServiceError::Protocol),
                };
                // The file was rotated while it was paged: what came before is another file.
                if before.is_some_and(|before| end != before) {
                    break;
                }
                if text.len() > budget {
                    pieces.push(newest_lines(&text, budget).to_owned());
                    budget = 0;
                    break;
                }
                budget -= text.len();
                pieces.push(text);
                if start == 0 {
                    complete = true;
                    break;
                }
                if start == end {
                    break;
                }
                before = Some(start);
            }
            pieces.reverse();
            files.push(ServiceLogText {
                file,
                text: pieces.concat(),
                truncated: !complete,
            });
        }
        Ok(files)
    }

    /// «Сохранить лог…»: this user's core journal in the service (see
    /// `multicore_core::core_sessions`), from segments written at or after `since_ms`, oldest
    /// first. At most [`MAX_CORE_JOURNAL_BYTES`] of text: past that the oldest pages are
    /// dropped (`truncated`). On a channel of its own like `service_log`: an older service
    /// rejects the unknown command (`Rejected(InvalidRequest)`), and reading claims nothing.
    pub fn core_journal(&self, since_ms: u64) -> Result<(String, bool), ServiceError> {
        let transport = self.connector.connect()?;
        let mut pages = std::collections::VecDeque::new();
        let mut held = 0_usize;
        let mut truncated = false;
        let mut cursor = None;
        for _ in 0..MAX_CORE_JOURNAL_PAGES {
            let command = ServiceCommand::CoreJournal { since_ms, cursor };
            let response = transport.request(&command)?;
            let (text, next) = match response {
                ServiceResponse::Busy => return Err(ServiceError::Busy),
                ServiceResponse::Error { code } => return Err(ServiceError::Rejected(code)),
                ref page if !service_response_matches(&command, page) => {
                    return Err(ServiceError::Protocol);
                }
                ServiceResponse::CoreJournal { text, next } => (text, next),
                _ => return Err(ServiceError::Protocol),
            };
            held += text.len();
            pages.push_back(text);
            while held > MAX_CORE_JOURNAL_BYTES {
                let Some(oldest) = pages.pop_front() else {
                    break;
                };
                held -= oldest.len();
                truncated = true;
            }
            match next {
                Some(next) => cursor = Some(next),
                None => return Ok((pages.into_iter().collect(), truncated)),
            }
        }
        Ok((pages.into_iter().collect(), true))
    }

    #[must_use]
    pub fn diagnostics(&self) -> ProcessDiagnostics {
        match self.request(&ServiceCommand::Diagnostics, false) {
            Ok(ServiceResponse::ProcessDiagnostics {
                xray,
                mihomo,
                tun,
                logs,
            }) => ProcessDiagnostics {
                xray: check_state(xray),
                mihomo: check_state(mihomo),
                tun: check_state(tun),
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
            _ => ProcessDiagnostics::default(),
        }
    }
}

impl<C: ServiceConnector> crate::CoresReadiness for ServiceSession<C> {
    fn cores_ready(&self) -> bool {
        ServiceSession::cores_ready(self)
    }

    fn service_available(&self) -> bool {
        ServiceSession::service_available(self)
    }

    fn connection_lost(&self) -> bool {
        ServiceSession::connection_lost(self)
    }

    fn loss_details(&self) -> serde_json::Value {
        ServiceSession::loss_details(self)
    }

    fn release(&self) {
        ServiceSession::shutdown(self);
    }
}

impl<C: ServiceConnector> crate::ServiceUpdates for ServiceSession<C> {
    fn update(&self, version: &str) -> Result<(), ServiceError> {
        ServiceSession::update(self, version)
    }

    fn stage(&self, version: &str) -> Result<(), ServiceError> {
        ServiceSession::stage_update(self, version)
    }

    fn progress(&self) -> Result<ServiceUpdateProgress, ServiceError> {
        ServiceSession::update_progress(self)
    }
}

/// Requests per file before the daemon stops paging: a 512 KiB tail is about 35 chunks.
const MAX_SERVICE_LOG_PAGES: usize = 128;
/// The most core journal text the daemon collects for one archive: the service keeps at most
/// this much per user anyway.
pub const MAX_CORE_JOURNAL_BYTES: usize =
    multicore_core::core_sessions::JOURNAL_TOTAL_BYTES as usize;
/// Pages before the daemon stops paging the core journal (a page holds up to 32 KiB).
const MAX_CORE_JOURNAL_PAGES: usize = 8_192;

/// One file of the service's own log as `ServiceSession::service_log` collected it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceLogText {
    pub file: ServiceLogFile,
    /// Whole, redacted lines, oldest first.
    pub text: String,
    /// Older lines exist that are not in `text`.
    pub truncated: bool,
}

/// The whole lines at the end of `text` that fit in `budget` bytes.
fn newest_lines(text: &str, budget: usize) -> &str {
    if text.len() <= budget {
        return text;
    }
    let mut start = text.len() - budget;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let tail = &text[start..];
    // Unless the cut fell exactly on a line start, drop the partial first line.
    if text.as_bytes()[start - 1] == b'\n' {
        tail
    } else {
        tail.find('\n').map_or("", |newline| &tail[newline + 1..])
    }
}

impl<C: ServiceConnector> crate::ServiceLogSource for ServiceSession<C> {
    fn service_log(&self) -> crate::ServiceLogDto {
        use crate::{ServiceLogDto, ServiceLogFileDto, ServiceLogStatus};
        let status = |status| ServiceLogDto {
            status,
            files: Vec::new(),
        };
        match ServiceSession::service_log(self) {
            Ok(files) => ServiceLogDto {
                status: ServiceLogStatus::Included,
                files: files
                    .into_iter()
                    .filter(|file| !file.text.is_empty())
                    .map(|file| ServiceLogFileDto {
                        name: file.file.file_name().to_owned(),
                        text: file.text,
                        truncated: file.truncated,
                    })
                    .collect(),
            },
            Err(
                ServiceError::NotInstalled
                | ServiceError::Rejected(BrokerErrorCode::AuthenticationFailed),
            ) => status(ServiceLogStatus::NoService),
            Err(ServiceError::Rejected(BrokerErrorCode::InvalidRequest)) => {
                status(ServiceLogStatus::TooOld)
            }
            Err(ServiceError::Busy) => status(ServiceLogStatus::Busy),
            Err(_) => status(ServiceLogStatus::Unavailable),
        }
    }

    fn core_journal(&self, since_ms: u64) -> crate::CoreJournalDto {
        use crate::{CoreJournalDto, ServiceLogStatus};
        let status = |status| CoreJournalDto {
            status,
            ..CoreJournalDto::default()
        };
        match ServiceSession::core_journal(self, since_ms) {
            Ok((text, truncated)) => CoreJournalDto {
                status: ServiceLogStatus::Included,
                text,
                truncated,
            },
            Err(
                ServiceError::NotInstalled
                | ServiceError::Rejected(BrokerErrorCode::AuthenticationFailed),
            ) => status(ServiceLogStatus::NoService),
            Err(ServiceError::Rejected(BrokerErrorCode::InvalidRequest)) => {
                status(ServiceLogStatus::TooOld)
            }
            Err(ServiceError::Busy) => status(ServiceLogStatus::Busy),
            Err(_) => status(ServiceLogStatus::Unavailable),
        }
    }
}

fn check_state(state: ElevatedRuntimeState) -> RuntimeCheckState {
    match state {
        ElevatedRuntimeState::Stopped => RuntimeCheckState::Stopped,
        ElevatedRuntimeState::Starting => RuntimeCheckState::Starting,
        ElevatedRuntimeState::Ready => RuntimeCheckState::Ready,
        ElevatedRuntimeState::Failed => RuntimeCheckState::Failed,
        ElevatedRuntimeState::Unsupported => RuntimeCheckState::Unsupported,
    }
}

async fn blocking<C: ServiceConnector, T: Send + 'static>(
    session: &Arc<ServiceSession<C>>,
    work: impl FnOnce(&ServiceSession<C>) -> T + Send + 'static,
) -> Result<T, ServiceError> {
    let session = session.clone();
    tokio::task::spawn_blocking(move || work(&session))
        .await
        .map_err(|_| ServiceError::Disconnected)
}

/// TUN mode through the service: `start(Xray)` prepares, `start(Mihomo)` switches TUN on,
/// `stop(Mihomo)` switches it off, `stop(Xray)` leaves the cores running.
pub struct ServiceTunController<C: ServiceConnector> {
    session: Arc<ServiceSession<C>>,
    generation: u64,
    warm_up: bool,
}

impl<C: ServiceConnector> ServiceTunController<C> {
    pub fn new(session: Arc<ServiceSession<C>>, generation: u64) -> Self {
        Self {
            session,
            generation,
            warm_up: false,
        }
    }

    /// The warm-up's controller: `start(Xray)` prepares only while no connection owns the
    /// session (`InUse` otherwise) and claims nothing.
    pub fn for_warm_up(session: Arc<ServiceSession<C>>, generation: u64) -> Self {
        Self {
            session,
            generation,
            warm_up: true,
        }
    }
}

/// A Connect's `start(Xray)`: claim the session for `generation`, then prepare it.
fn claim_and_prepare<C: ServiceConnector>(
    session: &ServiceSession<C>,
    generation: u64,
) -> Result<(), ServiceError> {
    session.claim(generation);
    let result = session.prepare(generation);
    if result.is_err() {
        session.release_claim();
    }
    result
}

impl<C: ServiceConnector> ProcessController for ServiceTunController<C> {
    type Error = ServiceError;

    fn start<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        let generation = self.generation;
        let warm_up = self.warm_up;
        Box::pin(async move {
            blocking(&self.session, move |session| match engine {
                Engine::Xray if warm_up => session.prepare_idle(generation),
                Engine::Xray => claim_and_prepare(session, generation),
                Engine::Mihomo => {
                    // In this mode starting Mihomo means switching its TUN adapter on: a refusal
                    // here is an adapter problem, not a missing or crashed core.
                    session.set_tun(true).map_err(|error| match error {
                        ServiceError::Rejected(code) => ServiceError::TunRejected(code),
                        other => other,
                    })?;
                    session.set_in_use(IN_USE_TUN);
                    Ok(())
                }
            })
            .await?
        })
    }

    fn stop<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async move {
            if engine == Engine::Xray {
                if !self.warm_up {
                    self.session.release_claim();
                }
                return Ok(());
            }
            blocking(&self.session, |session| {
                let result = session.set_tun(false);
                session.set_in_use(IN_USE_NONE);
                session.release_claim();
                match result {
                    // A dropped channel already made the service stop the cores and the
                    // tunnel; with the cores gone there is no tunnel left to switch off.
                    Ok(()) | Err(ServiceError::Disconnected) => Ok(()),
                    Err(_) if !session.cores_ready() => Ok(()),
                    Err(error) => Err(error),
                }
            })
            .await?
        })
    }

    fn diagnostics<'a>(&'a self) -> Pin<Box<dyn Future<Output = ProcessDiagnostics> + Send + 'a>> {
        Box::pin(async move {
            blocking(&self.session, |session| session.diagnostics())
                .await
                .unwrap_or_default()
        })
    }

    fn start_failure_cause(&self, error: &Self::Error) -> StartFailureCause {
        error.start_failure_cause()
    }
}

/// System-proxy mode through the service: both starts only make sure the generation runs
/// with TUN off; stopping never stops the cores (the proxy switch is the daemon's job).
pub struct ServiceCoresController<C: ServiceConnector> {
    session: Arc<ServiceSession<C>>,
    generation: u64,
}

impl<C: ServiceConnector> ServiceCoresController<C> {
    pub fn new(session: Arc<ServiceSession<C>>, generation: u64) -> Self {
        Self {
            session,
            generation,
        }
    }
}

impl<C: ServiceConnector> ProcessController for ServiceCoresController<C> {
    type Error = ServiceError;

    fn start<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        let generation = self.generation;
        Box::pin(async move {
            blocking(&self.session, move |session| {
                if engine == Engine::Xray {
                    claim_and_prepare(session, generation)?;
                } else {
                    session.prepare(generation)?;
                }
                if engine == Engine::Mihomo {
                    session.set_tun(false)?;
                    session.set_in_use(IN_USE_PROXY);
                }
                Ok(())
            })
            .await?
        })
    }

    fn stop<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async move {
            if engine == Engine::Mihomo {
                self.session.set_in_use(IN_USE_NONE);
            }
            self.session.release_claim();
            Ok(())
        })
    }

    fn diagnostics<'a>(&'a self) -> Pin<Box<dyn Future<Output = ProcessDiagnostics> + Send + 'a>> {
        Box::pin(async move {
            blocking(&self.session, |session| session.diagnostics())
                .await
                .unwrap_or_default()
        })
    }

    fn start_failure_cause(&self, error: &Self::Error) -> StartFailureCause {
        error.start_failure_cause()
    }
}

/// What a warm generation was staged from: the subscription payloads and the physical
/// interface Xray is bound to. A changed default interface (Wi-Fi <-> Ethernet) must re-stage
/// before Connect; kept apart so a miss can say which part differed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WarmInputs {
    snapshot: [u8; 32],
    interface: String,
}

impl WarmInputs {
    #[must_use]
    pub fn new(snapshot: &Snapshot, outbound_interface: &str) -> Self {
        Self {
            snapshot: snapshot_digest(snapshot),
            interface: outbound_interface.to_owned(),
        }
    }

    /// The single identity of these inputs.
    #[must_use]
    pub fn key(&self) -> [u8; 32] {
        let mut digest = Sha256::new();
        for part in [self.snapshot.as_slice(), self.interface.as_bytes()] {
            digest.update((part.len() as u64).to_le_bytes());
            digest.update(part);
        }
        digest.finalize().into()
    }
}

fn snapshot_digest(snapshot: &Snapshot) -> [u8; 32] {
    let mut digest = Sha256::new();
    for part in [
        snapshot.mihomo.raw_yaml().as_bytes(),
        snapshot.xray.raw_bytes(),
    ] {
        digest.update((part.len() as u64).to_le_bytes());
        digest.update(part);
    }
    digest.finalize().into()
}

/// Identity of a warm generation for `snapshot` bound to `outbound_interface`.
#[must_use]
pub fn warm_key(snapshot: &Snapshot, outbound_interface: &str) -> [u8; 32] {
    WarmInputs::new(snapshot, outbound_interface).key()
}

/// A runtime generation staged for the service: what it was staged from, its id and the
/// loopback mixed port written into it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WarmStage {
    pub key: [u8; 32],
    pub generation: u64,
    pub mixed_port: u16,
}

/// Why a Connect or warm-up could not reuse the warm generation and staged a new one. Carries
/// no secrets: interface aliases ("Ethernet", "Wi-Fi") are the only text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WarmMiss {
    /// Nothing was staged yet (first warm-up since the daemon started).
    NothingStaged,
    /// The subscription payloads differ from the staged ones.
    SnapshotChanged,
    /// The physical default interface differs from the one Xray was bound to.
    InterfaceChanged { staged: String, now: String },
    /// The service rejected the last `Prepare` of the staged generation.
    PrepareRejected,
}

impl fmt::Display for WarmMiss {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NothingStaged => formatter.write_str("nothing_staged"),
            Self::SnapshotChanged => formatter.write_str("snapshot_changed"),
            Self::InterfaceChanged { staged, now } => {
                write!(formatter, "interface_changed staged={staged:?} now={now:?}")
            }
            Self::PrepareRejected => formatter.write_str("prepare_rejected"),
        }
    }
}

struct Staged {
    stage: WarmStage,
    inputs: WarmInputs,
}

/// The daemon's cache of the staged service generation. A generation whose `Prepare` the
/// service rejected is dropped, so the next attempt re-stages it (with a fresh mixed port)
/// instead of failing the same way forever.
#[derive(Default)]
pub struct WarmStages {
    current: Mutex<Option<Staged>>,
}

impl WarmStages {
    /// The cached stage for `inputs`, or why there is none (the service rejected its last
    /// `Prepare`, or what it was staged from differs). A miss drops the cached stage.
    pub fn reusable<C: ServiceConnector>(
        &self,
        inputs: &WarmInputs,
        session: &ServiceSession<C>,
    ) -> Result<WarmStage, WarmMiss> {
        let Ok(mut current) = self.current.lock() else {
            return Err(WarmMiss::NothingStaged);
        };
        let miss = match current.as_ref() {
            None => WarmMiss::NothingStaged,
            Some(staged) if session.prepare_failed(staged.stage.generation) => {
                WarmMiss::PrepareRejected
            }
            Some(staged) if staged.inputs.snapshot != inputs.snapshot => WarmMiss::SnapshotChanged,
            Some(staged) if staged.inputs.interface != inputs.interface => {
                WarmMiss::InterfaceChanged {
                    staged: staged.inputs.interface.clone(),
                    now: inputs.interface.clone(),
                }
            }
            Some(staged) => return Ok(staged.stage),
        };
        *current = None;
        Err(miss)
    }

    pub fn remember(&self, stage: WarmStage, inputs: WarmInputs) {
        if let Ok(mut current) = self.current.lock() {
            *current = Some(Staged { stage, inputs });
        }
    }
}

#[cfg(windows)]
pub mod windows {
    use super::*;
    use crate::elevated_controller::{
        ElevationError,
        windows::{read_message, write_raw_message},
    };
    use multicore_core::service_protocol::{
        decode_service_frame, encode_service_frame, service_response_matches,
    };
    use std::{
        ffi::OsStr,
        fs::OpenOptions,
        os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
        ptr,
    };
    use windows_sys::Win32::{
        Foundation::{ERROR_SERVICE_DOES_NOT_EXIST, GetLastError},
        Storage::FileSystem::{
            FILE_FLAG_OVERLAPPED, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
        },
        System::{
            Pipes::{
                GetNamedPipeServerProcessId, PIPE_READMODE_MESSAGE, SetNamedPipeHandleState,
                WaitNamedPipeW,
            },
            Services::{
                CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatusEx,
                SC_MANAGER_CONNECT, SC_STATUS_PROCESS_INFO, SERVICE_QUERY_STATUS, SERVICE_RUNNING,
                SERVICE_START_PENDING, SERVICE_STATUS_PROCESS,
            },
        },
    };

    pub const SERVICE_PIPE_NAME: &str = r"\\.\pipe\MultiCore.Service.v2";
    pub const SERVICE_NAME: &str = "MultiCore";
    /// Exactly what the service's pipe DACL grants INTERACTIVE users (core-host
    /// `windows_server::CLIENT_PIPE_ACCESS`): `FILE_READ_DATA | FILE_WRITE_DATA |
    /// FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES | SYNCHRONIZE`. `GENERIC_WRITE` would include
    /// `FILE_APPEND_DATA` (= create pipe instance), which the service does not grant.
    pub const SERVICE_PIPE_ACCESS: u32 = 0x0010_0183;

    /// Opens a service pipe with [`SERVICE_PIPE_ACCESS`], identification-level SQOS (the
    /// server can check but never act as this user) and message read mode.
    pub fn open_service_pipe(name: &str) -> std::io::Result<std::fs::File> {
        let pipe = OpenOptions::new()
            .access_mode(SERVICE_PIPE_ACCESS)
            .custom_flags(FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION)
            .open(name)?;
        let mode = PIPE_READMODE_MESSAGE;
        if unsafe { SetNamedPipeHandleState(pipe.as_raw_handle(), &mode, ptr::null(), ptr::null()) }
            == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(pipe)
    }

    fn wide(value: &str) -> Vec<u16> {
        OsStr::new(value).encode_wide().chain(Some(0)).collect()
    }

    fn map(error: ElevationError) -> ServiceError {
        match error {
            ElevationError::TimedOut => ServiceError::TimedOut,
            ElevationError::Protocol => ServiceError::Protocol,
            _ => ServiceError::Disconnected,
        }
    }

    pub struct PipeTransport {
        pipe: std::fs::File,
    }

    impl ServiceTransport for PipeTransport {
        fn request(&self, command: &ServiceCommand) -> Result<ServiceResponse, ServiceError> {
            let cancelled = AtomicBool::new(false);
            let frame = encode_service_frame(command).map_err(|_| ServiceError::Protocol)?;
            write_raw_message(
                self.pipe.as_raw_handle(),
                frame,
                Duration::from_secs(5),
                &cancelled,
            )
            .map_err(map)?;
            let reply = read_message(
                self.pipe.as_raw_handle(),
                service_response_timeout(command),
                &cancelled,
            )
            .map_err(map)?;
            let response: ServiceResponse =
                decode_service_frame(&reply).map_err(|_| ServiceError::Protocol)?;
            if !service_response_matches(command, &response) {
                return Err(ServiceError::Protocol);
            }
            Ok(response)
        }
    }

    /// How long a Connect or probe waits for a service that is still starting (logon, or
    /// right after a self-update restart) before falling back.
    const START_WAIT: Duration = Duration::from_secs(20);
    const START_POLL: Duration = Duration::from_millis(500);

    /// PID of the running `MultiCore` service as the SCM knows it, waiting up to
    /// [`START_WAIT`] while the SCM reports it as starting.
    pub fn registered_service_pid() -> Result<u32, ServiceError> {
        let deadline = std::time::Instant::now() + START_WAIT;
        loop {
            match query_service() {
                Err(ServiceError::Unavailable) if std::time::Instant::now() < deadline => {
                    // START_PENDING (or a transient SCM failure): the service is on its way.
                }
                Err(ServiceError::Rejected(_)) => return Err(ServiceError::Unavailable),
                result => return result,
            }
            std::thread::sleep(START_POLL);
        }
    }

    /// One SCM query: the PID when running, `Unavailable` while starting, `Rejected` when
    /// stopped or stopping (no point in waiting).
    fn query_service() -> Result<u32, ServiceError> {
        let manager = unsafe { OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_CONNECT) };
        if manager.is_null() {
            return Err(ServiceError::Unavailable);
        }
        let name = wide(SERVICE_NAME);
        let service = unsafe { OpenServiceW(manager, name.as_ptr(), SERVICE_QUERY_STATUS) };
        if service.is_null() {
            let error = unsafe { GetLastError() };
            unsafe { CloseServiceHandle(manager) };
            return Err(if error == ERROR_SERVICE_DOES_NOT_EXIST {
                ServiceError::NotInstalled
            } else {
                ServiceError::Unavailable
            });
        }
        let mut status: SERVICE_STATUS_PROCESS = unsafe { std::mem::zeroed() };
        let mut needed = 0;
        let queried = unsafe {
            QueryServiceStatusEx(
                service,
                SC_STATUS_PROCESS_INFO,
                (&mut status as *mut SERVICE_STATUS_PROCESS).cast(),
                size_of::<SERVICE_STATUS_PROCESS>() as u32,
                &mut needed,
            )
        };
        unsafe {
            CloseServiceHandle(service);
            CloseServiceHandle(manager);
        }
        if queried == 0 {
            return Err(ServiceError::Unavailable);
        }
        match status.dwCurrentState {
            SERVICE_RUNNING if status.dwProcessId != 0 => Ok(status.dwProcessId),
            SERVICE_START_PENDING | SERVICE_RUNNING => Err(ServiceError::Unavailable),
            _ => Err(ServiceError::Rejected(BrokerErrorCode::NotReady)),
        }
    }

    #[derive(Clone, Copy, Debug, Default)]
    pub struct WindowsServiceConnector;

    impl ServiceConnector for WindowsServiceConnector {
        type Transport = PipeTransport;

        fn connect(&self) -> Result<PipeTransport, ServiceError> {
            let service_pid = registered_service_pid()?;
            let name = wide(SERVICE_PIPE_NAME);
            // A service that just reached RUNNING may not have created its pipe yet, and
            // `WaitNamedPipeW` fails at once for a pipe that does not exist.
            let deadline = std::time::Instant::now() + START_WAIT;
            while unsafe { WaitNamedPipeW(name.as_ptr(), 2_000) } == 0 {
                if std::time::Instant::now() >= deadline {
                    return Err(ServiceError::Unavailable);
                }
                std::thread::sleep(START_POLL);
            }
            let pipe =
                open_service_pipe(SERVICE_PIPE_NAME).map_err(|_| ServiceError::Unavailable)?;
            let mut server_pid = 0;
            if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut server_pid) } == 0
                || server_pid != service_pid
            {
                return Err(ServiceError::Impostor);
            }
            Ok(PipeTransport { pipe })
        }
    }
}

#[cfg(all(test, windows))]
mod windows_pipe_access_tests {
    use super::windows::{SERVICE_PIPE_ACCESS, open_service_pipe};
    use std::{ffi::OsStr, os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, LocalFree},
        Security::{
            Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SECURITY_ATTRIBUTES,
        },
        Storage::FileSystem::PIPE_ACCESS_DUPLEX,
        System::Pipes::{
            CreateNamedPipeW, PIPE_READMODE_MESSAGE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE,
            PIPE_WAIT,
        },
    };

    fn wide(value: &str) -> Vec<u16> {
        OsStr::new(value).encode_wide().chain(Some(0)).collect()
    }

    /// A pipe whose DACL grants everyone (standing in for INTERACTIVE) exactly the service's
    /// client rights.
    fn restricted_pipe(name: &str) -> HANDLE {
        let sddl = wide(&format!(
            "D:P(A;;GA;;;SY)(A;;{SERVICE_PIPE_ACCESS:#010x};;;WD)"
        ));
        let mut descriptor = ptr::null_mut();
        assert_ne!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    1,
                    &mut descriptor,
                    ptr::null_mut(),
                )
            },
            0
        );
        let security = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let name = wide(name);
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                2,
                4096,
                4096,
                0,
                &security,
            )
        };
        unsafe { LocalFree(descriptor.cast()) };
        assert_ne!(handle, INVALID_HANDLE_VALUE);
        handle
    }

    #[test]
    fn the_daemon_opens_the_service_pipe_with_only_the_granted_rights() {
        assert_eq!(SERVICE_PIPE_ACCESS, 0x0010_0183);
        assert_eq!(SERVICE_PIPE_ACCESS & 0x4, 0, "no FILE_CREATE_PIPE_INSTANCE");
        let name = format!(
            r"\\.\pipe\MultiCore.Service.daemon-test.{}",
            std::process::id()
        );
        let server = restricted_pipe(&name);
        let generic = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&name)
            .unwrap_err();
        assert_eq!(generic.raw_os_error(), Some(5), "{generic}");
        let pipe = open_service_pipe(&name).expect("open with the service client rights");
        drop(pipe);
        unsafe { CloseHandle(server) };
    }
}

#[cfg(unix)]
pub mod unix {
    use super::*;
    use multicore_core::{
        elevation_protocol::read_stream_frame,
        service_protocol::{decode_service_frame, service_response_matches, write_service_frame},
    };
    use std::{os::unix::net::UnixStream, path::Path};

    pub const SOCKET_PATH: &str = "/run/multicore/service.sock";
    const UNIT_PATHS: [&str; 2] = [
        "/usr/lib/systemd/system/multicore.service",
        "/lib/systemd/system/multicore.service",
    ];

    pub struct SocketTransport {
        stream: UnixStream,
    }

    impl SocketTransport {
        #[cfg(test)]
        pub(crate) fn for_test(stream: UnixStream) -> Self {
            Self { stream }
        }
    }

    impl ServiceTransport for SocketTransport {
        fn request(&self, command: &ServiceCommand) -> Result<ServiceResponse, ServiceError> {
            let mut stream = &self.stream;
            self.stream
                .set_read_timeout(Some(service_response_timeout(command)))
                .map_err(|_| ServiceError::Disconnected)?;
            write_service_frame(&mut stream, command).map_err(|_| ServiceError::Disconnected)?;
            let frame = read_stream_frame(&mut stream).map_err(|error| {
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) {
                    ServiceError::TimedOut
                } else {
                    ServiceError::Disconnected
                }
            })?;
            let response: ServiceResponse =
                decode_service_frame(&frame).map_err(|_| ServiceError::Protocol)?;
            if !service_response_matches(command, &response) {
                return Err(ServiceError::Protocol);
            }
            Ok(response)
        }
    }

    #[derive(Clone, Copy, Debug, Default)]
    pub struct UnixServiceConnector;

    impl ServiceConnector for UnixServiceConnector {
        type Transport = SocketTransport;

        fn connect(&self) -> Result<SocketTransport, ServiceError> {
            if !Path::new(SOCKET_PATH).exists() {
                return Err(if UNIT_PATHS.iter().any(|unit| Path::new(unit).exists()) {
                    ServiceError::Unavailable
                } else {
                    ServiceError::NotInstalled
                });
            }
            let stream = UnixStream::connect(SOCKET_PATH).map_err(|_| ServiceError::Unavailable)?;
            let peer = multicore_platform::peer::peer_credentials(&stream)
                .map_err(|_| ServiceError::Unavailable)?;
            if peer.uid != 0 {
                return Err(ServiceError::Impostor);
            }
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .map_err(|_| ServiceError::Unavailable)?;
            Ok(SocketTransport { stream })
        }
    }
}

#[cfg(windows)]
pub type ProductionServiceConnector = windows::WindowsServiceConnector;
#[cfg(unix)]
pub type ProductionServiceConnector = unix::UnixServiceConnector;

#[cfg(all(test, unix))]
mod unix_tests {
    use super::{ServiceTransport, unix::SocketTransport};
    use multicore_core::{
        elevation_protocol::{ElevatedRuntimeState, read_stream_frame},
        service_protocol::{
            ServiceCommand, ServiceResponse, decode_service_command, write_service_frame,
        },
    };
    use std::os::unix::net::UnixStream;

    #[test]
    fn the_socket_transport_frames_a_status_exchange() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let service = std::thread::spawn(move || {
            let frame = read_stream_frame(&mut server).unwrap();
            assert_eq!(
                decode_service_command(&frame).unwrap(),
                ServiceCommand::Status
            );
            write_service_frame(
                &mut server,
                &ServiceResponse::Status {
                    generation_id: Some(7),
                    xray: ElevatedRuntimeState::Ready,
                    mihomo: ElevatedRuntimeState::Ready,
                    tun_on: true,
                    failure: None,
                },
            )
            .unwrap();
        });
        let transport = SocketTransport::for_test(client);
        assert!(matches!(
            transport.request(&ServiceCommand::Status),
            Ok(ServiceResponse::Status {
                generation_id: Some(7),
                tun_on: true,
                ..
            })
        ));
        service.join().unwrap();
    }
}
