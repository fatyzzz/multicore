use std::{
    collections::{HashSet, VecDeque},
    ffi::OsStr,
    fmt, io,
    marker::PhantomData,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{Request, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use base64::{Engine as _, engine::general_purpose};
use multicore_core::{
    AtomicSnapshot, Component, ConnectionState as CoreConnectionState, DiagnosticStream, Event,
    HttpClient, PersistentSnapshotStore, ProcessController, ProcessDiagnostics, RuntimeCheckState,
    Severity, Snapshot, StagedRuntime, StateMachine, SubscriptionFetcher, Supervisor,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::{net::TcpListener, sync::RwLock};
use tower_http::limit::RequestBodyLimitLayer;

const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;
/// Maximum number of untrusted Mihomo groups exposed by the local UI API.
pub const MAX_CATALOG_GROUPS: usize = 64;
/// Maximum number of untrusted node names exposed for a single Mihomo group.
pub const MAX_CATALOG_NODES_PER_GROUP: usize = 256;
/// Maximum Unicode scalar count retained for an untrusted group or node name.
pub const MAX_CATALOG_NAME_CHARS: usize = 256;
/// Maximum number of latency results accepted and returned by one request.
pub const MAX_LATENCY_RESULTS: usize = 256;
/// Maximum number of in-flight requests to the Mihomo controller per latency operation.
pub const MAX_CONCURRENT_LATENCY_PROBES: usize = 8;
const LATENCY_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const LATENCY_BATCH_TIMEOUT: Duration = Duration::from_secs(8);
/// Maximum number of Mihomo-to-Xray loopback bridge mappings exposed in diagnostics.
pub const MAX_DIAGNOSTIC_MAPPINGS: usize = 256;
static NEXT_CORRELATION_ID: AtomicU64 = AtomicU64::new(1);
/// Maximum number of daemon events retained in memory. Older entries are evicted first.
pub const MAX_RETAINED_EVENTS: usize = 512;
/// Maximum number of events returned by one cursor request.
///
/// Pages contain the oldest retained events whose IDs are greater than `after`, so a client can
/// advance its cursor to the last returned ID without skipping retained entries.
pub const MAX_EVENT_PAGE_SIZE: usize = 256;
/// Maximum byte length of the single readiness line written for an owning launcher.
pub const MAX_READINESS_LINE_BYTES: usize = 80;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    #[default]
    Empty,
    Ready,
    Connected,
    Connecting,
    Error,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StatusDto {
    pub state: ConnectionState,
    pub profile: Option<String>,
    pub current_node: Option<String>,
    pub message: Option<String>,
    pub degraded: bool,
    pub subscription: Option<SubscriptionInfoDto>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubscriptionInfoDto {
    pub source_name: String,
    pub downloaded_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub expires_at_unix: Option<u64>,
    pub updated_at_unix: u64,
    pub refresh_available: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ImportSubscriptionRequest {
    pub url: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EventDto {
    pub id: u64,
    pub timestamp: String,
    pub level: String,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EventPageDto {
    pub epoch: String,
    pub events: Vec<EventDto>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeCheckDto {
    #[default]
    Stopped,
    Starting,
    Ready,
    Failed,
    Unsupported,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DiagnosticLogDto {
    pub id: u64,
    pub timestamp: String,
    pub component: String,
    pub level: String,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProxyMappingDto {
    pub proxy_name: String,
    pub address: String,
    pub xray_label: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct DiagnosticsDto {
    pub xray: RuntimeCheckDto,
    pub mihomo: RuntimeCheckDto,
    pub tun: RuntimeCheckDto,
    pub logs: Vec<DiagnosticLogDto>,
    #[serde(default)]
    pub mappings: Vec<ProxyMappingDto>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CatalogDto {
    pub revision: u64,
    pub groups: Vec<GroupDto>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GroupDto {
    pub id: String,
    pub label: String,
    pub selected: bool,
    pub nodes: Vec<NodeDto>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeDto {
    pub id: String,
    pub label: String,
    pub selected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionRequest {
    pub revision: u64,
    pub node_id: String,
    #[serde(skip)]
    pub group_id: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LatencyRequest {
    #[serde(default)]
    pub revision: Option<u64>,
    #[serde(default)]
    pub group_ids: Vec<String>,
    #[serde(default)]
    pub node_ids: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LatencyStatus {
    Ok,
    Timeout,
    Error,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LatencyResultDto {
    pub group_id: String,
    pub node_id: String,
    pub latency_ms: Option<u64>,
    pub status: LatencyStatus,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LatencyResponseDto {
    pub entries: Vec<LatencyResultDto>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApiErrorBody {
    pub code: String,
    pub message: String,
    pub correlation_id: String,
}

#[derive(Clone, Debug)]
pub struct BackendError {
    _private: (),
}

impl BackendError {
    #[must_use]
    pub const fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for BackendError {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("операция backend не выполнена")
    }
}

impl std::error::Error for BackendError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SelectionError {
    StaleRevision,
    NotConnected,
    UnknownGroup,
    UnknownNode,
    SelectorFailed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LatencyError {
    StaleRevision,
    NotConnected,
    UnknownGroup,
    UnknownNode,
    TooManyTargets,
}

impl fmt::Display for LatencyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Mihomo latency probe was not completed")
    }
}

impl std::error::Error for LatencyError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LatencyProbeError {
    Timeout,
    Unavailable,
}

impl fmt::Display for SelectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Mihomo selection was not applied")
    }
}

impl std::error::Error for SelectionError {}

#[derive(Clone, Debug)]
pub struct SelectorError {
    _private: (),
}

impl SelectorError {
    #[must_use]
    pub const fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for SelectorError {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SelectorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Mihomo controller request failed")
    }
}

impl std::error::Error for SelectorError {}

#[async_trait]
pub trait MihomoSelector: Send + Sync + 'static {
    async fn select(&self, raw_group: &str, raw_node: &str) -> Result<(), SelectorError>;

    async fn probe_latency(&self, _raw_name: &str) -> Result<u64, LatencyProbeError> {
        Err(LatencyProbeError::Unavailable)
    }
}

struct UnavailableSelector;

#[async_trait]
impl MihomoSelector for UnavailableSelector {
    async fn select(&self, _raw_group: &str, _raw_node: &str) -> Result<(), SelectorError> {
        Err(SelectorError::new())
    }
}

pub struct MihomoHttpSelector {
    client: reqwest::Client,
    base_url: reqwest::Url,
    authorization: header::HeaderValue,
}

impl MihomoHttpSelector {
    pub fn new(address: SocketAddr, secret: &str) -> io::Result<Self> {
        if !address.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Mihomo controller address must be loopback",
            ));
        }
        if secret.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Mihomo controller secret must not be empty",
            ));
        }
        let base_url = reqwest::Url::parse(&format!("http://{address}/"))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let authorization = header::HeaderValue::from_str(&format!("Bearer {secret}"))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(1))
            .timeout(Duration::from_secs(3))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(io::Error::other)?;
        Ok(Self {
            client,
            base_url,
            authorization,
        })
    }
}

#[async_trait]
impl MihomoSelector for MihomoHttpSelector {
    async fn select(&self, raw_group: &str, raw_node: &str) -> Result<(), SelectorError> {
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .map_err(|_| SelectorError::new())?
            .extend(["proxies", raw_group]);
        let response = self
            .client
            .put(url.clone())
            .header(header::AUTHORIZATION, self.authorization.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .body(
                serde_json::to_vec(&json!({ "name": raw_node }))
                    .map_err(|_| SelectorError::new())?,
            )
            .send()
            .await
            .map_err(|_| SelectorError::new())?;
        if response.status() != StatusCode::NO_CONTENT {
            return Err(SelectorError::new());
        }

        let mut response = self
            .client
            .get(url)
            .header(header::AUTHORIZATION, self.authorization.clone())
            .send()
            .await
            .map_err(|_| SelectorError::new())?;
        if response.status() != StatusCode::OK {
            return Err(SelectorError::new());
        }
        const MAX_PROXY_STATUS_BYTES: usize = 1024 * 1024;
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| SelectorError::new())? {
            if body.len().saturating_add(chunk.len()) > MAX_PROXY_STATUS_BYTES {
                return Err(SelectorError::new());
            }
            body.extend_from_slice(&chunk);
        }
        let live: serde_json::Value =
            serde_json::from_slice(&body).map_err(|_| SelectorError::new())?;
        if live.get("now").and_then(serde_json::Value::as_str) != Some(raw_node) {
            return Err(SelectorError::new());
        }

        let mut connections_url = self.base_url.clone();
        connections_url
            .path_segments_mut()
            .map_err(|_| SelectorError::new())?
            .push("connections");
        let _ = self
            .client
            .delete(connections_url)
            .header(header::AUTHORIZATION, self.authorization.clone())
            .send()
            .await;
        Ok(())
    }

    async fn probe_latency(&self, raw_name: &str) -> Result<u64, LatencyProbeError> {
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .map_err(|_| LatencyProbeError::Unavailable)?
            .extend(["proxies", raw_name, "delay"]);
        url.query_pairs_mut()
            .append_pair("timeout", &LATENCY_PROBE_TIMEOUT.as_millis().to_string())
            .append_pair("url", "https://www.gstatic.com/generate_204");
        let mut response = self
            .client
            .get(url)
            .header(header::AUTHORIZATION, self.authorization.clone())
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    LatencyProbeError::Timeout
                } else {
                    LatencyProbeError::Unavailable
                }
            })?;
        if response.status() == StatusCode::REQUEST_TIMEOUT
            || response.status() == StatusCode::GATEWAY_TIMEOUT
        {
            return Err(LatencyProbeError::Timeout);
        }
        if response.status() != StatusCode::OK {
            return Err(LatencyProbeError::Unavailable);
        }

        const MAX_DELAY_RESPONSE_BYTES: usize = 1024;
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| LatencyProbeError::Unavailable)?
        {
            if body.len().saturating_add(chunk.len()) > MAX_DELAY_RESPONSE_BYTES {
                return Err(LatencyProbeError::Unavailable);
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| value.get("delay").and_then(serde_json::Value::as_u64))
            .ok_or(LatencyProbeError::Unavailable)
    }
}

async fn probe_latency_target(
    selector: Arc<dyn MihomoSelector>,
    permits: Arc<tokio::sync::Semaphore>,
    index: usize,
    group_id: String,
    node_id: String,
    raw_node: String,
) -> (usize, LatencyResultDto) {
    let outcome = tokio::time::timeout(LATENCY_PROBE_TIMEOUT, async {
        let _permit = permits
            .acquire_owned()
            .await
            .map_err(|_| LatencyProbeError::Unavailable)?;
        selector.probe_latency(&raw_node).await
    })
    .await;
    let (latency_ms, status) = match outcome {
        Ok(Ok(latency_ms)) => (Some(latency_ms), LatencyStatus::Ok),
        Ok(Err(LatencyProbeError::Timeout)) | Err(_) => (None, LatencyStatus::Timeout),
        Ok(Err(LatencyProbeError::Unavailable)) => (None, LatencyStatus::Error),
    };
    (
        index,
        LatencyResultDto {
            group_id,
            node_id,
            latency_ms,
            status,
        },
    )
}

#[async_trait]
pub trait Backend: Send + Sync + 'static {
    async fn status(&self) -> Result<StatusDto, BackendError>;
    async fn catalog(&self) -> Result<CatalogDto, BackendError>;
    async fn import_subscription(
        &self,
        request: ImportSubscriptionRequest,
    ) -> Result<StatusDto, BackendError>;
    async fn refresh_subscription(&self) -> Result<StatusDto, BackendError> {
        Err(BackendError::new())
    }
    async fn connect(&self) -> Result<StatusDto, BackendError>;
    async fn disconnect(&self) -> Result<StatusDto, BackendError>;
    async fn select(&self, _request: SelectionRequest) -> Result<CatalogDto, SelectionError> {
        Err(SelectionError::NotConnected)
    }
    async fn latencies(
        &self,
        _request: LatencyRequest,
    ) -> Result<LatencyResponseDto, LatencyError> {
        Err(LatencyError::NotConnected)
    }
    /// Returns at most [`MAX_EVENT_PAGE_SIZE`] oldest retained events with IDs greater than
    /// `after`. A stale cursor resumes at the oldest event still present in the bounded buffer.
    async fn events_after(&self, after: u64) -> Result<Vec<EventDto>, BackendError>;
    async fn diagnostics(&self) -> Result<DiagnosticsDto, BackendError> {
        Ok(DiagnosticsDto::default())
    }
}

#[derive(Clone)]
struct AppState {
    backend: Arc<dyn Backend>,
    event_epoch: Arc<str>,
}

#[derive(Clone)]
struct AuthState {
    token_hash: [u8; 32],
}

/// Builds the local daemon HTTP API. No CORS layer is installed intentionally.
pub fn router<B>(backend: B, bearer_token: impl AsRef<str>) -> Router
where
    B: Backend,
{
    try_router(backend, bearer_token)
        .expect("operating-system randomness unavailable for daemon event epoch")
}

/// Fallible production constructor. Event epochs are generated exclusively from OS randomness.
pub fn try_router<B>(backend: B, bearer_token: impl AsRef<str>) -> Result<Router, getrandom::Error>
where
    B: Backend,
{
    Ok(router_with_event_epoch(
        backend,
        bearer_token,
        generate_event_epoch()?,
    ))
}

/// Builds the local daemon HTTP API with an explicit event epoch.
///
/// Production callers should use [`try_router`]. This constructor exists so transport tests can
/// use a deterministic restart identity.
pub fn router_with_event_epoch<B>(
    backend: B,
    bearer_token: impl AsRef<str>,
    event_epoch: impl Into<String>,
) -> Router
where
    B: Backend,
{
    let app_state = AppState {
        backend: Arc::new(backend),
        event_epoch: Arc::from(event_epoch.into()),
    };
    let auth_state = AuthState {
        token_hash: Sha256::digest(bearer_token.as_ref().as_bytes()).into(),
    };

    let v1 = Router::new()
        .route("/status", get(status))
        .route("/catalog", get(catalog))
        .route("/latencies", post(latencies))
        .route("/selections/{group_id}", put(select_node))
        .route("/subscriptions/import", post(import_subscription))
        .route("/subscriptions/refresh", post(refresh_subscription))
        .route("/connect", post(connect))
        .route("/disconnect", post(disconnect))
        .route("/events", get(events))
        .route("/diagnostics", get(diagnostics))
        .fallback(not_found)
        .layer(RequestBodyLimitLayer::new(MAX_REQUEST_BODY_BYTES))
        .layer(middleware::from_fn(normalize_errors))
        .layer(middleware::from_fn_with_state(auth_state, authorize))
        .with_state(app_state);

    Router::new().nest("/v1", v1)
}

fn generate_event_epoch() -> Result<String, getrandom::Error> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random)?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

async fn normalize_errors(request: Request<Body>, next: Next) -> Response {
    let response = next.run(request).await;
    if !response.status().is_client_error() && !response.status().is_server_error() {
        return response;
    }
    if response
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(|value| value.as_bytes().starts_with(b"application/json"))
    {
        return response;
    }

    match response.status() {
        StatusCode::METHOD_NOT_ALLOWED => ApiError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "Метод не поддерживается. Проверьте HTTP-метод запроса.",
        )
        .into_response(),
        StatusCode::PAYLOAD_TOO_LARGE => ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            "Тело запроса превышает 1 МиБ. Уменьшите запрос и повторите попытку.",
        )
        .into_response(),
        status => ApiError::new(
            status,
            "request_failed",
            "Запрос не выполнен. Проверьте его параметры и повторите попытку.",
        )
        .into_response(),
    }
}

/// Binds only to an IP loopback interface.
pub async fn bind_loopback(address: SocketAddr) -> io::Result<TcpListener> {
    if !address.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "адрес daemon должен быть loopback; используйте 127.0.0.1 или ::1",
        ));
    }
    TcpListener::bind(address).await
}

