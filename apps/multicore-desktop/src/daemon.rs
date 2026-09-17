use std::{
    error::Error,
    fmt,
    io::Read,
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

const STATUS_RESPONSE_LIMIT_BYTES: usize = 64 * 1024;
const EVENTS_RESPONSE_LIMIT_BYTES: usize = 2 * 1024 * 1024;
const DIAGNOSTICS_RESPONSE_LIMIT_BYTES: usize = 2 * 1024 * 1024;
const CATALOG_RESPONSE_LIMIT_BYTES: usize = 32 * 1024 * 1024;
const LATENCIES_RESPONSE_LIMIT_BYTES: usize = 32 * 1024 * 1024;
const API_ERROR_RESPONSE_LIMIT_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct DaemonStatus {
    #[serde(default)]
    pub state: String,
    #[serde(default, alias = "profile_name")]
    pub profile: Option<String>,
    #[serde(default, alias = "node")]
    pub current_node: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub degraded: bool,
    #[serde(default)]
    pub subscription: Option<DaemonSubscriptionInfo>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct DaemonSubscriptionInfo {
    #[serde(default)]
    pub source_name: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub uploaded_bytes: Option<u64>,
    #[serde(default)]
    pub downloaded_bytes: Option<u64>,
    #[serde(default)]
    pub used_bytes: Option<u64>,
    #[serde(default)]
    pub total_bytes: Option<u64>,
    #[serde(default)]
    pub expires_at_unix: Option<u64>,
    #[serde(default)]
    pub updated_at_unix: u64,
    #[serde(default)]
    pub refresh_interval_secs: Option<u64>,
    #[serde(default)]
    pub announcement_text: Option<String>,
    #[serde(default)]
    pub announcement_action_label: Option<String>,
    #[serde(default)]
    pub announcement_tone: Option<String>,
    #[serde(default)]
    pub home_available: bool,
    #[serde(default)]
    pub support_available: bool,
    #[serde(default)]
    pub announcement_action_available: bool,
    #[serde(default)]
    pub service_logo_path: Option<String>,
    #[serde(default)]
    pub refresh_available: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct DaemonEvent {
    pub id: u64,
    #[serde(default)]
    pub timestamp: String,
    #[serde(default)]
    pub level: String,
    #[serde(default, alias = "summary")]
    pub message: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct DaemonEventBatch {
    pub epoch: String,
    pub events: Vec<DaemonEvent>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct DaemonDiagnosticLog {
    pub id: u64,
    #[serde(default)]
    pub timestamp: String,
    #[serde(default)]
    pub component: String,
    #[serde(default)]
    pub level: String,
    #[serde(default)]
    pub message: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct DaemonProxyMapping {
    #[serde(default)]
    pub proxy_name: String,
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub xray_label: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct DaemonDiagnostics {
    #[serde(default)]
    pub xray: String,
    #[serde(default)]
    pub mihomo: String,
    #[serde(default)]
    pub tun: String,
    #[serde(default)]
    pub logs: Vec<DaemonDiagnosticLog>,
    #[serde(default)]
    pub mappings: Vec<DaemonProxyMapping>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct DaemonCatalog {
    pub revision: u64,
    pub groups: Vec<DaemonCatalogGroup>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct DaemonCatalogGroup {
    pub id: String,
    pub label: String,
    pub selected: bool,
    pub nodes: Vec<DaemonCatalogNode>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct DaemonCatalogNode {
    pub id: String,
    pub label: String,
    pub selected: bool,
    pub delay_ms: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct DaemonLatencies {
    #[serde(default)]
    pub entries: Vec<DaemonLatencyEntry>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct DaemonLatencyEntry {
    #[serde(default)]
    pub group_id: String,
    #[serde(default)]
    pub node_id: String,
    #[serde(default)]
    pub latency_ms: Option<u64>,
    #[serde(default)]
    pub status: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DaemonErrorKind {
    Other,
    StaleRevision,
    DefinitiveHttp,
    Ambiguous,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonError {
    message: String,
    kind: DaemonErrorKind,
}

impl DaemonError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: DaemonErrorKind::Other,
        }
    }

    pub(crate) fn stale_revision() -> Self {
        Self {
            message: "Каталог маршрутов изменился.".into(),
            kind: DaemonErrorKind::StaleRevision,
        }
    }

    pub(crate) fn is_stale_revision(&self) -> bool {
        self.kind == DaemonErrorKind::StaleRevision
    }

    /// Whether a successful selection may have reached the daemon even though the client could
    /// not safely decode its response. Callers should refresh state before making another choice.
    pub(crate) fn requires_reconciliation(&self) -> bool {
        self.kind == DaemonErrorKind::Ambiguous
    }

    fn definitive_http(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: DaemonErrorKind::DefinitiveHttp,
        }
    }

    pub(crate) fn ambiguous(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: DaemonErrorKind::Ambiguous,
        }
    }
}

impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for DaemonError {}

pub struct UnavailableDaemonClient {
    message: String,
}

impl UnavailableDaemonClient {
    pub fn new(error: DaemonError) -> Self {
        Self {
            message: error.to_string(),
        }
    }

    fn unavailable<T>(&self) -> Result<T, DaemonError> {
        Err(DaemonError::new(self.message.clone()))
    }
}

pub trait DaemonClient: Send + Sync {
    /// GET /v1/status
    fn status(&self) -> Result<DaemonStatus, DaemonError>;

    /// POST /v1/subscriptions/import with `{ "url": "…" }`.
    fn import_subscription(&self, url: &str) -> Result<DaemonStatus, DaemonError>;

    /// POST /v1/subscriptions/refresh. The saved source URL never leaves the daemon.
    fn refresh_subscription(&self) -> Result<DaemonStatus, DaemonError> {
        Err(DaemonError::new("Обновление подписки недоступно."))
    }

    /// POST /v1/connect. Group and node choices remain daemon DTO values; the UI
    /// never interprets Xray JSON or constructs proxy mappings.
    fn connect(&self) -> Result<DaemonStatus, DaemonError>;

    /// POST /v1/disconnect.
    fn disconnect(&self) -> Result<DaemonStatus, DaemonError>;

    /// GET /v1/events?after=<u64>[&epoch=<known-epoch>].
    fn events(
        &self,
        after: u64,
        known_epoch: Option<&str>,
    ) -> Result<DaemonEventBatch, DaemonError>;

    /// GET /v1/diagnostics. Contains only bounded, redacted runtime data.
    fn diagnostics(&self) -> Result<DaemonDiagnostics, DaemonError> {
        Err(DaemonError::new("Диагностика недоступна."))
    }

    /// GET /v1/catalog. IDs are opaque daemon values; labels are presentation data.
    fn catalog(&self) -> Result<DaemonCatalog, DaemonError>;

    /// POST /v1/latencies. Entries contain only opaque catalog IDs and safe probe outcomes.
    fn latencies(&self, _revision: Option<u64>) -> Result<DaemonLatencies, DaemonError> {
        Err(DaemonError::new("Проверка задержки недоступна."))
    }

    /// PUT /v1/selections/{group_id} with `{ "revision", "node_id" }`.
    fn select_node(
        &self,
        group_id: &str,
        revision: u64,
        node_id: &str,
    ) -> Result<DaemonCatalog, DaemonError>;
}

impl DaemonClient for UnavailableDaemonClient {
    fn status(&self) -> Result<DaemonStatus, DaemonError> {
        self.unavailable()
    }

    fn import_subscription(&self, _url: &str) -> Result<DaemonStatus, DaemonError> {
        self.unavailable()
    }

    fn refresh_subscription(&self) -> Result<DaemonStatus, DaemonError> {
        self.unavailable()
    }

    fn connect(&self) -> Result<DaemonStatus, DaemonError> {
        self.unavailable()
    }

    fn disconnect(&self) -> Result<DaemonStatus, DaemonError> {
        self.unavailable()
    }

    fn events(
        &self,
        _after: u64,
        _known_epoch: Option<&str>,
    ) -> Result<DaemonEventBatch, DaemonError> {
        self.unavailable()
    }

    fn catalog(&self) -> Result<DaemonCatalog, DaemonError> {
        self.unavailable()
    }

    fn latencies(&self, _revision: Option<u64>) -> Result<DaemonLatencies, DaemonError> {
        self.unavailable()
    }

    fn select_node(
        &self,
        _group_id: &str,
        _revision: u64,
        _node_id: &str,
    ) -> Result<DaemonCatalog, DaemonError> {
        self.unavailable()
    }
}

#[derive(Clone)]
pub struct HttpDaemonClient {
    base_url: reqwest::Url,
    token: String,
    http: Client,
}

impl HttpDaemonClient {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Result<Self, DaemonError> {
        let token = token.into();
        if token.trim().is_empty() {
            return Err(DaemonError::new("Daemon access token is not configured."));
        }
        let base_url = parse_daemon_base_url(&base_url.into())?;
        let http = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(map_error)?;
        Ok(Self {
            base_url,
            token,
            http,
        })
    }

    fn endpoint(&self, path: &str) -> reqwest::Url {
        let mut endpoint = self.base_url.clone();
        endpoint.set_path(path);
        endpoint.set_query(None);
        endpoint
    }

    fn send_json<T: DeserializeOwned>(
        &self,
        request: reqwest::blocking::RequestBuilder,
        response_limit: usize,
    ) -> Result<T, DaemonError> {
        let response = request.bearer_auth(&self.token).send().map_err(map_error)?;
        if !response.status().is_success() {
            return Err(DaemonError::definitive_http(
                "Фоновый сервис отклонил запрос.",
            ));
        }
        decode_bounded_json(response, response_limit)
    }

    fn parse_status(
        &self,
        request: reqwest::blocking::RequestBuilder,
    ) -> Result<DaemonStatus, DaemonError> {
        self.send_json(request, STATUS_RESPONSE_LIMIT_BYTES)
    }
}

fn parse_daemon_base_url(value: &str) -> Result<reqwest::Url, DaemonError> {
    let invalid = || DaemonError::new("Некорректный адрес фонового сервиса.");
    let url = reqwest::Url::parse(value.trim()).map_err(|_| invalid())?;
    if url.scheme() != "http"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(invalid());
    }
    let host = url.host_str().ok_or_else(invalid)?;
    let address_text = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    let ip = address_text.parse::<IpAddr>().map_err(|_| invalid())?;
    if !ip.is_loopback() {
        return Err(invalid());
    }
    let port = url.port_or_known_default().ok_or_else(invalid)?;
    let _address = SocketAddr::new(ip, port);
    Ok(url)
}

fn decode_bounded_json<T: DeserializeOwned>(
    response: reqwest::blocking::Response,
    response_limit: usize,
) -> Result<T, DaemonError> {
    if response
        .content_length()
        .is_some_and(|length| length > response_limit as u64)
    {
        return Err(response_too_large());
    }

    let mut bytes = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or_default()
            .min(response_limit as u64) as usize,
    );
    response
        .take(response_limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| DaemonError::ambiguous("Не удалось прочитать ответ фонового сервиса."))?;
    if bytes.len() > response_limit {
        return Err(response_too_large());
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| DaemonError::ambiguous("Фоновый сервис вернул некорректный ответ."))
}

fn response_too_large() -> DaemonError {
    DaemonError::ambiguous("Ответ фонового сервиса превышает допустимый размер.")
}

#[derive(Serialize)]
struct ImportRequest<'a> {
    url: &'a str,
}

#[derive(Serialize)]
struct SelectionRequest<'a> {
    revision: u64,
    node_id: &'a str,
}

#[derive(Serialize)]
struct LatencyRequest {
    revision: Option<u64>,
}

#[derive(Deserialize)]
struct ApiErrorBody {
    #[serde(default)]
    code: String,
}

impl DaemonClient for HttpDaemonClient {
    fn status(&self) -> Result<DaemonStatus, DaemonError> {
        self.parse_status(self.http.get(self.endpoint("/v1/status")))
    }

    fn import_subscription(&self, url: &str) -> Result<DaemonStatus, DaemonError> {
        self.parse_status(
            self.http
                .post(self.endpoint("/v1/subscriptions/import"))
                .json(&ImportRequest { url }),
        )
    }

    fn refresh_subscription(&self) -> Result<DaemonStatus, DaemonError> {
        self.parse_status(self.http.post(self.endpoint("/v1/subscriptions/refresh")))
    }

    fn connect(&self) -> Result<DaemonStatus, DaemonError> {
        self.parse_status(self.http.post(self.endpoint("/v1/connect")))
    }

    fn disconnect(&self) -> Result<DaemonStatus, DaemonError> {
        self.parse_status(self.http.post(self.endpoint("/v1/disconnect")))
    }

    fn events(
        &self,
        after: u64,
        known_epoch: Option<&str>,
    ) -> Result<DaemonEventBatch, DaemonError> {
        let mut request = self
            .http
            .get(self.endpoint("/v1/events"))
            .bearer_auth(&self.token)
            .query(&[("after", after)]);
        if let Some(epoch) = known_epoch {
            request = request.query(&[("epoch", epoch)]);
        }
        self.send_json(request, EVENTS_RESPONSE_LIMIT_BYTES)
    }

    fn diagnostics(&self) -> Result<DaemonDiagnostics, DaemonError> {
        self.send_json(
            self.http.get(self.endpoint("/v1/diagnostics")),
            DIAGNOSTICS_RESPONSE_LIMIT_BYTES,
        )
    }

    fn catalog(&self) -> Result<DaemonCatalog, DaemonError> {
        self.send_json(
            self.http.get(self.endpoint("/v1/catalog")),
            CATALOG_RESPONSE_LIMIT_BYTES,
        )
    }

    fn latencies(&self, revision: Option<u64>) -> Result<DaemonLatencies, DaemonError> {
        let response = self
            .http
            .post(self.endpoint("/v1/latencies"))
            .bearer_auth(&self.token)
            .json(&LatencyRequest { revision })
            .send()
            .map_err(map_error)?;
        if response.status() == reqwest::StatusCode::CONFLICT {
            let stale_revision =
                decode_bounded_json::<ApiErrorBody>(response, API_ERROR_RESPONSE_LIMIT_BYTES)
                    .is_ok_and(|body| body.code == "stale_revision");
            return Err(if stale_revision {
                DaemonError::stale_revision()
            } else {
                DaemonError::definitive_http("Не удалось проверить задержку.")
            });
        }
        if !response.status().is_success() {
            return Err(DaemonError::definitive_http(
                "Фоновый сервис отклонил проверку задержки.",
            ));
        }
        decode_bounded_json(response, LATENCIES_RESPONSE_LIMIT_BYTES)
    }

    fn select_node(
        &self,
        group_id: &str,
        revision: u64,
        node_id: &str,
    ) -> Result<DaemonCatalog, DaemonError> {
        let mut endpoint = self.endpoint("/v1/selections/");
        endpoint
            .path_segments_mut()
            .map_err(|()| DaemonError::new("Некорректный адрес фонового сервиса."))?
            .pop_if_empty()
            .push(group_id);
        let response = self
            .http
            .put(endpoint)
            .bearer_auth(&self.token)
            .json(&SelectionRequest { revision, node_id })
            .send()
            .map_err(map_error)?;
        if response.status() == reqwest::StatusCode::CONFLICT {
            let stale_revision =
                decode_bounded_json::<ApiErrorBody>(response, API_ERROR_RESPONSE_LIMIT_BYTES)
                    .is_ok_and(|body| body.code == "stale_revision");
            return Err(if stale_revision {
                DaemonError::stale_revision()
            } else {
                DaemonError::definitive_http("Не удалось изменить маршрут.")
            });
        }
        if !response.status().is_success() {
            return Err(DaemonError::definitive_http(
                "Фоновый сервис отклонил изменение маршрута.",
            ));
        }
        decode_bounded_json(response, CATALOG_RESPONSE_LIMIT_BYTES)
    }
}

fn map_error(error: reqwest::Error) -> DaemonError {
    if error.is_timeout() {
        DaemonError::ambiguous("Фоновый сервис не ответил вовремя.")
    } else if error.is_connect() {
        DaemonError::ambiguous("Фоновый сервис не запущен.")
    } else {
        DaemonError::ambiguous("Не удалось выполнить запрос к фоновому сервису.")
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Mutex;

    use super::*;

    pub struct MockDaemonClient {
        pub calls: Mutex<Vec<String>>,
        pub status: DaemonStatus,
        pub events: Vec<DaemonEvent>,
        pub event_epoch: String,
        pub catalog: DaemonCatalog,
        pub fail_import: bool,
        pub fail_events: bool,
        pub fail_catalog: bool,
        pub latencies: DaemonLatencies,
        pub fail_latencies: bool,
    }

    impl MockDaemonClient {
        pub fn ready() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                status: DaemonStatus {
                    state: "ready".into(),
                    profile: Some("Тестовый".into()),
                    current_node: Some("Авто".into()),
                    ..DaemonStatus::default()
                },
                events: Vec::new(),
                event_epoch: "test-epoch".into(),
                catalog: DaemonCatalog::default(),
                fail_import: false,
                fail_events: false,
                fail_catalog: false,
                latencies: DaemonLatencies::default(),
                fail_latencies: false,
            }
        }

        fn record(&self, call: impl Into<String>) {
            self.calls.lock().expect("mock call lock").push(call.into());
        }
    }

    impl DaemonClient for MockDaemonClient {
        fn status(&self) -> Result<DaemonStatus, DaemonError> {
            self.record("GET /v1/status");
            Ok(self.status.clone())
        }

        fn import_subscription(&self, url: &str) -> Result<DaemonStatus, DaemonError> {
            self.record(format!("POST /v1/subscriptions/import {url}"));
            if self.fail_import {
                Err(DaemonError::new(
                    "unsafe upstream token=secret url=https://private.invalid",
                ))
            } else {
                Ok(self.status.clone())
            }
        }

        fn refresh_subscription(&self) -> Result<DaemonStatus, DaemonError> {
            self.record("POST /v1/subscriptions/refresh");
            Ok(self.status.clone())
        }

        fn connect(&self) -> Result<DaemonStatus, DaemonError> {
            self.record("POST /v1/connect");
            Ok(self.status.clone())
        }

        fn disconnect(&self) -> Result<DaemonStatus, DaemonError> {
            self.record("POST /v1/disconnect");
            Ok(self.status.clone())
        }

        fn events(
            &self,
            after: u64,
            known_epoch: Option<&str>,
        ) -> Result<DaemonEventBatch, DaemonError> {
            let epoch = known_epoch
                .map(|epoch| format!(" epoch={epoch}"))
                .unwrap_or_default();
            self.record(format!("GET /v1/events after={after}{epoch}"));
            if self.fail_events {
                Err(DaemonError::new("unsafe upstream details"))
            } else {
                Ok(DaemonEventBatch {
                    epoch: self.event_epoch.clone(),
                    events: self.events.clone(),
                })
            }
        }

        fn catalog(&self) -> Result<DaemonCatalog, DaemonError> {
            self.record("GET /v1/catalog");
            if self.fail_catalog {
                Err(DaemonError::new("unsafe upstream catalog details"))
            } else {
                Ok(self.catalog.clone())
            }
        }

        fn latencies(&self, _revision: Option<u64>) -> Result<DaemonLatencies, DaemonError> {
            self.record("POST /v1/latencies");
            if self.fail_latencies {
                Err(DaemonError::new("unsafe upstream latency details"))
            } else {
                Ok(self.latencies.clone())
            }
        }

        fn select_node(
            &self,
            _group_id: &str,
            _revision: u64,
            _node_id: &str,
        ) -> Result<DaemonCatalog, DaemonError> {
            self.record("PUT /v1/selections/{opaque}");
            Ok(self.catalog.clone())
        }
    }
}

#[cfg(test)]
mod http_tests {
    use std::{
        ffi::OsString,
        io::{Read, Write},
        net::TcpListener,
        process::Command,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };

    use super::*;

    #[test]
    fn subscription_info_deserializes_legacy_payload_with_safe_defaults() {
        let info: DaemonSubscriptionInfo = serde_json::from_value(serde_json::json!({
            "source_name": "legacy.example",
            "downloaded_bytes": 7,
            "total_bytes": 10,
            "expires_at_unix": null,
            "updated_at_unix": 123,
            "refresh_available": true
        }))
        .expect("legacy subscription DTO");

        assert_eq!(info.display_name, "");
        assert_eq!(info.uploaded_bytes, None);
        assert_eq!(info.used_bytes, None);
        assert_eq!(info.refresh_interval_secs, None);
        assert_eq!(info.announcement_text, None);
        assert_eq!(info.announcement_action_label, None);
        assert_eq!(info.announcement_tone, None);
        assert!(!info.home_available);
        assert!(!info.support_available);
        assert!(!info.announcement_action_available);
        assert_eq!(info.service_logo_path, None);
        assert!(info.refresh_available);
    }

    #[test]
    fn subscription_info_deserializes_safe_metadata_without_any_url_field() {
        let json = serde_json::json!({
            "source_name": "fallback.example",
            "display_name": "Provider",
            "uploaded_bytes": 4,
            "downloaded_bytes": 8,
            "used_bytes": 12,
            "total_bytes": 100,
            "expires_at_unix": 456,
            "updated_at_unix": 123,
            "refresh_interval_secs": 900,
            "announcement_text": "Maintenance",
            "announcement_action_label": "Details",
            "announcement_tone": "blue",
            "home_available": true,
            "support_available": true,
            "announcement_action_available": true,
            "service_logo_path": "C:\\\\cache\\\\provider.png",
            "refresh_available": true
        });
        let info: DaemonSubscriptionInfo =
            serde_json::from_value(json.clone()).expect("current subscription DTO");

        assert_eq!(info.display_name, "Provider");
        assert_eq!(info.used_bytes, Some(12));
        assert_eq!(info.announcement_tone.as_deref(), Some("blue"));
        assert!(info.home_available);
        assert!(info.support_available);
        assert!(info.announcement_action_available);
        assert!(info.service_logo_path.is_some());
        assert!(json.get("url").is_none());
    }

    fn start_server(request_count: usize) -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test HTTP server");
        let address = listener.local_addr().expect("test server address");
        let handle = thread::spawn(move || {
            let mut requests = Vec::with_capacity(request_count);
            for _ in 0..request_count {
                let (mut stream, _) = listener.accept().expect("accept test request");
                let mut bytes = Vec::new();
                let mut buffer = [0_u8; 1024];
                loop {
                    let read = stream.read(&mut buffer).expect("read test request");
                    if read == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buffer[..read]);
                    if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&bytes).into_owned();
                let is_events = request.starts_with("GET /v1/events?");
                let is_catalog = request.starts_with("GET /v1/catalog ");
                let is_latencies = request.starts_with("POST /v1/latencies ");
                let is_diagnostics = request.starts_with("GET /v1/diagnostics ");
                requests.push(request);

                let body = if is_latencies {
                    r#"{"entries":[{"group_id":"group:auto","node_id":"node:a","latency_ms":42,"status":"ok"},{"group_id":"group:auto","node_id":"node:b","latency_ms":null,"status":"timeout"}]}"#
                } else if is_diagnostics {
                    r#"{"xray":"ready","mihomo":"ready","tun":"ready","logs":[{"id":7,"timestamp":"11","component":"mihomo","level":"error","message":"safe core line"}],"mappings":[{"proxy_name":"Sweden [se]","address":"127.0.0.1:31006","xray_label":"Sweden [se]"}]}"#
                } else if is_events {
                    r#"{"epoch":"boot-a","events":[{"id":43,"timestamp":"1","level":"info","message":"Подключение установлено."}]}"#
                } else if is_catalog {
                    r#"{"revision":17,"groups":[{"id":"group:auto","label":"Авто","selected":true,"nodes":[{"id":"node:a","label":"Север","selected":true,"delay_ms":42},{"id":"node:b","label":"Резерв","selected":false}]}]}"#
                } else {
                    r#"{"state":"ready","profile":"Test","current_node":"Auto","message":null,"degraded":false}"#
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .expect("write test response");
            }
            requests
        });
        (format!("http://{address}"), handle)
    }

    fn start_single_response_server(body: &'static str) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test HTTP server");
        let address = listener.local_addr().expect("test server address");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept test request");
            let mut buffer = [0_u8; 2048];
            let _ = stream.read(&mut buffer).expect("read test request");
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("write test response");
        });
        (format!("http://{address}"), handle)
    }

    fn start_selection_server(
        status: &'static str,
        body: &'static str,
    ) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind selection HTTP server");
        let address = listener.local_addr().expect("selection server address");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept selection request");
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 1024];
            let mut expected_len = None;
            loop {
                let read = stream.read(&mut buffer).expect("read selection request");
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..read]);
                if expected_len.is_none()
                    && let Some(headers_end) = bytes.windows(4).position(|part| part == b"\r\n\r\n")
                {
                    let headers = String::from_utf8_lossy(&bytes[..headers_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(str::to_owned)
                        })
                        .and_then(|value| value.parse::<usize>().ok())
                        .unwrap_or_default();
                    expected_len = Some(headers_end + 4 + content_length);
                }
                if expected_len.is_some_and(|length| bytes.len() >= length) {
                    break;
                }
            }
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("write selection response");
            String::from_utf8(bytes).expect("selection request is HTTP text")
        });
        (format!("http://{address}"), handle)
    }

    fn start_observing_server(
        bind_address: &str,
        response: Vec<u8>,
        stop: Arc<AtomicBool>,
    ) -> (String, thread::JoinHandle<Option<String>>) {
        let listener = TcpListener::bind(bind_address).expect("bind observing HTTP server");
        listener
            .set_nonblocking(true)
            .expect("make observing server nonblocking");
        let address = listener.local_addr().expect("observing server address");
        let handle = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_nonblocking(false)
                            .expect("make observed stream blocking");
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .expect("bound observed request read");
                        stream
                            .set_write_timeout(Some(Duration::from_secs(2)))
                            .expect("bound observing response write");
                        let mut bytes = Vec::new();
                        let mut buffer = [0_u8; 2048];
                        loop {
                            match stream.read(&mut buffer) {
                                Ok(0) => break,
                                Ok(read) => {
                                    bytes.extend_from_slice(&buffer[..read]);
                                    if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                                        break;
                                    }
                                }
                                Err(error)
                                    if matches!(
                                        error.kind(),
                                        std::io::ErrorKind::WouldBlock
                                            | std::io::ErrorKind::TimedOut
                                    ) =>
                                {
                                    break;
                                }
                                Err(_) => break,
                            }
                        }
                        let _ = stream.write_all(&response);
                        return Some(String::from_utf8_lossy(&bytes).into_owned());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept observed request: {error}"),
                }
            }
            None
        });
        (format!("http://{address}"), handle)
    }

    fn start_raw_response_server(response: Vec<u8>) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind raw HTTP server");
        let address = listener.local_addr().expect("raw server address");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept raw request");
            let mut buffer = [0_u8; 2048];
            let _ = stream.read(&mut buffer).expect("read raw request");
            stream.write_all(&response).expect("write raw response");
        });
        (format!("http://{address}"), handle)
    }

    struct EnvironmentGuard {
        values: Vec<(&'static str, Option<OsString>)>,
    }

    impl EnvironmentGuard {
        fn set(values: &[(&'static str, &str)]) -> Self {
            let previous = values
                .iter()
                .map(|(name, _)| (*name, std::env::var_os(name)))
                .collect();
            for (name, value) in values {
                // SAFETY: the proxy regression runs in an isolated child process and holds
                // ENVIRONMENT_LOCK for the entire mutation lifetime.
                unsafe { std::env::set_var(name, value) };
            }
            Self { values: previous }
        }
    }

    impl Drop for EnvironmentGuard {
        fn drop(&mut self) {
            for (name, value) in &self.values {
                // SAFETY: the isolated child still holds ENVIRONMENT_LOCK here.
                unsafe {
                    if let Some(value) = value {
                        std::env::set_var(name, value);
                    } else {
                        std::env::remove_var(name);
                    }
                }
            }
        }
    }

    static ENVIRONMENT_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn every_v1_request_sends_the_configured_bearer_token() {
        let (base_url, server) = start_server(9);
        let client = HttpDaemonClient::new(base_url, "test-daemon-token")
            .expect("construct authenticated client");

        client.status().expect("status request");
        client
            .import_subscription("https://subscription.invalid/value")
            .expect("import request");
        client
            .refresh_subscription()
            .expect("refresh subscription request");
        client.connect().expect("connect request");
        client.disconnect().expect("disconnect request");
        client.events(0, None).expect("events request");
        client.catalog().expect("catalog request");
        client.latencies(Some(17)).expect("latency request");
        client.diagnostics().expect("diagnostics request");

        let requests = server.join().expect("join test HTTP server");
        assert_eq!(requests.len(), 9);
        for request in requests {
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer test-daemon-token\r\n"),
                "request omitted the Bearer header: {request}"
            );
        }
    }

    #[test]
    fn diagnostics_request_decodes_owned_health_and_core_logs() {
        let (base_url, server) = start_server(1);
        let client = HttpDaemonClient::new(base_url, "token").expect("authenticated client");

        let diagnostics = client.diagnostics().expect("diagnostics request");

        let requests = server.join().expect("join test HTTP server");
        assert!(requests[0].starts_with("GET /v1/diagnostics HTTP/1.1\r\n"));
        assert_eq!(diagnostics.xray, "ready");
        assert_eq!(diagnostics.mihomo, "ready");
        assert_eq!(diagnostics.tun, "ready");
        assert_eq!(diagnostics.logs.len(), 1);
        assert_eq!(diagnostics.logs[0].component, "mihomo");
        assert_eq!(diagnostics.logs[0].message, "safe core line");
        assert_eq!(diagnostics.mappings.len(), 1);
        assert_eq!(diagnostics.mappings[0].proxy_name, "Sweden [se]");
        assert_eq!(diagnostics.mappings[0].address, "127.0.0.1:31006");
        assert_eq!(diagnostics.mappings[0].xray_label, "Sweden [se]");
    }

    #[test]
    fn catalog_request_decodes_exact_dto_and_optional_delay() {
        let (base_url, server) = start_server(1);
        let client = HttpDaemonClient::new(base_url, "token").expect("authenticated client");

        let catalog = client.catalog().expect("catalog request");

        let requests = server.join().expect("join test HTTP server");
        assert!(requests[0].starts_with("GET /v1/catalog HTTP/1.1\r\n"));
        assert_eq!(
            catalog,
            DaemonCatalog {
                revision: 17,
                groups: vec![DaemonCatalogGroup {
                    id: "group:auto".into(),
                    label: "Авто".into(),
                    selected: true,
                    nodes: vec![
                        DaemonCatalogNode {
                            id: "node:a".into(),
                            label: "Север".into(),
                            selected: true,
                            delay_ms: Some(42),
                        },
                        DaemonCatalogNode {
                            id: "node:b".into(),
                            label: "Резерв".into(),
                            selected: false,
                            delay_ms: None,
                        },
                    ],
                }],
            }
        );
    }

    #[test]
    fn latency_request_posts_and_decodes_only_opaque_ids_safe_status_and_optional_value() {
        let body = r#"{"entries":[{"group_id":"group:auto","node_id":"node:a","latency_ms":42,"status":"ok"},{"group_id":"group:auto","node_id":"node:b","latency_ms":null,"status":"timeout"}]}"#;
        let (base_url, server) = start_selection_server("200 OK", body);
        let client =
            HttpDaemonClient::new(base_url, "latency-token").expect("authenticated client");

        let latencies = client.latencies(Some(17)).expect("latency request");

        let request = server.join().expect("join latency server");
        assert!(request.starts_with("POST /v1/latencies HTTP/1.1\r\n"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer latency-token\r\n")
        );
        assert!(request.ends_with(r#"{"revision":17}"#));
        assert_eq!(
            latencies,
            DaemonLatencies {
                entries: vec![
                    DaemonLatencyEntry {
                        group_id: "group:auto".into(),
                        node_id: "node:a".into(),
                        latency_ms: Some(42),
                        status: "ok".into(),
                    },
                    DaemonLatencyEntry {
                        group_id: "group:auto".into(),
                        node_id: "node:b".into(),
                        latency_ms: None,
                        status: "timeout".into(),
                    },
                ],
            }
        );
    }

    #[test]
    fn latency_conflict_decodes_stale_revision_without_exposing_response_details() {
        let (base_url, server) = start_selection_server(
            "409 Conflict",
            r#"{"code":"stale_revision","message":"private node marker"}"#,
        );
        let client = HttpDaemonClient::new(base_url, "latency-token").expect("loopback client");

        let error = client
            .latencies(Some(17))
            .expect_err("stale latency revision");

        let request = server.join().expect("join latency conflict server");
        assert!(request.starts_with("POST /v1/latencies HTTP/1.1\r\n"));
        assert!(error.is_stale_revision());
        assert!(!error.to_string().contains("private node marker"));
    }

    #[test]
    fn events_request_includes_cursor_and_url_encoded_known_epoch() {
        let (base_url, server) = start_server(2);
        let client =
            HttpDaemonClient::new(base_url, "token").expect("construct authenticated client");

        let first_events = client.events(0, None).expect("first events request");
        let events = client
            .events(42, Some("boot/a b"))
            .expect("subsequent events request");

        let requests = server.join().expect("join test HTTP server");
        assert!(
            requests[0].starts_with("GET /v1/events?after=0 HTTP/1.1\r\n"),
            "first request must omit epoch: {}",
            requests[0]
        );
        assert!(
            requests[1].starts_with("GET /v1/events?after=42&epoch=boot%2Fa+b HTTP/1.1\r\n"),
            "unexpected known-epoch request: {}",
            requests[1]
        );
        assert_eq!(first_events.epoch, "boot-a");
        assert_eq!(
            events,
            DaemonEventBatch {
                epoch: "boot-a".into(),
                events: vec![DaemonEvent {
                    id: 43,
                    timestamp: "1".into(),
                    level: "info".into(),
                    message: "Подключение установлено.".into(),
                }],
            }
        );
    }

    #[test]
    fn empty_daemon_token_is_rejected_without_making_a_request() {
        let result = HttpDaemonClient::new("http://127.0.0.1:1", "  ");

        assert!(result.is_err());
    }

    #[test]
    fn daemon_base_url_accepts_only_plain_http_loopback_socket_roots() {
        assert!(HttpDaemonClient::new("http://127.0.0.1:12345", "token").is_ok());
        assert!(HttpDaemonClient::new("http://127.9.8.7:12345/", "token").is_ok());
        assert!(HttpDaemonClient::new("http://[::1]:12345", "token").is_ok());

        for invalid in [
            "https://127.0.0.1:12345",
            "http://localhost:12345",
            "http://192.0.2.1:12345",
            "http://[2001:db8::1]:12345",
            "http://user@127.0.0.1:12345",
            "http://user:password@127.0.0.1:12345",
            "http://127.0.0.1:12345/v1",
            "http://127.0.0.1:12345/?mode=test",
            "http://127.0.0.1:12345/#fragment",
        ] {
            assert!(
                HttpDaemonClient::new(invalid, "token").is_err(),
                "accepted unsafe daemon URL: {invalid}"
            );
        }
    }

    #[test]
    fn redirects_are_not_followed_and_are_definitive_http_errors() {
        let stop = Arc::new(AtomicBool::new(false));
        let ok_body = r#"{"state":"ready"}"#;
        let ok_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            ok_body.len(),
            ok_body
        )
        .into_bytes();
        let (redirect_target, target) =
            start_observing_server("127.0.0.1:0", ok_response, Arc::clone(&stop));
        let redirect_response = format!(
            "HTTP/1.1 302 Found\r\nLocation: {redirect_target}/v1/status\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .into_bytes();
        let (base_url, redirector) = start_raw_response_server(redirect_response);
        let client = HttpDaemonClient::new(base_url, "redirect-token").expect("loopback client");

        let error = client.status().expect_err("redirect must not be followed");

        redirector.join().expect("join redirect server");
        stop.store(true, Ordering::SeqCst);
        let target_request = target.join().expect("join redirect target");
        assert!(target_request.is_none(), "redirect leaked a request");
        assert!(!error.requires_reconciliation());
        assert!(!error.to_string().contains("redirect-token"));
    }

    #[test]
    fn proxy_environment_cannot_receive_a_daemon_request_or_token() {
        let stop = Arc::new(AtomicBool::new(false));
        let status_body = r#"{"state":"ready"}"#;
        let status_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            status_body.len(),
            status_body
        )
        .into_bytes();
        let proxy_response =
            b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
        let (daemon_url, daemon) =
            start_observing_server("127.0.0.2:0", status_response, Arc::clone(&stop));
        let (proxy_url, proxy) =
            start_observing_server("127.0.0.1:0", proxy_response, Arc::clone(&stop));

        let output = Command::new(std::env::current_exe().expect("current test executable"))
            .args([
                "--ignored",
                "--exact",
                "daemon::http_tests::proxy_environment_child",
            ])
            .env("MULTICORE_PROXY_TEST_DAEMON_URL", &daemon_url)
            .env("MULTICORE_PROXY_TEST_PROXY_URL", &proxy_url)
            .output()
            .expect("run isolated proxy regression child");

        stop.store(true, Ordering::SeqCst);
        let daemon_request = daemon.join().expect("join daemon observer");
        let proxy_request = proxy.join().expect("join proxy observer");
        assert!(
            output.status.success(),
            "proxy child failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let daemon_request = daemon_request.expect("daemon must receive direct request");
        assert!(
            daemon_request
                .to_ascii_lowercase()
                .contains("authorization: bearer proxy-regression-secret\r\n")
        );
        assert!(
            proxy_request.is_none(),
            "proxy received daemon credentials: {proxy_request:?}"
        );
    }

    #[test]
    #[ignore = "isolated helper invoked by proxy_environment_cannot_receive_a_daemon_request_or_token"]
    fn proxy_environment_child() {
        let daemon_url =
            std::env::var("MULTICORE_PROXY_TEST_DAEMON_URL").expect("proxy test daemon URL");
        let proxy_url =
            std::env::var("MULTICORE_PROXY_TEST_PROXY_URL").expect("proxy test proxy URL");
        let _environment_lock = ENVIRONMENT_LOCK.lock().expect("environment lock");
        let _environment = EnvironmentGuard::set(&[
            ("HTTP_PROXY", &proxy_url),
            ("http_proxy", &proxy_url),
            ("ALL_PROXY", &proxy_url),
            ("all_proxy", &proxy_url),
            ("NO_PROXY", ""),
            ("no_proxy", ""),
        ]);

        let client = HttpDaemonClient::new(daemon_url, "proxy-regression-secret")
            .expect("construct isolated proxy test client");
        client.status().expect("direct daemon status request");
    }

    #[test]
    fn catalog_rejects_oversized_content_length_before_decoding() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 33554433\r\nConnection: close\r\n\r\n".to_vec();
        let (base_url, server) = start_raw_response_server(response);
        let client = HttpDaemonClient::new(base_url, "catalog-token").expect("loopback client");

        let error = client.catalog().expect_err("oversized catalog must fail");

        server.join().expect("join oversized catalog server");
        assert!(error.requires_reconciliation());
        assert!(!error.to_string().contains("catalog-token"));
    }

    #[test]
    fn decoded_status_body_is_bounded_for_chunked_and_close_delimited_responses() {
        let oversized = vec![b'x'; 65_537];
        let mut chunked = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n",
            oversized.len()
        )
        .into_bytes();
        chunked.extend_from_slice(&oversized);
        chunked.extend_from_slice(b"\r\n0\r\n\r\n");

        let mut close_delimited =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n"
                .to_vec();
        close_delimited.extend_from_slice(&oversized);

        for response in [chunked, close_delimited] {
            let (base_url, server) = start_raw_response_server(response);
            let client = HttpDaemonClient::new(base_url, "status-token").expect("loopback client");

            let error = client.status().expect_err("oversized status must fail");

            server.join().expect("join oversized status server");
            assert!(error.requires_reconciliation());
            assert!(!error.to_string().contains("status-token"));
        }
    }

    #[test]
    fn malformed_events_json_is_returned_as_an_error() {
        let (base_url, server) = start_single_response_server(r#"{"events":"not-an-array"}"#);
        let client =
            HttpDaemonClient::new(base_url, "token").expect("construct authenticated client");

        let result = client.events(0, None);

        server.join().expect("join test HTTP server");
        assert!(result.is_err());
    }

    #[test]
    fn malformed_catalog_json_is_returned_as_an_error() {
        let (base_url, server) = start_single_response_server(r#"{"revision":1,"groups":{}}"#);
        let client = HttpDaemonClient::new(base_url, "token").expect("authenticated client");

        let result = client.catalog();

        server.join().expect("join test HTTP server");
        assert!(result.is_err());
    }

    #[test]
    fn selection_request_percent_encodes_group_and_sends_exact_json_and_auth() {
        let response = r#"{"revision":91,"groups":[]}"#;
        let (base_url, server) = start_selection_server("200 OK", response);
        let client = HttpDaemonClient::new(base_url, "selection-token")
            .expect("construct authenticated client");

        let catalog = client
            .select_node("opaque/group ?#%", 91, "opaque-node")
            .expect("selection request");

        let request = server.join().expect("join selection server");
        let (headers, body) = request.split_once("\r\n\r\n").expect("HTTP request");
        assert!(headers.starts_with("PUT /v1/selections/opaque%2Fgroup%20%3F%23%25 HTTP/1.1\r\n"));
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("authorization: bearer selection-token\r\n")
        );
        assert_eq!(body, r#"{"revision":91,"node_id":"opaque-node"}"#);
        assert_eq!(catalog.revision, 91);
    }

    #[test]
    fn selection_conflict_is_classified_without_exposing_response_details() {
        let sensitive = "submitted-private-node-marker";
        let (base_url, server) = start_selection_server(
            "409 Conflict",
            r#"{"code":"stale_revision","message":"submitted-private-node-marker"}"#,
        );
        let client = HttpDaemonClient::new(base_url, "selection-token")
            .expect("construct authenticated client");

        let error = client
            .select_node("opaque-group", 90, sensitive)
            .expect_err("stale selection must fail");

        server.join().expect("join selection server");
        assert!(error.is_stale_revision());
        assert!(!error.requires_reconciliation());
        assert!(!error.to_string().contains(sensitive));
    }

    #[test]
    fn non_stale_selection_conflicts_are_ordinary_safe_errors() {
        let sensitive = "submitted-private-node-marker";
        for (case, body) in [
            (
                "not connected",
                r#"{"code":"not_connected","message":"submitted-private-node-marker"}"#,
            ),
            (
                "unknown code",
                r#"{"code":"future_conflict","message":"submitted-private-node-marker"}"#,
            ),
            (
                "malformed JSON",
                r#"{"code":"stale_revision","message":"submitted-private-node-marker""#,
            ),
        ] {
            let (base_url, server) = start_selection_server("409 Conflict", body);
            let client = HttpDaemonClient::new(base_url, "selection-token")
                .expect("construct authenticated client");

            let error = client
                .select_node("opaque-group", 90, sensitive)
                .unwrap_err();

            server.join().expect("join selection server");
            assert!(!error.is_stale_revision(), "case: {case}");
            assert!(!error.requires_reconciliation(), "case: {case}");
            assert_eq!(
                error.to_string(),
                "Не удалось изменить маршрут.",
                "case: {case}"
            );
            assert!(!error.to_string().contains(sensitive), "case: {case}");
        }
    }

    #[test]
    fn selection_errors_reconcile_only_after_ambiguous_success_or_transport() {
        let sensitive = "private-selection-marker";

        let (base_url, server) = start_selection_server(
            "500 Internal Server Error",
            r#"{"message":"private-selection-marker"}"#,
        );
        let client = HttpDaemonClient::new(base_url, "selection-token").expect("loopback client");
        let definitive = client
            .select_node("group", 1, sensitive)
            .expect_err("HTTP business failure");
        server.join().expect("join business-error server");
        assert!(!definitive.requires_reconciliation());
        assert!(!definitive.to_string().contains(sensitive));

        let (base_url, server) = start_selection_server("200 OK", r#"{"revision":1,"groups":{}}"#);
        let client = HttpDaemonClient::new(base_url, "selection-token").expect("loopback client");
        let malformed = client
            .select_node("group", 1, sensitive)
            .expect_err("malformed successful response");
        server.join().expect("join malformed-response server");
        assert!(malformed.requires_reconciliation());
        assert!(!malformed.to_string().contains(sensitive));

        let response = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 33554433\r\nConnection: close\r\n\r\n".to_vec();
        let (base_url, server) = start_raw_response_server(response);
        let client = HttpDaemonClient::new(base_url, "selection-token").expect("loopback client");
        let oversized = client
            .select_node("group", 1, sensitive)
            .expect_err("oversized successful response");
        server.join().expect("join oversized-response server");
        assert!(oversized.requires_reconciliation());
        assert!(!oversized.to_string().contains(sensitive));

        let listener = TcpListener::bind("127.0.0.1:0").expect("reserve unused port");
        let address = listener.local_addr().expect("unused port address");
        drop(listener);
        let client = HttpDaemonClient::new(format!("http://{address}"), "selection-token")
            .expect("loopback client");
        let transport = client
            .select_node("group", 1, sensitive)
            .expect_err("transport failure");
        assert!(transport.requires_reconciliation());
        assert!(!transport.to_string().contains(sensitive));
    }
}
