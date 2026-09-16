mod event;
mod fetch;
mod input;
mod network;
mod sidecar;
mod snapshot;
mod state;
mod supervisor;

pub use event::{Component, Event, Severity};
pub use fetch::{
    FetchError, HttpClient, HttpResponse, ReqwestHttpClient, SubscriptionFetcher, UA_MIHOMO,
    UA_NATIVE, UA_XRAY,
};
pub use input::{
    ConfigError, MAX_CONFIG_BYTES, MihomoConfig, MihomoSocksMapping, ProxyGroup, XrayConfig,
    parse_mihomo, parse_xray,
};
pub use network::default_ipv4_interface;
pub use sidecar::{
    CommandSpec, CoreLogBuffer, CoreLogRecord, DiagnosticStream, ManagedChild, ProcessDiagnostics,
    ProcessError, ProcessLauncher, RuntimeCheckState, RuntimePaths, SidecarProcessController,
    TokioManagedChild, TokioProcessLauncher,
};
pub use snapshot::{
    AtomicSnapshot, MihomoRuntimeControl, PersistenceError, PersistentSnapshotStore,
    PublishedRuntime, Snapshot, SnapshotSink, StagedRuntime, SubscriptionInfo, publish_runtime,
    publish_runtime_with_mihomo_control, stage_runtime_with_mihomo_control,
    xray_outbound_server_domains,
};
pub use state::{ConnectionState, StateError, StateMachine};
pub use supervisor::{Engine, ProcessController, Supervisor, SupervisorError};