/// Publishes the resolved daemon address to an owning launcher when explicitly enabled.
///
/// The bearer token is deliberately not an input to this function, so the bounded stdout
/// protocol can contain only the literal socket address. `Ok(false)` means publication was not
/// enabled; the writer is left untouched in that case.
pub fn publish_readiness_if_enabled<W>(
    writer: &mut W,
    address: SocketAddr,
    ready_stdout: Option<&OsStr>,
) -> io::Result<bool>
where
    W: io::Write,
{
    if ready_stdout != Some(OsStr::new("1")) {
        return Ok(false);
    }
    if !address.ip().is_loopback() || address.port() == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "readiness address must be a loopback SocketAddr with a nonzero port",
        ));
    }

    let line = format!("MULTICORE_READY {address}\n");
    if !line.is_ascii() || line.len() > MAX_READINESS_LINE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "readiness line exceeds the bounded ASCII protocol",
        ));
    }
    writer.write_all(line.as_bytes())?;
    writer.flush()?;
    Ok(true)
}

async fn authorize(State(auth): State<AuthState>, request: Request<Body>, next: Next) -> Response {
    let supplied = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| !token.is_empty());

    let authenticated = supplied.is_some_and(|token| {
        let supplied_hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        bool::from(supplied_hash.ct_eq(&auth.token_hash))
    });

    if authenticated {
        next.run(request).await
    } else {
        ApiError::unauthorized().into_response()
    }
}

async fn status(State(state): State<AppState>, body: Bytes) -> Result<Json<StatusDto>, ApiError> {
    reject_nonempty_body(&body)?;
    state
        .backend
        .status()
        .await
        .map(Json)
        .map_err(|_| ApiError::backend("Не удалось получить состояние. Повторите попытку."))
}

async fn catalog(State(state): State<AppState>, body: Bytes) -> Result<Json<CatalogDto>, ApiError> {
    reject_nonempty_body(&body)?;
    state
        .backend
        .catalog()
        .await
        .map(Json)
        .map_err(|_| ApiError::backend("Не удалось получить каталог. Повторите попытку."))
}

async fn latencies(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Json<LatencyResponseDto>, ApiError> {
    let request = if body.is_empty() {
        LatencyRequest::default()
    } else {
        serde_json::from_slice(&body).map_err(|_| {
            ApiError::bad_request("Некорректное тело запроса. Проверьте JSON и поля целей.")
        })?
    };
    state
        .backend
        .latencies(request)
        .await
        .map(Json)
        .map_err(|error| match error {
            LatencyError::StaleRevision => ApiError::new(
                StatusCode::CONFLICT,
                "stale_revision",
                "Каталог изменился. Обновите список маршрутов и повторите проверку.",
            ),
            LatencyError::NotConnected => ApiError::new(
                StatusCode::CONFLICT,
                "not_connected",
                "Сначала установите подключение.",
            ),
            LatencyError::UnknownGroup => ApiError::new(
                StatusCode::NOT_FOUND,
                "unknown_group",
                "Группа маршрутов не найдена. Обновите каталог.",
            ),
            LatencyError::UnknownNode => ApiError::new(
                StatusCode::NOT_FOUND,
                "unknown_node",
                "Узел не найден. Обновите каталог.",
            ),
            LatencyError::TooManyTargets => ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "too_many_targets",
                "Запрос содержит слишком много целей проверки.",
            ),
        })
}

async fn select_node(
    State(state): State<AppState>,
    Path(group_id): Path<String>,
    payload: Result<Json<SelectionRequest>, JsonRejection>,
) -> Result<Json<CatalogDto>, ApiError> {
    let Json(mut request) = payload.map_err(ApiError::from_json_rejection)?;
    request.group_id = group_id;
    state
        .backend
        .select(request)
        .await
        .map(Json)
        .map_err(|error| match error {
            SelectionError::StaleRevision => ApiError::new(
                StatusCode::CONFLICT,
                "stale_revision",
                "Каталог изменился. Обновите список маршрутов и повторите выбор.",
            ),
            SelectionError::NotConnected => ApiError::new(
                StatusCode::CONFLICT,
                "not_connected",
                "Сначала установите подключение.",
            ),
            SelectionError::UnknownGroup => ApiError::new(
                StatusCode::NOT_FOUND,
                "unknown_group",
                "Группа маршрутов не найдена. Обновите каталог.",
            ),
            SelectionError::UnknownNode => ApiError::new(
                StatusCode::NOT_FOUND,
                "unknown_node",
                "Узел не найден. Обновите каталог.",
            ),
            SelectionError::SelectorFailed => ApiError::new(
                StatusCode::BAD_GATEWAY,
                "selector_failed",
                "Mihomo не применил выбор маршрута. Повторите попытку.",
            ),
        })
}

async fn import_subscription(
    State(state): State<AppState>,
    payload: Result<Json<ImportSubscriptionRequest>, JsonRejection>,
) -> Result<Json<StatusDto>, ApiError> {
    let Json(request) = payload.map_err(ApiError::from_json_rejection)?;
    if request.url.trim().is_empty() {
        return Err(ApiError::bad_request(
            "Ссылка подписки пуста. Укажите URL и повторите попытку.",
        ));
    }
    state
        .backend
        .import_subscription(request)
        .await
        .map(Json)
        .map_err(|_| {
            ApiError::backend(
                "Не удалось импортировать подписку. Проверьте ссылку и повторите попытку.",
            )
        })
}

async fn refresh_subscription(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Json<StatusDto>, ApiError> {
    reject_nonempty_body(&body)?;
    state
        .backend
        .refresh_subscription()
        .await
        .map(Json)
        .map_err(|_| {
            ApiError::backend(
                "Не удалось обновить подписку. Если профиль старый, укажите ссылку ещё раз.",
            )
        })
}

async fn connect(State(state): State<AppState>, body: Bytes) -> Result<Json<StatusDto>, ApiError> {
    reject_nonempty_body(&body)?;
    state
        .backend
        .connect()
        .await
        .map(Json)
        .map_err(|_| ApiError::backend("Не удалось подключиться. Повторите попытку."))
}

async fn disconnect(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Json<StatusDto>, ApiError> {
    reject_nonempty_body(&body)?;
    state
        .backend
        .disconnect()
        .await
        .map(Json)
        .map_err(|_| ApiError::backend("Не удалось отключиться. Повторите попытку."))
}

#[derive(Deserialize)]
struct EventsQuery {
    after: u64,
    epoch: Option<String>,
}

async fn events(
    State(state): State<AppState>,
    query: Result<Query<EventsQuery>, axum::extract::rejection::QueryRejection>,
    body: Bytes,
) -> Result<Json<EventPageDto>, ApiError> {
    reject_nonempty_body(&body)?;
    let Query(query) = query.map_err(|_| {
        ApiError::bad_request("Некорректный параметр after. Укажите целое неотрицательное число.")
    })?;
    let after = match query.epoch.as_deref() {
        Some(epoch) if epoch != state.event_epoch.as_ref() => 0,
        Some(_) | None => query.after,
    };
    let mut events = state
        .backend
        .events_after(after)
        .await
        .map_err(|_| ApiError::backend("Не удалось получить события. Повторите попытку."))?;
    events.truncate(MAX_EVENT_PAGE_SIZE);
    Ok(Json(EventPageDto {
        epoch: state.event_epoch.to_string(),
        events,
    }))
}

async fn diagnostics(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Json<DiagnosticsDto>, ApiError> {
    reject_nonempty_body(&body)?;
    state
        .backend
        .diagnostics()
        .await
        .map(Json)
        .map_err(|_| ApiError::backend("Не удалось получить диагностику. Повторите попытку."))
}

fn reject_nonempty_body(body: &Bytes) -> Result<(), ApiError> {
    if body.is_empty() {
        Ok(())
    } else {
        Err(ApiError::bad_request(
            "Тело для этого запроса не допускается. Удалите тело и повторите попытку.",
        ))
    }
}

async fn not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "route_not_found",
        "Маршрут не найден. Проверьте путь запроса.",
    )
}

struct ApiError {
    status: StatusCode,
    body: ApiErrorBody,
}

impl ApiError {
    fn new(status: StatusCode, code: &str, message: &str) -> Self {
        let id = NEXT_CORRELATION_ID.fetch_add(1, Ordering::Relaxed);
        Self {
            status,
            body: ApiErrorBody {
                code: code.to_owned(),
                message: message.to_owned(),
                correlation_id: format!("daemon-{id:016x}"),
            },
        }
    }

    fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Требуется авторизация. Передайте действующий Bearer-токен.",
        )
    }

    fn bad_request(message: &str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }

    fn backend(message: &str) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "backend_error", message)
    }

    fn from_json_rejection(rejection: JsonRejection) -> Self {
        let status = rejection.status();
        if status == StatusCode::PAYLOAD_TOO_LARGE {
            Self::new(
                status,
                "payload_too_large",
                "Тело запроса превышает 1 МиБ. Уменьшите запрос и повторите попытку.",
            )
        } else {
            Self::bad_request("Некорректное тело запроса. Проверьте JSON и обязательные поля.")
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

struct EventBuffer<T> {
    entries: VecDeque<(u64, T)>,
    next_event_id: u64,
}

impl<T> Default for EventBuffer<T> {
    fn default() -> Self {
        Self {
            entries: VecDeque::with_capacity(MAX_RETAINED_EVENTS),
            next_event_id: 1,
        }
    }
}

impl<T> EventBuffer<T> {
    fn push_with(&mut self, build: impl FnOnce(u64) -> T) -> u64 {
        let id = self.next_event_id;
        self.next_event_id = self
            .next_event_id
            .checked_add(1)
            .expect("daemon event ID space exhausted");
        if self.entries.len() == MAX_RETAINED_EVENTS {
            self.entries.pop_front();
        }
        self.entries.push_back((id, build(id)));
        id
    }

    fn after(&self, cursor: u64) -> impl Iterator<Item = (u64, &T)> {
        self.entries
            .iter()
            .filter(move |(id, _)| *id > cursor)
            .take(MAX_EVENT_PAGE_SIZE)
            .map(|(id, event)| (*id, event))
    }
}

pub struct PreparedController<P: ProcessController> {
    controller: Arc<P>,
    runtime: Option<StagedRuntime>,
}

impl<P: ProcessController> PreparedController<P> {
    pub fn with_runtime(controller: Arc<P>, runtime: StagedRuntime) -> Self {
        Self {
            controller,
            runtime: Some(runtime),
        }
    }

    fn plain(controller: Arc<P>) -> Self {
        Self {
            controller,
            runtime: None,
        }
    }

    fn controller(&self) -> Arc<P> {
        self.controller.clone()
    }

    fn activate(mut self) -> Result<(), BackendError> {
        if let Some(runtime) = self.runtime.take() {
            runtime.commit().map_err(|_| BackendError::new())?;
        }
        Ok(())
    }
}

pub struct TransactionalControllerFactory<F>(F);

pub fn transactional_controller_factory<F>(factory: F) -> TransactionalControllerFactory<F> {
    TransactionalControllerFactory(factory)
}

type PreparedFactory<P> =
    dyn Fn(&Snapshot) -> Result<PreparedController<P>, BackendError> + Send + Sync;

pub struct CoreBackend<C, P, F>
where
    C: HttpClient,
    P: ProcessController,
{
    store: PersistentSnapshotStore,
    fetcher: SubscriptionFetcher<C>,
    controller_factory: Arc<PreparedFactory<P>>,
    factory_type: PhantomData<fn() -> F>,
    selector: Arc<dyn MihomoSelector>,
    latency_permits: Arc<tokio::sync::Semaphore>,
    operation: tokio::sync::Mutex<()>,
    inner: RwLock<CoreRuntime<P>>,
}

struct CoreRuntime<P: ProcessController> {
    state: StateMachine,
    controller: Option<Arc<P>>,
    snapshot: Option<Arc<Snapshot>>,
    catalog: CatalogDto,
    profile: Option<String>,
    current_node: Option<String>,
    selected_nodes: Vec<Option<String>>,
    message: Option<String>,
    degraded: bool,
    events: EventBuffer<Event>,
}

impl<P: ProcessController> CoreRuntime<P> {
    fn empty() -> Self {
        Self {
            state: StateMachine::default(),
            controller: None,
            snapshot: None,
            catalog: CatalogDto {
                revision: 0,
                groups: Vec::new(),
            },
            profile: None,
            current_node: None,
            selected_nodes: Vec::new(),
            message: None,
            degraded: false,
            events: EventBuffer::default(),
        }
    }

    fn status(&self) -> StatusDto {
        let profile = self
            .catalog
            .groups
            .iter()
            .find(|group| group.selected)
            .map(|group| group.label.clone())
            .or_else(|| {
                self.profile
                    .as_deref()
                    .map(|profile| safe_catalog_label(profile, "Профиль", 0))
            });
        let current_node = self
            .catalog
            .groups
            .iter()
            .find(|group| group.selected)
            .and_then(|group| group.nodes.iter().find(|node| node.selected))
            .map(|node| node.label.clone())
            .or_else(|| {
                self.current_node
                    .as_deref()
                    .map(|node| safe_catalog_label(node, "Узел", 0))
            });
        StatusDto {
            state: map_core_state(self.state.current()),
            profile,
            current_node,
            message: self.message.clone(),
            degraded: self.degraded,
            subscription: self.snapshot.as_ref().and_then(|snapshot| {
                snapshot
                    .subscription_info()
                    .map(|info| SubscriptionInfoDto {
                        source_name: info.source_host.clone(),
                        downloaded_bytes: info.downloaded_bytes,
                        total_bytes: info.total_bytes,
                        expires_at_unix: info.expires_at_unix,
                        updated_at_unix: info.updated_at_unix,
                        refresh_available: snapshot.subscription_source_url().is_some(),
                    })
            }),
        }
    }

    fn apply_snapshot(&mut self, snapshot: Arc<Snapshot>, controller: Arc<P>, revision: u64) {
        self.controller = Some(controller);
        self.apply_snapshot_metadata(snapshot, revision);
    }

    fn apply_snapshot_without_controller(&mut self, snapshot: Arc<Snapshot>, revision: u64) {
        self.controller = None;
        self.apply_snapshot_metadata(snapshot, revision);
    }

    fn apply_snapshot_metadata(&mut self, snapshot: Arc<Snapshot>, revision: u64) {
        if matches!(
            self.state.current(),
            CoreConnectionState::Empty | CoreConnectionState::Error
        ) {
            let _ = self.state.transition(CoreConnectionState::Ready);
        }
        let selectable = selectable_groups(&snapshot);
        self.profile = selectable.first().map(|group| group.name.to_owned());
        self.selected_nodes = selectable
            .iter()
            .map(|group| group.nodes.first().map(|node| (*node).to_owned()))
            .collect();
        self.current_node = self.selected_nodes.first().cloned().flatten();
        self.catalog = build_catalog(
            &snapshot,
            revision,
            self.profile.as_deref(),
            &self.selected_nodes,
        );
        self.snapshot = Some(snapshot);
        self.message = None;
        self.degraded = false;
    }

    fn mark_error(&mut self, message: &str, degraded: bool) {
        match self.state.current() {
            CoreConnectionState::Ready => {
                let _ = self.state.transition(CoreConnectionState::Empty);
                let _ = self.state.transition(CoreConnectionState::Error);
            }
            CoreConnectionState::Empty
            | CoreConnectionState::Connecting
            | CoreConnectionState::Connected => {
                let _ = self.state.transition(CoreConnectionState::Error);
            }
            CoreConnectionState::Error => {}
        }
        self.message = Some(message.to_owned());
        self.degraded = degraded;
    }

    fn push_event(&mut self, severity: Severity, component: Component, phase: &str, message: &str) {
        self.events.push_with(|id| {
            Event::new(
                unix_timestamp_ms(),
                severity,
                component,
                phase,
                format!("daemon-{id:016x}"),
                message,
                json!({}),
            )
        });
    }
}

impl<C, P, F> CoreBackend<C, P, F>
where
    C: HttpClient,
    P: ProcessController,
    F: Fn(&Snapshot) -> Result<Arc<P>, BackendError> + Send + Sync + 'static,
{
    pub fn new(
        store: PersistentSnapshotStore,
        fetcher: SubscriptionFetcher<C>,
        controller_factory: F,
    ) -> Result<Self, BackendError> {
        Self::new_with_selector(
            store,
            fetcher,
            controller_factory,
            Arc::new(UnavailableSelector),
        )
    }

    pub fn new_with_selector(
        store: PersistentSnapshotStore,
        fetcher: SubscriptionFetcher<C>,
        controller_factory: F,
        selector: Arc<dyn MihomoSelector>,
    ) -> Result<Self, BackendError> {
        let controller_factory: Arc<PreparedFactory<P>> = Arc::new(move |snapshot: &Snapshot| {
            controller_factory(snapshot).map(PreparedController::plain)
        });
        Self::from_prepared_factory(store, fetcher, controller_factory, selector)
    }
}

impl<C, P, F> CoreBackend<C, P, TransactionalControllerFactory<F>>
where
    C: HttpClient,
    P: ProcessController,
    F: Fn(&Snapshot) -> Result<PreparedController<P>, BackendError> + Send + Sync + 'static,
{
    pub fn new_transactional(
        store: PersistentSnapshotStore,
        fetcher: SubscriptionFetcher<C>,
        controller_factory: TransactionalControllerFactory<F>,
    ) -> Result<Self, BackendError> {
        Self::new_transactional_with_selector(
            store,
            fetcher,
            controller_factory,
            Arc::new(UnavailableSelector),
        )
    }

    pub fn new_transactional_with_selector(
        store: PersistentSnapshotStore,
        fetcher: SubscriptionFetcher<C>,
        controller_factory: TransactionalControllerFactory<F>,
        selector: Arc<dyn MihomoSelector>,
    ) -> Result<Self, BackendError> {
        let factory = controller_factory.0;
        let controller_factory: Arc<PreparedFactory<P>> =
            Arc::new(move |snapshot: &Snapshot| factory(snapshot));
        Self::from_prepared_factory(store, fetcher, controller_factory, selector)
    }
}

impl<C, P, F> CoreBackend<C, P, F>
where
    C: HttpClient,
    P: ProcessController,
{
    fn from_prepared_factory(
        store: PersistentSnapshotStore,
        fetcher: SubscriptionFetcher<C>,
        controller_factory: Arc<PreparedFactory<P>>,
        selector: Arc<dyn MihomoSelector>,
    ) -> Result<Self, BackendError> {
        let mut inner = CoreRuntime::empty();
        if let Some((revision, snapshot)) = store.current_with_generation() {
            inner.apply_snapshot_without_controller(snapshot, revision);
        }
        Ok(Self {
            store,
            fetcher,
            controller_factory,
            factory_type: PhantomData,
            selector,
            latency_permits: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_LATENCY_PROBES)),
            operation: tokio::sync::Mutex::new(()),
            inner: RwLock::new(inner),
        })
    }
}

#[async_trait]
impl<C, P, F> Backend for CoreBackend<C, P, F>
where
    C: HttpClient + 'static,
    P: ProcessController + 'static,
    F: 'static,
{
    async fn status(&self) -> Result<StatusDto, BackendError> {
        Ok(self.inner.read().await.status())
    }

    async fn catalog(&self) -> Result<CatalogDto, BackendError> {
        let runtime = self.inner.read().await;
        Ok(runtime.catalog.clone())
    }

    async fn import_subscription(
        &self,
        request: ImportSubscriptionRequest,
    ) -> Result<StatusDto, BackendError> {
        let _operation = self.operation.lock().await;
        {
            let state = self.inner.read().await;
            if matches!(
                state.state.current(),
                CoreConnectionState::Connecting | CoreConnectionState::Connected
            ) || state.degraded
            {
                return Err(BackendError::new());
            }
        }

        let staging = AtomicSnapshot::default();
        let snapshot = match self.fetcher.refresh(&request.url, &staging).await {
            Ok(snapshot) => snapshot,
            Err(_) => {
                let mut state = self.inner.write().await;
                state.mark_error(
                    "Импорт не выполнен. Проверьте ссылку и доступ к сети.",
                    false,
                );
                state.push_event(
                    Severity::Error,
                    Component::Subscription,
                    "import",
                    "Импорт подписки завершился ошибкой.",
                );
                return Err(BackendError::new());
            }
        };
        let prepared = match (self.controller_factory)(&snapshot) {
            Ok(prepared) => prepared,
            Err(error) => {
                let mut state = self.inner.write().await;
                state.mark_error(
                    "Не удалось подготовить конфигурацию запуска. Повторите импорт.",
                    false,
                );
                state.push_event(
                    Severity::Error,
                    Component::Core,
                    "runtime_publish",
                    "Подготовка конфигурации запуска завершилась ошибкой.",
                );
                return Err(error);
            }
        };

        let (revision, snapshot) = match self.store.commit_with_generation((*snapshot).clone()) {
            Ok(persisted) => persisted,
            Err(_) => {
                drop(prepared);
                let mut state = self.inner.write().await;
                if state.snapshot.is_some() {
                    if state.state.current() != CoreConnectionState::Ready {
                        let _ = state.state.transition(CoreConnectionState::Ready);
                    }
                    state.message = Some(
                        "Не удалось сохранить новый профиль. Предыдущий профиль сохранён."
                            .to_owned(),
                    );
                    state.degraded = false;
                } else {
                    state.mark_error(
                        "Не удалось сохранить профиль. Освободите место и повторите импорт.",
                        false,
                    );
                }
                state.push_event(
                    Severity::Error,
                    Component::Core,
                    "snapshot_commit",
                    "Сохранение профиля завершилось ошибкой.",
                );
                return Err(BackendError::new());
            }
        };

        let mut state = self.inner.write().await;
        let controller = prepared.controller();
        state.apply_snapshot(snapshot, controller, revision);
        state.push_event(
            Severity::Info,
            Component::Subscription,
            "import",
            "Подписка импортирована.",
        );
        let status = state.status();
        drop(state);
        prepared.activate()?;
        Ok(status)
    }

    async fn refresh_subscription(&self) -> Result<StatusDto, BackendError> {
        let source_url = {
            let state = self.inner.read().await;
            if matches!(
                state.state.current(),
                CoreConnectionState::Connecting | CoreConnectionState::Connected
            ) || state.degraded
            {
                return Err(BackendError::new());
            }
            state
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.subscription_source_url())
                .map(ToOwned::to_owned)
                .ok_or_else(BackendError::new)?
        };
        let result = self
            .import_subscription(ImportSubscriptionRequest { url: source_url })
            .await;
        if result.is_err() {
            let mut state = self.inner.write().await;
            if state.snapshot.is_some() {
                if state.state.current() != CoreConnectionState::Ready {
                    let _ = state.state.transition(CoreConnectionState::Ready);
                }
                state.message =
                    Some("Не удалось обновить подписку. Предыдущий профиль сохранён.".to_owned());
                state.degraded = false;
                state.push_event(
                    Severity::Warning,
                    Component::Subscription,
                    "refresh",
                    "Обновление подписки не выполнено; предыдущий профиль сохранён.",
                );
            }
        }
        result
    }

    async fn connect(&self) -> Result<StatusDto, BackendError> {
        let _operation = self.operation.lock().await;
        let snapshot = {
            let state = self.inner.read().await;
            if state.degraded {
                return Err(BackendError::new());
            }
            if state.state.current() != CoreConnectionState::Ready {
                return Err(BackendError::new());
            }
            state.snapshot.clone().ok_or_else(BackendError::new)?
        };
        let prepared = match (self.controller_factory)(&snapshot) {
            Ok(prepared) => prepared,
            Err(error) => {
                let mut state = self.inner.write().await;
                state.message =
                    Some("Не удалось подготовить сетевой интерфейс и адреса узлов.".to_owned());
                state.push_event(
                    Severity::Error,
                    Component::Core,
                    "runtime_publish",
                    "Подготовка конфигурации перед подключением завершилась ошибкой.",
                );
                return Err(error);
            }
        };
        let controller = prepared.controller();
        if prepared.activate().is_err() {
            let mut state = self.inner.write().await;
            state.message = Some("Не удалось активировать конфигурацию запуска.".to_owned());
            state.push_event(
                Severity::Error,
                Component::Core,
                "runtime_publish",
                "Активация конфигурации перед подключением завершилась ошибкой.",
            );
            return Err(BackendError::new());
        }
        {
            let mut state = self.inner.write().await;
            state.controller = Some(controller.clone());
            state
                .state
                .transition(CoreConnectionState::Connecting)
                .map_err(|_| BackendError::new())?;
            state.message = Some("Запускается подключение.".to_owned());
            state.push_event(
                Severity::Info,
                Component::Core,
                "connect",
                "Запускается подключение.",
            );
        }

        let result = Supervisor::new(controller).connect().await;
        let mut state = self.inner.write().await;
        match result {
            Ok(()) => {
                state
                    .state
                    .transition(CoreConnectionState::Connected)
                    .map_err(|_| BackendError::new())?;
                state.message = None;
                state.degraded = false;
                state.push_event(
                    Severity::Info,
                    Component::Core,
                    "connect",
                    "Подключение установлено.",
                );
                Ok(state.status())
            }
            Err(multicore_core::SupervisorError::StartFailed {
                rollback_failed, ..
            }) => {
                if rollback_failed {
                    state.mark_error(
                        "Не удалось полностью отменить запуск. Выполните отключение для очистки.",
                        true,
                    );
                } else {
                    state
                        .state
                        .transition(CoreConnectionState::Ready)
                        .map_err(|_| BackendError::new())?;
                    state.message = Some(
                        "Не удалось запустить подключение. Проверьте исполняемые файлы.".to_owned(),
                    );
                    state.degraded = false;
                }
                state.push_event(
                    Severity::Error,
                    Component::Core,
                    "connect",
                    "Запуск подключения завершился ошибкой.",
                );
                Err(BackendError::new())
            }
            Err(multicore_core::SupervisorError::StopFailed) => {
                state.mark_error(
                    "Состояние процессов неизвестно. Выполните отключение для очистки.",
                    true,
                );
                state.push_event(
                    Severity::Error,
                    Component::Core,
                    "connect",
                    "Запуск подключения завершился ошибкой.",
                );
                Err(BackendError::new())
            }
        }
    }

    async fn disconnect(&self) -> Result<StatusDto, BackendError> {
        let _operation = self.operation.lock().await;
        let controller = {
            let state = self.inner.read().await;
            if matches!(
                state.state.current(),
                CoreConnectionState::Empty | CoreConnectionState::Ready
            ) {
                return Ok(state.status());
            }
            state.controller.clone().ok_or_else(BackendError::new)?
        };

        let result = Supervisor::new(controller).disconnect().await;
        let mut state = self.inner.write().await;
        match result {
            Ok(()) => {
                state
                    .state
                    .transition(CoreConnectionState::Ready)
                    .map_err(|_| BackendError::new())?;
                state.message = None;
                state.degraded = false;
                state.push_event(
                    Severity::Info,
                    Component::Core,
                    "disconnect",
                    "Подключение остановлено.",
                );
                Ok(state.status())
            }
            Err(_) => {
                state.mark_error(
                    "Не удалось полностью остановить подключение. Повторите попытку.",
                    true,
                );
                state.push_event(
                    Severity::Error,
                    Component::Core,
                    "disconnect",
                    "Остановка подключения завершилась ошибкой.",
                );
                Err(BackendError::new())
            }
        }
    }

    async fn select(&self, request: SelectionRequest) -> Result<CatalogDto, SelectionError> {
        let _operation = self.operation.lock().await;
        let (snapshot, group_index, raw_group, raw_node) = {
            let state = self.inner.read().await;
            if state.state.current() != CoreConnectionState::Connected || state.degraded {
                return Err(SelectionError::NotConnected);
            }
            if request.revision != state.catalog.revision {
                return Err(SelectionError::StaleRevision);
            }
            let snapshot = state.snapshot.clone().ok_or(SelectionError::UnknownGroup)?;
            let groups = selectable_groups(&snapshot);
            let group_index = groups
                .iter()
                .enumerate()
                .find_map(|(index, _)| {
                    (catalog_group_id(request.revision, index) == request.group_id).then_some(index)
                })
                .ok_or(SelectionError::UnknownGroup)?;
            let group = &groups[group_index];
            let node_index = group
                .nodes
                .iter()
                .enumerate()
                .find_map(|(index, _)| {
                    (catalog_node_id(request.revision, group_index, index) == request.node_id)
                        .then_some(index)
                })
                .ok_or(SelectionError::UnknownNode)?;
            let raw_group = group.name.to_owned();
            let raw_node = group.nodes[node_index].to_owned();
            (snapshot, group_index, raw_group, raw_node)
        };

        self.selector
            .select(&raw_group, &raw_node)
            .await
            .map_err(|_| SelectionError::SelectorFailed)?;

        let mut state = self.inner.write().await;
        // All state-changing operations share `operation`, so this revision cannot change while
        // the controller request is in flight.
        if state.catalog.revision != request.revision {
            return Err(SelectionError::StaleRevision);
        }
        state.selected_nodes[group_index] = Some(raw_node.clone());
        if group_index == 0 {
            state.current_node = Some(raw_node);
        }
        state.catalog = build_catalog(
            &snapshot,
            request.revision,
            state.profile.as_deref(),
            &state.selected_nodes,
        );
        state.push_event(
            Severity::Info,
            Component::Mihomo,
            "selection",
            "Маршрут Mihomo изменён.",
        );
        Ok(state.catalog.clone())
    }

    async fn latencies(&self, request: LatencyRequest) -> Result<LatencyResponseDto, LatencyError> {
        let batch_deadline = tokio::time::Instant::now() + LATENCY_BATCH_TIMEOUT;
        if request
            .group_ids
            .len()
            .saturating_add(request.node_ids.len())
            > MAX_LATENCY_RESULTS
        {
            return Err(LatencyError::TooManyTargets);
        }

        let mut targets = {
            let state = self.inner.read().await;
            if state.state.current() != CoreConnectionState::Connected || state.degraded {
                return Err(LatencyError::NotConnected);
            }
            if request
                .revision
                .is_some_and(|revision| revision != state.catalog.revision)
            {
                return Err(LatencyError::StaleRevision);
            }
            let revision = state.catalog.revision;
            let snapshot = state.snapshot.as_ref().ok_or(LatencyError::UnknownGroup)?;
            let groups = selectable_groups(snapshot);
            let requested_groups: HashSet<&str> =
                request.group_ids.iter().map(String::as_str).collect();
            let requested_nodes: HashSet<&str> =
                request.node_ids.iter().map(String::as_str).collect();

            let known_groups: HashSet<String> = (0..groups.len())
                .map(|group_index| catalog_group_id(revision, group_index))
                .collect();
            if !requested_groups
                .iter()
                .all(|group_id| known_groups.contains(*group_id))
            {
                return Err(LatencyError::UnknownGroup);
            }
            let known_nodes: HashSet<String> = groups
                .iter()
                .enumerate()
                .flat_map(|(group_index, group)| {
                    (0..group.nodes.len())
                        .map(move |node_index| catalog_node_id(revision, group_index, node_index))
                })
                .collect();
            if !requested_nodes
                .iter()
                .all(|node_id| known_nodes.contains(*node_id))
            {
                return Err(LatencyError::UnknownNode);
            }

            let probe_all = requested_groups.is_empty() && requested_nodes.is_empty();
            let mut targets: VecDeque<_> = groups
                .iter()
                .enumerate()
                .flat_map(|(group_index, group)| {
                    let group_id = catalog_group_id(revision, group_index);
                    let group_requested = requested_groups.contains(group_id.as_str());
                    let requested_nodes = &requested_nodes;
                    group
                        .nodes
                        .iter()
                        .enumerate()
                        .filter_map(move |(node_index, raw_node)| {
                            let node_id = catalog_node_id(revision, group_index, node_index);
                            (probe_all
                                || group_requested
                                || requested_nodes.contains(node_id.as_str()))
                            .then(|| (group_id.clone(), node_id, (*raw_node).to_owned()))
                        })
                })
                .collect();
            if targets.len() > MAX_LATENCY_RESULTS {
                if probe_all {
                    targets.truncate(MAX_LATENCY_RESULTS);
                } else {
                    return Err(LatencyError::TooManyTargets);
                }
            }
            targets
        };

        let all_targets: Vec<_> = targets.iter().cloned().collect();
        let mut probes = tokio::task::JoinSet::new();
        let mut next_index = 0_usize;
        while probes.len() < MAX_CONCURRENT_LATENCY_PROBES {
            let Some((group_id, node_id, raw_node)) = targets.pop_front() else {
                break;
            };
            let selector = self.selector.clone();
            let permits = self.latency_permits.clone();
            let index = next_index;
            next_index += 1;
            probes.spawn(probe_latency_target(
                selector, permits, index, group_id, node_id, raw_node,
            ));
        }

        let mut completed = vec![None; all_targets.len()];
        while !probes.is_empty() {
            match tokio::time::timeout_at(batch_deadline, probes.join_next()).await {
                Ok(Some(Ok((index, entry)))) => completed[index] = Some(entry),
                Ok(Some(Err(_))) => {}
                Ok(None) => break,
                Err(_) => {
                    probes.abort_all();
                    break;
                }
            }
            if tokio::time::Instant::now() < batch_deadline
                && let Some((group_id, node_id, raw_node)) = targets.pop_front()
            {
                let selector = self.selector.clone();
                let permits = self.latency_permits.clone();
                let index = next_index;
                next_index += 1;
                probes.spawn(probe_latency_target(
                    selector, permits, index, group_id, node_id, raw_node,
                ));
            }
        }
        let entries = all_targets
            .into_iter()
            .enumerate()
            .map(|(index, (group_id, node_id, _))| {
                completed[index].take().unwrap_or(LatencyResultDto {
                    group_id,
                    node_id,
                    latency_ms: None,
                    status: LatencyStatus::Timeout,
                })
            })
            .collect();
        Ok(LatencyResponseDto { entries })
    }

    async fn events_after(&self, after: u64) -> Result<Vec<EventDto>, BackendError> {
        let state = self.inner.read().await;
        Ok(state
            .events
            .after(after)
            .map(|(id, event)| core_event_to_dto(id, event))
            .collect())
    }

    async fn diagnostics(&self) -> Result<DiagnosticsDto, BackendError> {
        let (controller, snapshot) = {
            let state = self.inner.read().await;
            (state.controller.clone(), state.snapshot.clone())
        };
        let diagnostics = match controller {
            Some(controller) => controller.diagnostics().await,
            None => ProcessDiagnostics::default(),
        };
        let mut dto = process_diagnostics_to_dto(diagnostics);
        if let Some(snapshot) = snapshot {
            dto.mappings = diagnostic_mappings(&snapshot);
        }
        Ok(dto)
    }
}

fn diagnostic_mappings(snapshot: &Snapshot) -> Vec<ProxyMappingDto> {
    snapshot
        .mihomo
        .loopback_socks_mappings()
        .iter()
        .take(MAX_DIAGNOSTIC_MAPPINGS)
        .enumerate()
        .map(|(index, mapping)| {
            let label = safe_catalog_label(&mapping.name, "Proxy", index);
            ProxyMappingDto {
                proxy_name: label.clone(),
                address: format!("127.0.0.1:{}", mapping.port),
                xray_label: label,
            }
        })
        .collect()
}

fn process_diagnostics_to_dto(diagnostics: ProcessDiagnostics) -> DiagnosticsDto {
    DiagnosticsDto {
        xray: runtime_check_to_dto(diagnostics.xray),
        mihomo: runtime_check_to_dto(diagnostics.mihomo),
        tun: runtime_check_to_dto(diagnostics.tun),
        logs: diagnostics
            .logs
            .into_iter()
            .map(|record| DiagnosticLogDto {
                id: record.id,
                timestamp: record.timestamp_ms.to_string(),
                component: match record.engine {
                    multicore_core::Engine::Xray => "xray",
                    multicore_core::Engine::Mihomo => "mihomo",
                }
                .to_owned(),
                level: match record.stream {
                    DiagnosticStream::Stdout => "info",
                    DiagnosticStream::Stderr => "error",
                }
                .to_owned(),
                message: record.message,
            })
            .collect(),
        mappings: Vec::new(),
    }
}

fn runtime_check_to_dto(state: RuntimeCheckState) -> RuntimeCheckDto {
    match state {
        RuntimeCheckState::Stopped => RuntimeCheckDto::Stopped,
        RuntimeCheckState::Starting => RuntimeCheckDto::Starting,
        RuntimeCheckState::Ready => RuntimeCheckDto::Ready,
        RuntimeCheckState::Failed => RuntimeCheckDto::Failed,
        RuntimeCheckState::Unsupported => RuntimeCheckDto::Unsupported,
    }
}

fn map_core_state(state: CoreConnectionState) -> ConnectionState {
    match state {
        CoreConnectionState::Empty => ConnectionState::Empty,
        CoreConnectionState::Ready => ConnectionState::Ready,
        CoreConnectionState::Connecting => ConnectionState::Connecting,
        CoreConnectionState::Connected => ConnectionState::Connected,
        CoreConnectionState::Error => ConnectionState::Error,
    }
}

fn build_catalog(
    snapshot: &Snapshot,
    revision: u64,
    profile: Option<&str>,
    selected_nodes: &[Option<String>],
) -> CatalogDto {
    let source_groups = selectable_groups(snapshot);
    let selected_group_index =
        profile.and_then(|profile| source_groups.iter().position(|group| group.name == profile));
    let groups = source_groups
        .iter()
        .enumerate()
        .map(|(group_index, group)| {
            let selected = selected_group_index == Some(group_index);
            let selected_node_index = selected_nodes
                .get(group_index)
                .and_then(Option::as_deref)
                .and_then(|current_node| group.nodes.iter().position(|node| *node == current_node));
            let nodes = group
                .nodes
                .iter()
                .enumerate()
                .map(|(node_index, node)| NodeDto {
                    id: catalog_node_id(revision, group_index, node_index),
                    label: safe_catalog_label(node, "Узел", node_index),
                    selected: selected_node_index == Some(node_index),
                    delay_ms: None,
                })
                .collect();
            GroupDto {
                id: catalog_group_id(revision, group_index),
                label: safe_catalog_label(group.name, "Группа", group_index),
                selected,
                nodes,
            }
        })
        .collect();
    CatalogDto { revision, groups }
}

struct SelectableGroup<'a> {
    name: &'a str,
    nodes: Vec<&'a str>,
}

/// The exact name-addressable namespace exposed by Mihomo's controller. Only interactive
/// selectors are included, and the first duplicate raw identifier wins.
fn selectable_groups(snapshot: &Snapshot) -> Vec<SelectableGroup<'_>> {
    let mut group_names = HashSet::new();
    snapshot
        .mihomo
        .groups()
        .iter()
        .filter(|group| group_names.insert(group.name.as_str()))
        .filter(|group| group.group_type.eq_ignore_ascii_case("select"))
        .take(MAX_CATALOG_GROUPS)
        .map(|group| {
            let mut node_names = HashSet::new();
            let nodes = group
                .proxies
                .iter()
                .map(String::as_str)
                .filter(|node| node_names.insert(*node))
                .take(MAX_CATALOG_NODES_PER_GROUP)
                .collect();
            SelectableGroup {
                name: &group.name,
                nodes,
            }
        })
        .collect()
}

fn catalog_group_id(revision: u64, group_index: usize) -> String {
    format!("g-{revision:016x}-{group_index:04x}")
}

fn catalog_node_id(revision: u64, group_index: usize, node_index: usize) -> String {
    format!("n-{revision:016x}-{group_index:04x}-{node_index:04x}")
}

/// Accepts only Unicode letters/numbers, ASCII space and this harmless punctuation:
/// `- _ . , ( ) [ ] + # !`. Everything else receives a positional generic label.
fn safe_catalog_label(source: &str, kind: &str, index: usize) -> String {
    // Nothing after this boundary can be published, so no presentation safety scan may spend
    // time or memory proportional to the rest of an untrusted (up to 32 MiB) YAML scalar.
    let published_end = source
        .char_indices()
        .nth(MAX_CATALOG_NAME_CHARS)
        .map_or(source.len(), |(byte_index, _)| byte_index);
    let published = &source[..published_end];
    if published.is_empty()
        || contains_uuid_like(published)
        || contains_structured_credential(published)
        || !published.chars().all(is_safe_catalog_label_char)
    {
        return format!("{kind} {}", index + 1);
    }
    let published = published.trim_matches(' ');
    if published.is_empty() {
        return format!("{kind} {}", index + 1);
    }
    published.to_owned()
}

/// Blocks recognizable credential structures. This intentionally does not claim to infer
/// arbitrary secrets from otherwise ordinary-looking names.
fn contains_structured_credential(source: &str) -> bool {
    let mut previous_was_api = false;
    for word in source
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
    {
        let credential_word = [
            "authorization",
            "bearer",
            "basic",
            "token",
            "password",
            "passwd",
            "secret",
            "apikey",
            "subscription",
        ]
        .iter()
        .any(|credential| word.eq_ignore_ascii_case(credential));
        if credential_word || (previous_was_api && word.eq_ignore_ascii_case("key")) {
            return true;
        }
        previous_was_api = word.eq_ignore_ascii_case("api");
    }

    source
        .split(|character: char| {
            !(character.is_ascii_alphanumeric() || matches!(character, '+' | '/' | '_' | '-'))
        })
        .filter(|run| !run.is_empty())
        .any(base64_run_contains_credential)
}

fn base64_run_contains_credential(run: &str) -> bool {
    scan_base64_run(run).credential
}

#[derive(Debug, Default)]
struct Base64Scan {
    credential: bool,
    decode_attempts: usize,
    source_bytes_visited: usize,
    encoded_bytes_decoded: usize,
    decoded_bytes_visited: usize,
}

fn scan_base64_run(run: &str) -> Base64Scan {
    let mut scan = Base64Scan::default();
    if scan_explicit_base64_candidates(run, &mut scan)
        || scan_phase_aligned_segments(
            run,
            is_standard_base64_byte,
            &general_purpose::STANDARD_NO_PAD,
            &mut scan,
        )
        || scan_phase_aligned_segments(
            run,
            is_url_safe_base64_byte,
            &general_purpose::URL_SAFE_NO_PAD,
            &mut scan,
        )
    {
        scan.credential = true;
    }
    scan
}

fn scan_explicit_base64_candidates(run: &str, scan: &mut Base64Scan) -> bool {
    let standard_compatible = run.bytes().all(is_standard_base64_byte);
    scan.source_bytes_visited += run.len();
    let url_compatible = run.bytes().all(is_url_safe_base64_byte);
    scan.source_bytes_visited += run.len();

    if standard_compatible {
        if decode_candidate(run, &general_purpose::STANDARD_NO_PAD, true, scan) {
            return true;
        }
    } else if url_compatible && decode_candidate(run, &general_purpose::URL_SAFE_NO_PAD, true, scan)
    {
        return true;
    }

    // These characters are explicit presentation separators even though each belongs to one
    // Base64 alphabet. Checking the maximal alphanumeric components preserves standalone
    // short credentials such as `YTpi` without accepting arbitrary interior suffixes.
    scan.source_bytes_visited += run.len();
    run.split(['-', '_', '+', '/'])
        .filter(|candidate| !candidate.is_empty() && candidate.len() != run.len())
        .any(|candidate| decode_candidate(candidate, &general_purpose::STANDARD_NO_PAD, true, scan))
}

fn is_standard_base64_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/')
}

fn is_url_safe_base64_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')
}

fn scan_phase_aligned_segments(
    run: &str,
    accepts: fn(u8) -> bool,
    engine: &general_purpose::GeneralPurpose,
    scan: &mut Base64Scan,
) -> bool {
    // An embedded Base64 token can start in only one of four source phases. Decode each phase
    // once per maximal alphabet-compatible segment, then inspect the decoded stream. Across
    // all segments this visits/decodes only a constant multiple of the capped source length.
    scan.source_bytes_visited += run.len();
    let bytes = run.as_bytes();
    let mut segment_start = 0;
    while segment_start < bytes.len() {
        while segment_start < bytes.len() && !accepts(bytes[segment_start]) {
            segment_start += 1;
        }
        let mut segment_end = segment_start;
        while segment_end < bytes.len() && accepts(bytes[segment_end]) {
            segment_end += 1;
        }
        for phase in 0..4.min(segment_end.saturating_sub(segment_start)) {
            let start = segment_start + phase;
            let mut end = segment_end;
            if (end - start) % 4 == 1 {
                end -= 1;
            }
            if end - start >= 4 && decode_candidate(&run[start..end], engine, false, scan) {
                return true;
            }
        }
        segment_start = segment_end.saturating_add(1);
    }
    false
}

fn decode_candidate(
    candidate: &str,
    engine: &general_purpose::GeneralPurpose,
    allow_short_pair: bool,
    scan: &mut Base64Scan,
) -> bool {
    if candidate.len() < 4 || candidate.len() > MAX_CATALOG_NAME_CHARS || candidate.len() % 4 == 1 {
        return false;
    }
    let mut decoded = [0_u8; MAX_CATALOG_NAME_CHARS];
    scan.decode_attempts += 1;
    scan.encoded_bytes_decoded += candidate.len();
    let Ok(decoded_len) = engine.decode_slice(candidate, &mut decoded) else {
        return false;
    };
    scan.decoded_bytes_visited += decoded_len;
    decoded_contains_credential(&decoded[..decoded_len], allow_short_pair)
}

fn decoded_contains_credential(decoded: &[u8], allow_short_pair: bool) -> bool {
    let entire_record_is_printable = decoded.iter().all(|byte| (b' '..=b'~').contains(byte));
    let semantic_credential = decoded
        .split(|byte| !byte.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .any(|word| {
            [
                b"authorization".as_slice(),
                b"bearer",
                b"basic",
                b"token",
                b"password",
                b"passwd",
                b"secret",
                b"apikey",
                b"subscription",
            ]
            .iter()
            .any(|credential| word.eq_ignore_ascii_case(credential))
        });
    if semantic_credential {
        return true;
    }

    decoded.iter().enumerate().any(|(colon, byte)| {
        if *byte != b':' {
            return false;
        }
        let left_len = decoded[..colon]
            .iter()
            .rev()
            .take_while(|byte| byte.is_ascii_graphic() && **byte != b':')
            .count();
        let right_len = decoded[colon + 1..]
            .iter()
            .take_while(|byte| byte.is_ascii_graphic() && **byte != b':')
            .count();
        (left_len > 0 && right_len > 0 && allow_short_pair && entire_record_is_printable)
            || (left_len >= 3 && right_len >= 3)
            || (left_len >= 2
                && right_len >= 2
                && decoded[colon - left_len..colon]
                    .iter()
                    .chain(&decoded[colon + 1..colon + 1 + right_len])
                    .any(u8::is_ascii_alphanumeric))
    })
}

fn is_safe_catalog_label_char(character: char) -> bool {
    if character.is_control() {
        return false;
    }
    character.is_alphanumeric()
        || character == ' '
        || matches!(
            character,
            '-' | '_'
                | '.'
                | ','
                | '('
                | ')'
                | '['
                | ']'
                | '+'
                | '#'
                | '!'
                | '\u{2190}'..='\u{21ff}'
                | '\u{2600}'..='\u{27bf}'
                | '\u{1f1e6}'..='\u{1f1ff}'
                | '\u{1f300}'..='\u{1faff}'
                | '\u{fe0f}'
        )
}

fn contains_uuid_like(source: &str) -> bool {
    source.as_bytes().windows(36).any(|candidate| {
        candidate.iter().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
    })
}

fn core_event_to_dto(id: u64, event: &Event) -> EventDto {
    let value = serde_json::to_value(event).unwrap_or_default();
    EventDto {
        id,
        timestamp: value
            .get("timestamp_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default()
            .to_string(),
        level: value
            .get("severity")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("error")
            .to_owned(),
        message: value
            .get("safe_message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Событие недоступно.")
            .to_owned(),
    }
}

fn unix_timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[derive(Default)]
pub struct MemoryBackend {
    inner: RwLock<MemoryState>,
}

#[derive(Default)]
struct MemoryState {
    connection: ConnectionState,
    subscriptions: u64,
    events: EventBuffer<EventDto>,
}

impl MemoryState {
    fn status(&self) -> StatusDto {
        let has_profile = self.subscriptions > 0;
        StatusDto {
            state: self.connection,
            profile: has_profile.then(|| "Основной профиль".to_owned()),
            current_node: has_profile.then(|| "Авто".to_owned()),
            message: None,
            degraded: false,
            subscription: None,
        }
    }

    fn push_event(&mut self, level: &str, message: &str) {
        self.events.push_with(|id| EventDto {
            id,
            timestamp: id.to_string(),
            level: level.to_owned(),
            message: message.to_owned(),
        });
    }
}

#[async_trait]
impl Backend for MemoryBackend {
    async fn status(&self) -> Result<StatusDto, BackendError> {
        Ok(self.inner.read().await.status())
    }

    async fn catalog(&self) -> Result<CatalogDto, BackendError> {
        Ok(CatalogDto {
            revision: 0,
            groups: Vec::new(),
        })
    }

    async fn import_subscription(
        &self,
        _request: ImportSubscriptionRequest,
    ) -> Result<StatusDto, BackendError> {
        let mut state = self.inner.write().await;
        state.subscriptions += 1;
        state.connection = ConnectionState::Ready;
        state.push_event("info", "Подписка импортирована.");
        Ok(state.status())
    }

    async fn connect(&self) -> Result<StatusDto, BackendError> {
        let mut state = self.inner.write().await;
        state.connection = ConnectionState::Connected;
        state.push_event("info", "Подключение установлено.");
        Ok(state.status())
    }

    async fn disconnect(&self) -> Result<StatusDto, BackendError> {
        let mut state = self.inner.write().await;
        state.connection = if state.subscriptions > 0 {
            ConnectionState::Ready
        } else {
            ConnectionState::Empty
        };
        state.push_event("info", "Подключение остановлено.");
        Ok(state.status())
    }

    async fn events_after(&self, after: u64) -> Result<Vec<EventDto>, BackendError> {
        let state = self.inner.read().await;
        Ok(state
            .events
            .after(after)
            .map(|(_, event)| event.clone())
            .collect())
    }

    async fn diagnostics(&self) -> Result<DiagnosticsDto, BackendError> {
        Ok(DiagnosticsDto::default())
    }
}

#[cfg(test)]
mod presentation_safety_tests {
    use super::{MAX_CATALOG_NAME_CHARS, scan_base64_run};

    #[test]
    fn base64_scanner_has_linear_attempt_and_total_byte_bounds() {
        let solid = "A".repeat(MAX_CATALOG_NAME_CHARS);
        let mixed = "AAAA_AAAA+AAAA-AAAA/"
            .repeat(MAX_CATALOG_NAME_CHARS / 20 + 1)
            .chars()
            .take(MAX_CATALOG_NAME_CHARS)
            .collect::<String>();
        for adversarial in [solid, mixed] {
            let scan = scan_base64_run(&adversarial);
            assert!(!scan.credential);
            assert!(
                scan.decode_attempts <= MAX_CATALOG_NAME_CHARS * 4,
                "{} decoder attempts exceeded the linear bound",
                scan.decode_attempts
            );
            let total_bytes =
                scan.source_bytes_visited + scan.encoded_bytes_decoded + scan.decoded_bytes_visited;
            assert!(
                total_bytes <= MAX_CATALOG_NAME_CHARS * 24,
                "{total_bytes} total visited/decoded bytes exceeded the linear bound"
            );
        }
    }
}
