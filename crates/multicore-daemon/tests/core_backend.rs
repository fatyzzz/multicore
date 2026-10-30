use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use multicore_core::{
    CoreLogRecord, DiagnosticStream, Engine, FetchError, HttpClient, HttpResponse, MAX_PROFILES,
    MihomoRuntimeControl, ProcessController, ProcessDiagnostics, ProfileId, ProfileStore,
    RuntimeCheckState, Snapshot, StartFailureCause, SubscriptionFetcher, UA_MIHOMO, UA_NATIVE,
    UA_SERVICE_LOGO, UA_XRAY, stage_runtime_with_mihomo_control,
};
use multicore_daemon::{
    Backend, BackendError, ConnectionState, CoreBackend, ImportSubscriptionRequest,
    MAX_DIAGNOSTIC_MAPPINGS, MihomoSelector, PreparedController, ProfileMutationError,
    RuntimeCheckDto, SelectionRequest, SelectorError, transactional_controller_factory,
};

const MIHOMO: &str = r#"
proxies:
  - name: Test Node
    type: socks5
proxy-groups:
  - name: Main
    type: select
    proxies: [Test Node]
"#;

const REFRESH_SELECTION_MIHOMO: &str = r#"
proxies:
  - {name: First, type: socks5}
  - {name: Second, type: socks5}
proxy-groups:
  - {name: Main, type: select, proxies: [First, Second]}
"#;

struct ConfirmingSelector;

#[async_trait]
impl MihomoSelector for ConfirmingSelector {
    async fn select(&self, _group: &str, _node: &str) -> Result<(), SelectorError> {
        Ok(())
    }
}

struct BlockingStartController {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl ProcessController for BlockingStartController {
    type Error = ();

    fn start<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async move {
            if engine == Engine::Xray {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Ok(())
        })
    }

    fn stop<'a>(
        &'a self,
        _engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

fn test_backend<C, P, F>(
    store: ProfileStore,
    fetcher: SubscriptionFetcher<C>,
    controller_factory: F,
) -> Result<CoreBackend<C, P, F>, BackendError>
where
    C: HttpClient,
    P: ProcessController,
    F: Fn(&Snapshot) -> Result<Arc<P>, BackendError> + Send + Sync + 'static,
{
    CoreBackend::new_with_selector(
        store,
        fetcher,
        controller_factory,
        Arc::new(ConfirmingSelector),
    )
}

const SOURCE_URL_CANARY: &str =
    "https://user:source-password@source-canary.invalid/sub?source_token=source-canary";
const HOME_URL_CANARY: &str = "https://home-url-canary.invalid/private-home";
const SUPPORT_URL_CANARY: &str = "tg://resolve?domain=support_url_canary";
const ANNOUNCEMENT_URL_CANARY: &str =
    "https://announcement-url-canary.invalid/news?action_token=announcement-canary";
const LOGO_URL_CANARY: &str = "https://logo-url-canary.invalid/private-logo.png";
const RAW_HEADER_NAME_CANARY: &str = "x-raw-header-name-canary";
const RAW_HEADER_VALUE_CANARY: &str = "raw-header-value-canary";
const MIHOMO_BODY_CANARY: &str = "mihomo-body-canary.invalid";
const XRAY_BODY_CANARY: &str = "xray-body-canary";

#[derive(Clone, Copy)]
enum FixtureMetadata {
    Standard,
    Maximal,
    Hostile,
}

#[derive(Clone)]
struct FixtureHttp {
    calls: Arc<Mutex<Vec<&'static str>>>,
    mihomo: String,
    xray: String,
    metadata: FixtureMetadata,
    include_logo: Arc<std::sync::atomic::AtomicBool>,
    logo_download_fails: Arc<std::sync::atomic::AtomicBool>,
}

impl Default for FixtureHttp {
    fn default() -> Self {
        Self {
            calls: Arc::default(),
            mihomo: MIHOMO.to_owned(),
            xray: "{}".to_owned(),
            metadata: FixtureMetadata::Standard,
            include_logo: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            logo_download_fails: Arc::default(),
        }
    }
}

impl FixtureHttp {
    fn with_mihomo(mihomo: &'static str) -> Self {
        Self {
            calls: Arc::default(),
            mihomo: mihomo.to_owned(),
            xray: "{}".to_owned(),
            metadata: FixtureMetadata::Standard,
            include_logo: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            logo_download_fails: Arc::default(),
        }
    }

    fn maximal(mihomo: String, xray: String) -> Self {
        Self {
            calls: Arc::default(),
            mihomo,
            xray,
            metadata: FixtureMetadata::Maximal,
            include_logo: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            logo_download_fails: Arc::default(),
        }
    }

    fn hostile() -> Self {
        Self {
            metadata: FixtureMetadata::Hostile,
            ..Self::default()
        }
    }

    fn hostile_with_logo_toggle() -> (Self, Arc<std::sync::atomic::AtomicBool>) {
        let http = Self::hostile();
        (http.clone(), http.include_logo)
    }
}

fn maximal_fixture() -> (FixtureHttp, String, String, String) {
    let display_name = "Д".repeat(128);
    let group_name = "Г".repeat(256);
    let node_name = "У".repeat(256);
    let mihomo = format!(
        "proxies:\n  - name: '{node_name}'\n    type: socks5\n    server: {MIHOMO_BODY_CANARY}\n    port: 443\nproxy-groups:\n  - name: '{group_name}'\n    type: select\n    proxies: ['{node_name}']\n"
    );
    let xray = format!(r#"{{"private_config_marker":"{XRAY_BODY_CANARY}"}}"#);
    (
        FixtureHttp::maximal(mihomo, xray),
        display_name,
        group_name,
        node_name,
    )
}

fn metadata_headers(metadata: FixtureMetadata) -> Vec<(String, String)> {
    match metadata {
        FixtureMetadata::Standard => vec![
            (
                "subscription-userinfo".into(),
                "upload=18446744073709551615; download=938375741110; total=0; expire=1792851157"
                    .into(),
            ),
            ("profile-title".into(), "Provider Plus".into()),
            ("profile-update-interval".into(), "6".into()),
            (
                "announce".into(),
                "  Planned   maintenance tonight  ".into(),
            ),
            ("sub-info-button-text".into(), "Read more".into()),
            ("sub-info-color".into(), "green".into()),
            (
                "profile-web-page-url".into(),
                "https://provider.example/home".into(),
            ),
            (
                "support-url".into(),
                "tg://resolve?domain=provider_help".into(),
            ),
            (
                "announce-url".into(),
                "https://provider.example/news?token=hidden".into(),
            ),
            (
                "flclashx-servicelogo".into(),
                "https://provider.example/logo.png".into(),
            ),
        ],
        FixtureMetadata::Maximal => vec![
            (
                "subscription-userinfo".into(),
                format!(
                    "upload={}; download=938375741110; total=18446744073709551615; expire=1792851157; ignored={RAW_HEADER_VALUE_CANARY}",
                    u64::MAX
                ),
            ),
            ("profile-title".into(), "Д".repeat(128)),
            ("profile-update-interval".into(), "720".into()),
            ("announce".into(), "А".repeat(512)),
            ("sub-info-button-text".into(), "К".repeat(32)),
            ("sub-info-color".into(), "green".into()),
            ("profile-web-page-url".into(), HOME_URL_CANARY.into()),
            ("support-url".into(), SUPPORT_URL_CANARY.into()),
            ("announce-url".into(), ANNOUNCEMENT_URL_CANARY.into()),
            ("flclashx-servicelogo".into(), LOGO_URL_CANARY.into()),
            (
                RAW_HEADER_NAME_CANARY.into(),
                RAW_HEADER_VALUE_CANARY.into(),
            ),
        ],
        FixtureMetadata::Hostile => vec![
            (
                "subscription-userinfo".into(),
                "upload=3; download=4; total=20; expire=1792851157".into(),
            ),
            (
                "profile-title".into(),
                "Bearer token=password https://provider.invalid/raw-config".into(),
            ),
            (
                "flclashx-servicelogo".into(),
                "https://provider.invalid/private-logo.png".into(),
            ),
        ],
    }
}

impl HttpClient for FixtureHttp {
    fn get<'a>(
        &'a self,
        _url: &'a str,
        user_agent: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, FetchError>> + Send + 'a>> {
        self.calls.lock().unwrap().push(user_agent);
        let mihomo = self.mihomo.clone();
        let xray = self.xray.clone();
        let metadata = self.metadata;
        let include_logo = self.include_logo.load(std::sync::atomic::Ordering::SeqCst);
        let logo_download_fails = self
            .logo_download_fails
            .load(std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move {
            let response = match user_agent {
                UA_NATIVE => HttpResponse::new(
                    200,
                    format!("[{},{}]", serde_json::to_string(&mihomo).unwrap(), xray).into_bytes(),
                    metadata_headers(metadata).into_iter().filter(|(name, _)| {
                        include_logo || !name.eq_ignore_ascii_case("flclashx-servicelogo")
                    }),
                ),
                UA_MIHOMO => HttpResponse::new(
                    200,
                    mihomo.as_bytes().to_vec(),
                    [(
                        "subscription-userinfo",
                        "upload=0; download=938375741110; total=0; expire=1792851157",
                    )],
                ),
                UA_XRAY => {
                    HttpResponse::new(200, xray.into_bytes(), std::iter::empty::<(&str, &str)>())
                }
                UA_SERVICE_LOGO if logo_download_fails => return Err(FetchError::Network),
                UA_SERVICE_LOGO => HttpResponse::new(
                    200,
                    b"<svg xmlns='http://www.w3.org/2000/svg' width='16' height='16'><circle cx='8' cy='8' r='8'/></svg>".to_vec(),
                    std::iter::empty::<(&str, &str)>(),
                ),
                _ => return Err(FetchError::Network),
            };
            Ok(response)
        })
    }
}

#[derive(Clone, Copy)]
struct OfflineHttp;

impl HttpClient for OfflineHttp {
    fn get<'a>(
        &'a self,
        _url: &'a str,
        _user_agent: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, FetchError>> + Send + 'a>> {
        Box::pin(async { Err(FetchError::Network) })
    }
}

#[derive(Clone, Default)]
struct ProfileHttp {
    update_one: Arc<std::sync::atomic::AtomicBool>,
}

impl HttpClient for ProfileHttp {
    fn get<'a>(
        &'a self,
        url: &'a str,
        user_agent: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, FetchError>> + Send + 'a>> {
        Box::pin(async move {
            let label = if url.contains("one.invalid") {
                if self.update_one.load(std::sync::atomic::Ordering::SeqCst) {
                    "One Updated"
                } else {
                    "One"
                }
            } else {
                "Two"
            };
            let mihomo = format!(
                "proxies:\n  - name: '{label} Node'\n    type: socks5\n  - name: '{label} Alt'\n    type: socks5\nproxy-groups:\n  - name: '{label}'\n    type: select\n    proxies: ['{label} Node', '{label} Alt']\n"
            );
            match user_agent {
                UA_NATIVE => Ok(HttpResponse::new(
                    200,
                    format!("[{},{{}}]", serde_json::to_string(&mihomo).unwrap()).into_bytes(),
                    std::iter::empty::<(&str, &str)>(),
                )),
                UA_MIHOMO => Ok(HttpResponse::new(
                    200,
                    mihomo.into_bytes(),
                    std::iter::empty::<(&str, &str)>(),
                )),
                UA_XRAY => Ok(HttpResponse::new(
                    200,
                    b"{}".to_vec(),
                    std::iter::empty::<(&str, &str)>(),
                )),
                _ => Err(FetchError::Network),
            }
        })
    }
}

fn profile_id(value: &str) -> ProfileId {
    ProfileId::parse(value).unwrap()
}

fn profile_backend(
    root: &std::path::Path,
) -> CoreBackend<
    ProfileHttp,
    RecordingController,
    impl Fn(&Snapshot) -> Result<Arc<RecordingController>, BackendError> + use<>,
> {
    profile_backend_with_http(root, ProfileHttp::default())
}

fn profile_backend_with_http(
    root: &std::path::Path,
    http: ProfileHttp,
) -> CoreBackend<
    ProfileHttp,
    RecordingController,
    impl Fn(&Snapshot) -> Result<Arc<RecordingController>, BackendError> + use<>,
> {
    test_backend(
        ProfileStore::open(root, None).unwrap(),
        SubscriptionFetcher::new(http),
        |_snapshot| Ok(Arc::new(RecordingController::default())),
    )
    .unwrap()
}

#[tokio::test]
async fn profiles_stay_isolated_duplicate_urls_reuse_and_inactive_refresh_does_not_activate() {
    let root = tempfile::tempdir().unwrap();
    let http = ProfileHttp::default();
    let backend = profile_backend_with_http(root.path(), http.clone());
    let first = backend
        .import_subscription(ImportSubscriptionRequest {
            url: "  https://one.invalid/sub  ".into(),
        })
        .await
        .unwrap();
    let first_id = first.active_profile_id.unwrap();
    assert_eq!(first.profile.as_deref(), Some("One"));
    let catalog = backend.catalog().await.unwrap();
    backend
        .select(SelectionRequest {
            revision: catalog.revision,
            group_id: catalog.groups[0].id.clone(),
            node_id: catalog.groups[0].nodes[1].id.clone(),
        })
        .await
        .unwrap();
    let duplicate = backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://one.invalid/sub".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        duplicate.active_profile_id.as_deref(),
        Some(first_id.as_str())
    );
    assert_eq!(duplicate.current_node.as_deref(), Some("One Alt"));
    let first_revision = backend.catalog().await.unwrap().revision;
    assert_eq!(backend.list_subscriptions().unwrap().len(), 1);

    let second = backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://two.invalid/sub".into(),
        })
        .await
        .unwrap();
    let second_id = second.active_profile_id.unwrap();
    assert_ne!(first_id, second_id);
    assert_eq!(second.profile.as_deref(), Some("Two"));
    http.update_one
        .store(true, std::sync::atomic::Ordering::SeqCst);
    backend
        .refresh_subscription_profile(&profile_id(&first_id))
        .await
        .unwrap();
    let status = backend.status().await.unwrap();
    assert_eq!(
        status.active_profile_id.as_deref(),
        Some(second_id.as_str())
    );
    assert_eq!(status.profile.as_deref(), Some("Two"));
    let activated = backend
        .activate_subscription(&profile_id(&first_id))
        .await
        .unwrap();
    assert_eq!(activated.profile.as_deref(), Some("One Updated"));
    assert_ne!(backend.catalog().await.unwrap().revision, first_revision);
}

#[tokio::test]
async fn source_replacement_preserves_profile_identity_and_active_profile() {
    let root = tempfile::tempdir().unwrap();
    let backend = profile_backend(root.path());
    let first_id = backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://one.invalid/sub".into(),
        })
        .await
        .unwrap()
        .active_profile_id
        .unwrap();
    let second_id = backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://two.invalid/sub".into(),
        })
        .await
        .unwrap()
        .active_profile_id
        .unwrap();

    let profiles = backend
        .replace_subscription_source(
            profile_id(&first_id),
            ImportSubscriptionRequest {
                url: "  https://replacement.invalid/sub?token=secret  ".into(),
            },
        )
        .await
        .unwrap();

    assert_eq!(
        profiles.active_profile_id.as_deref(),
        Some(second_id.as_str())
    );
    assert_eq!(profiles.profiles[0].id, first_id);
    assert_eq!(profiles.profiles[0].display_name, "replacement.invalid");
    drop(backend);
    let store = ProfileStore::open(root.path(), None).unwrap();
    let (_, snapshot) = store.profile_snapshot(&profile_id(&first_id)).unwrap();
    assert_eq!(
        snapshot.subscription_source_url(),
        Some("https://replacement.invalid/sub?token=secret")
    );
}

#[tokio::test]
async fn committed_source_replacement_fault_is_reported_as_ambiguous_and_persists_new_source() {
    let root = tempfile::tempdir().unwrap();
    let backend = profile_backend(root.path());
    let id = backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://one.invalid/sub".into(),
        })
        .await
        .unwrap()
        .active_profile_id
        .unwrap();
    backend.inject_post_commit_load_fault_for_testing();

    let error = backend
        .replace_subscription_source(
            profile_id(&id),
            ImportSubscriptionRequest {
                url: "https://replacement.invalid/sub".into(),
            },
        )
        .await
        .unwrap_err();

    assert_eq!(
        error.profile_mutation(),
        Some(ProfileMutationError::Committed)
    );
    drop(backend);
    let store = ProfileStore::open(root.path(), None).unwrap();
    let (_, snapshot) = store.profile_snapshot(&profile_id(&id)).unwrap();
    assert_eq!(
        snapshot.subscription_source_url(),
        Some("https://replacement.invalid/sub")
    );
}

#[tokio::test]
async fn active_deletion_selects_next_then_empty_and_active_id_survives_restart() {
    let root = tempfile::tempdir().unwrap();
    let backend = profile_backend(root.path());
    let first_id = backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://one.invalid/sub".into(),
        })
        .await
        .unwrap()
        .active_profile_id
        .unwrap();
    let second_id = backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://two.invalid/sub".into(),
        })
        .await
        .unwrap()
        .active_profile_id
        .unwrap();
    drop(backend);

    let backend = profile_backend(root.path());
    assert_eq!(
        backend.status().await.unwrap().active_profile_id.as_deref(),
        Some(second_id.as_str())
    );
    let next = backend
        .delete_subscription(&profile_id(&second_id))
        .await
        .unwrap();
    assert_eq!(next.active_profile_id.as_deref(), Some(first_id.as_str()));
    assert_eq!(next.profile.as_deref(), Some("One"));
    let empty = backend
        .delete_subscription(&profile_id(&first_id))
        .await
        .unwrap();
    assert_eq!(empty.state, ConnectionState::Empty);
    assert_eq!(empty.active_profile_id, None);
    assert!(backend.list_subscriptions().unwrap().is_empty());
}

#[tokio::test]
async fn unavailable_active_profile_keeps_authoritative_id_and_another_can_be_activated() {
    let root = tempfile::tempdir().unwrap();
    let backend = profile_backend(root.path());
    let first_id = backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://one.invalid/sub".into(),
        })
        .await
        .unwrap()
        .active_profile_id
        .unwrap();
    let second_id = backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://two.invalid/sub".into(),
        })
        .await
        .unwrap()
        .active_profile_id
        .unwrap();
    drop(backend);
    std::fs::remove_dir_all(root.path().join(&second_id)).unwrap();

    let backend = profile_backend(root.path());
    let unavailable = backend.status().await.unwrap();
    assert_eq!(unavailable.state, ConnectionState::Empty);
    assert_eq!(
        unavailable.active_profile_id.as_deref(),
        Some(second_id.as_str())
    );
    let restored = backend
        .activate_subscription(&profile_id(&first_id))
        .await
        .unwrap();
    assert_eq!(
        restored.active_profile_id.as_deref(),
        Some(first_id.as_str())
    );
    assert_eq!(restored.profile.as_deref(), Some("One"));
}

#[tokio::test]
async fn connected_refresh_and_delete_are_safe_and_activation_stays_blocked() {
    let root = tempfile::tempdir().unwrap();
    let backend = profile_backend(root.path());
    let first_id = backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://one.invalid/sub".into(),
        })
        .await
        .unwrap()
        .active_profile_id
        .unwrap();
    let second_id = backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://two.invalid/sub".into(),
        })
        .await
        .unwrap()
        .active_profile_id
        .unwrap();
    backend.connect().await.unwrap();
    assert_eq!(
        backend.activate_subscription(&profile_id(&first_id)).await,
        Err(ProfileMutationError::Connected)
    );

    let inactive_refresh = backend
        .refresh_subscription_profile(&profile_id(&first_id))
        .await
        .unwrap();
    assert_eq!(inactive_refresh.state, ConnectionState::Connected);
    let inactive_delete = backend
        .delete_subscription(&profile_id(&first_id))
        .await
        .unwrap();
    assert_eq!(inactive_delete.state, ConnectionState::Connected);
    assert_eq!(
        inactive_delete.active_profile_id.as_deref(),
        Some(second_id.as_str())
    );

    let active_refresh = backend
        .refresh_subscription_profile(&profile_id(&second_id))
        .await
        .unwrap();
    assert_eq!(active_refresh.state, ConnectionState::Connected);
    let home_refresh = Backend::refresh_subscription(&backend).await.unwrap();
    assert_eq!(home_refresh.state, ConnectionState::Connected);
    let active_delete = backend
        .delete_subscription(&profile_id(&second_id))
        .await
        .unwrap();
    assert_eq!(active_delete.state, ConnectionState::Empty);
}

#[tokio::test]
async fn connecting_profile_mutations_reject_immediately_without_waiting_for_operation_lock() {
    let root = tempfile::tempdir().unwrap();
    let store = ProfileStore::open(root.path(), None).unwrap();
    let id = store
        .create_profile(Arc::new(
            Snapshot::parse(MIHOMO.as_bytes(), br#"{}"#).unwrap(),
        ))
        .unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let entered_wait = entered.notified();
    let controller = Arc::new(BlockingStartController {
        entered: entered.clone(),
        release: release.clone(),
    });
    let backend = Arc::new(
        CoreBackend::new_with_selector(
            store,
            SubscriptionFetcher::new(OfflineHttp),
            move |_snapshot| Ok(controller.clone()),
            Arc::new(ConfirmingSelector),
        )
        .unwrap(),
    );
    let connecting = {
        let backend = backend.clone();
        tokio::spawn(async move { backend.connect().await })
    };
    entered_wait.await;

    let activation = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        backend.activate_subscription(&id),
    )
    .await
    .expect("activation must not wait for the operation mutex");
    assert_eq!(activation, Err(ProfileMutationError::Connected));
    let deletion = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        backend.delete_subscription(&id),
    )
    .await
    .expect("deletion must not wait for the operation mutex");
    assert_eq!(deletion, Err(ProfileMutationError::Connected));

    release.notify_one();
    connecting.await.unwrap().unwrap();
}

#[tokio::test]
async fn abandoned_connect_does_not_leave_the_state_connecting() {
    let root = tempfile::tempdir().unwrap();
    let store = ProfileStore::open(root.path(), None).unwrap();
    store
        .create_profile(Arc::new(
            Snapshot::parse(MIHOMO.as_bytes(), br#"{}"#).unwrap(),
        ))
        .unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let entered_wait = entered.notified();
    let controller = Arc::new(BlockingStartController {
        entered: entered.clone(),
        release: Arc::new(tokio::sync::Notify::new()),
    });
    let backend = Arc::new(
        CoreBackend::new_with_selector(
            store,
            SubscriptionFetcher::new(OfflineHttp),
            move |_snapshot| Ok(controller.clone()),
            Arc::new(ConfirmingSelector),
        )
        .unwrap(),
    );
    let connecting = {
        let backend = backend.clone();
        tokio::spawn(async move { backend.connect().await })
    };
    entered_wait.await;
    assert_eq!(
        backend.status().await.unwrap().state,
        ConnectionState::Connecting
    );

    // The HTTP layer drops the handler future when the client goes away.
    connecting.abort();
    assert!(connecting.await.unwrap_err().is_cancelled());

    let status = backend.status().await.unwrap();
    assert_eq!(status.state, ConnectionState::Error);
    assert!(status.degraded);
    let cleaned = backend.disconnect().await.unwrap();
    assert_eq!(cleaned.state, ConnectionState::Ready);
    assert!(!cleaned.degraded);
}

#[tokio::test]
async fn profile_limit_is_reported_as_a_typed_mutation_error() {
    let root = tempfile::tempdir().unwrap();
    let store = ProfileStore::open(root.path(), None).unwrap();
    let snapshot = Arc::new(Snapshot::parse(MIHOMO.as_bytes(), br#"{}"#).unwrap());
    for _ in 0..MAX_PROFILES {
        store.create_profile(snapshot.clone()).unwrap();
    }
    let backend = test_backend(
        store,
        SubscriptionFetcher::new(ProfileHttp::default()),
        |_snapshot| Ok(Arc::new(RecordingController::default())),
    )
    .unwrap();
    assert_eq!(
        backend
            .import_subscription_profile(ImportSubscriptionRequest {
                url: "https://one.invalid/new".into(),
            })
            .await,
        Err(ProfileMutationError::LimitReached)
    );
    assert_eq!(backend.list_subscriptions().unwrap().len(), MAX_PROFILES);

    // Regression coverage for a real bug report: a genuine subscription-add
    // failure on a user's machine produced a bare HTTP 500 "backend_error"
    // with a *completely empty* daemon event log, because every path that
    // collapses into ProfileMutationError::Storage (and this LimitReached
    // path too, since it shares the same create_profile failure branch)
    // used to return silently with nothing logged at all.
    let events = backend.events_after(0).await.unwrap();
    assert!(
        events.iter().any(|event| {
            event
                .details
                .as_ref()
                .and_then(|details| details.get("phase"))
                .and_then(serde_json::Value::as_str)
                == Some("import_create_failed")
        }),
        "{events:?}"
    );
}

#[tokio::test]
#[cfg_attr(
    not(any(target_os = "linux", target_os = "android", windows)),
    ignore = "the logo path comes from /proc/<pid>/fd, which this platform lacks"
)]
async fn imported_source_exposes_safe_metadata_and_can_refresh_without_resubmitting_url() {
    let root = tempfile::tempdir().unwrap();
    let store = ProfileStore::open(root.path(), None).unwrap();
    let (http, expected_display_name, expected_group_name, expected_node_name) = maximal_fixture();
    let calls = http.calls.clone();
    let backend = test_backend(store, SubscriptionFetcher::new(http), |_snapshot| {
        Ok(Arc::new(RecordingController::default()))
    })
    .unwrap();

    let imported = backend
        .import_subscription(ImportSubscriptionRequest {
            url: SOURCE_URL_CANARY.into(),
        })
        .await
        .unwrap();
    assert_eq!(
        imported.profile.as_deref(),
        Some(expected_group_name.as_str())
    );
    assert_eq!(
        imported.current_node.as_deref(),
        Some(expected_node_name.as_str())
    );
    assert_eq!(imported.profile.as_ref().unwrap().chars().count(), 256);
    assert_eq!(imported.current_node.as_ref().unwrap().chars().count(), 256);
    let info = imported.subscription.as_ref().unwrap();
    assert_eq!(info.source_name, "source-canary.invalid");
    assert_eq!(info.display_name, expected_display_name);
    assert_eq!(info.display_name.chars().count(), 128);
    assert_eq!(info.uploaded_bytes, Some(u64::MAX));
    assert_eq!(info.downloaded_bytes, Some(938_375_741_110));
    assert_eq!(info.used_bytes, Some(u64::MAX));
    assert_eq!(info.total_bytes, Some(u64::MAX));
    assert_eq!(info.expires_at_unix, Some(1_792_851_157));
    assert!(info.updated_at_unix > 0);
    assert_eq!(info.refresh_interval_secs, Some(30 * 24 * 60 * 60));
    assert_eq!(
        info.announcement_text.as_ref().unwrap().chars().count(),
        512
    );
    assert_eq!(
        info.announcement_action_label
            .as_ref()
            .unwrap()
            .chars()
            .count(),
        32
    );
    assert_eq!(info.announcement_tone.as_deref(), Some("success"));
    assert!(info.home_available);
    assert!(info.support_available);
    assert!(info.announcement_action_available);
    // The logo arrives in the background, after the import answered.
    assert_eq!(info.service_logo_path, None);
    assert!(info.service_logo_pending);
    backend.fetch_service_logos().await;
    let with_logo = backend.status().await.unwrap();
    let first_logo = with_logo
        .subscription
        .as_ref()
        .unwrap()
        .service_logo_path
        .clone()
        .unwrap();
    assert!(
        std::fs::read(&first_logo)
            .unwrap()
            .starts_with(b"\x89PNG\r\n\x1a\n")
    );
    #[cfg(windows)]
    assert!(first_logo.ends_with(".png"));
    assert!(std::path::Path::new(&first_logo).is_file());
    assert!(info.refresh_available);

    let serialized = serde_json::to_string(&imported).unwrap();
    assert!(serialized.len() < 64 * 1024);
    for secret in [
        SOURCE_URL_CANARY,
        "source-password",
        "source_token=source-canary",
        HOME_URL_CANARY,
        SUPPORT_URL_CANARY,
        ANNOUNCEMENT_URL_CANARY,
        LOGO_URL_CANARY,
        RAW_HEADER_NAME_CANARY,
        RAW_HEADER_VALUE_CANARY,
        MIHOMO_BODY_CANARY,
        XRAY_BODY_CANARY,
        "subscription-userinfo",
        "profile-web-page-url",
        "support-url",
        "announce-url",
        "flclashx-servicelogo",
    ] {
        assert!(
            !serialized.contains(secret),
            "leaked {secret}: {serialized}"
        );
    }

    let refreshed = backend.refresh_subscription().await.unwrap();
    assert!(refreshed.subscription.as_ref().unwrap().refresh_available);
    assert_eq!(refreshed.profile, imported.profile);
    assert_eq!(refreshed.current_node, imported.current_node);
    // The refresh keeps showing the logo while its new copy downloads.
    assert_eq!(
        refreshed.subscription.as_ref().unwrap().service_logo_path,
        Some(first_logo.clone())
    );
    let first_image = std::fs::read(&first_logo).unwrap();
    backend.fetch_service_logos().await;
    // The same image again (the advertised path is a lease's: a /proc fd path on Linux).
    let downloaded_again = backend
        .status()
        .await
        .unwrap()
        .subscription
        .unwrap()
        .service_logo_path
        .unwrap();
    assert_eq!(std::fs::read(downloaded_again).unwrap(), first_image);
    assert_eq!(
        *calls.lock().unwrap(),
        [UA_NATIVE, UA_SERVICE_LOGO, UA_NATIVE, UA_SERVICE_LOGO]
    );
    assert!(!format!("{refreshed:?}").contains("source-password"));
}

#[tokio::test]
async fn connected_refresh_keeps_tunnel_up_and_restores_selected_node_after_restart() {
    let root = tempfile::tempdir().unwrap();
    let store = ProfileStore::open(root.path(), None).unwrap();
    let source = "https://subscription.example/config";
    store
        .create_profile(Arc::new(
            Snapshot::parse(REFRESH_SELECTION_MIHOMO.as_bytes(), br#"{}"#)
                .unwrap()
                .with_subscription_source(source, None, 1)
                .unwrap(),
        ))
        .unwrap();
    let controller = Arc::new(RecordingController::default());
    let backend = CoreBackend::new_with_selector(
        store,
        SubscriptionFetcher::new(FixtureHttp::with_mihomo(REFRESH_SELECTION_MIHOMO)),
        {
            let controller = controller.clone();
            move |_snapshot| Ok(controller.clone())
        },
        Arc::new(ConfirmingSelector),
    )
    .unwrap();

    let catalog = backend.catalog().await.unwrap();
    backend
        .select(SelectionRequest {
            revision: catalog.revision,
            group_id: catalog.groups[0].id.clone(),
            node_id: catalog.groups[0].nodes[1].id.clone(),
        })
        .await
        .unwrap();
    backend.connect().await.unwrap();
    let refreshed = backend.refresh_subscription().await.unwrap();
    assert_eq!(refreshed.state, ConnectionState::Connected);
    assert!(refreshed.subscription.unwrap().updated_at_unix > 1);
    assert_eq!(controller.calls.lock().unwrap().len(), 2);
    backend.disconnect().await.unwrap();
    assert!(backend.catalog().await.unwrap().groups[0].nodes[1].selected);
    drop(backend);

    let restarted = test_backend(
        ProfileStore::open(root.path(), None).unwrap(),
        SubscriptionFetcher::new(OfflineHttp),
        |_snapshot| Ok(Arc::new(RecordingController::default())),
    )
    .unwrap();
    assert!(restarted.catalog().await.unwrap().groups[0].nodes[1].selected);
}

#[tokio::test]
async fn selection_after_connected_refresh_is_saved_for_the_new_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let store = ProfileStore::open(root.path(), None).unwrap();
    store
        .create_profile(Arc::new(
            Snapshot::parse(REFRESH_SELECTION_MIHOMO.as_bytes(), br#"{}"#)
                .unwrap()
                .with_subscription_source("https://subscription.example/config", None, 1)
                .unwrap(),
        ))
        .unwrap();
    let backend = test_backend(
        store,
        SubscriptionFetcher::new(FixtureHttp::with_mihomo(REFRESH_SELECTION_MIHOMO)),
        |_snapshot| Ok(Arc::new(RecordingController::default())),
    )
    .unwrap();
    let before = backend.catalog().await.unwrap();
    backend.connect().await.unwrap();
    backend.refresh_subscription().await.unwrap();
    backend
        .select(SelectionRequest {
            revision: before.revision,
            group_id: before.groups[0].id.clone(),
            node_id: before.groups[0].nodes[1].id.clone(),
        })
        .await
        .unwrap();
    backend.disconnect().await.unwrap();
    assert!(backend.catalog().await.unwrap().groups[0].nodes[1].selected);
    drop(backend);
    let restarted = test_backend(
        ProfileStore::open(root.path(), None).unwrap(),
        SubscriptionFetcher::new(OfflineHttp),
        |_snapshot| Ok(Arc::new(RecordingController::default())),
    )
    .unwrap();
    assert!(restarted.catalog().await.unwrap().groups[0].nodes[1].selected);
}

#[tokio::test]
async fn importing_an_existing_subscription_preserves_its_selected_node() {
    let root = tempfile::tempdir().unwrap();
    let backend = test_backend(
        ProfileStore::open(root.path(), None).unwrap(),
        SubscriptionFetcher::new(FixtureHttp::with_mihomo(REFRESH_SELECTION_MIHOMO)),
        |_snapshot| Ok(Arc::new(RecordingController::default())),
    )
    .unwrap();
    let source = "https://subscription.example/config";
    backend
        .import_subscription(ImportSubscriptionRequest { url: source.into() })
        .await
        .unwrap();
    let catalog = backend.catalog().await.unwrap();
    backend
        .select(SelectionRequest {
            revision: catalog.revision,
            group_id: catalog.groups[0].id.clone(),
            node_id: catalog.groups[0].nodes[1].id.clone(),
        })
        .await
        .unwrap();
    backend
        .import_subscription(ImportSubscriptionRequest { url: source.into() })
        .await
        .unwrap();
    assert!(backend.catalog().await.unwrap().groups[0].nodes[1].selected);
    drop(backend);
    let restarted = test_backend(
        ProfileStore::open(root.path(), None).unwrap(),
        SubscriptionFetcher::new(OfflineHttp),
        |_snapshot| Ok(Arc::new(RecordingController::default())),
    )
    .unwrap();
    assert!(restarted.catalog().await.unwrap().groups[0].nodes[1].selected);
}

#[tokio::test]
#[cfg_attr(
    not(any(target_os = "linux", target_os = "android", windows)),
    ignore = "the logo path comes from /proc/<pid>/fd, which this platform lacks"
)]
async fn refresh_keeps_the_logo_when_only_its_download_fails() {
    let root = tempfile::tempdir().unwrap();
    let http = FixtureHttp::default();
    let logo_download_fails = http.logo_download_fails.clone();
    let include_logo = http.include_logo.clone();
    let backend = test_backend(
        ProfileStore::open(root.path(), None).unwrap(),
        SubscriptionFetcher::new(http),
        |_snapshot| Ok(Arc::new(RecordingController::default())),
    )
    .unwrap();
    backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://provider.example/sub".into(),
        })
        .await
        .unwrap();
    backend.fetch_service_logos().await;
    let imported = backend.status().await.unwrap();
    let logo_of = |status: &multicore_daemon::StatusDto| {
        status
            .subscription
            .as_ref()
            .unwrap()
            .service_logo_path
            .as_ref()
            .map(|path| std::fs::read(path).unwrap())
    };
    let original = logo_of(&imported).expect("imported with a logo");

    // A network blip or a fake-ip VPN makes only the logo download fail.
    logo_download_fails.store(true, std::sync::atomic::Ordering::SeqCst);
    backend.refresh_subscription().await.unwrap();
    backend.fetch_service_logos().await;
    let refreshed = backend.status().await.unwrap();
    assert_eq!(logo_of(&refreshed), Some(original));

    // A provider that stops naming a logo does drop it.
    include_logo.store(false, std::sync::atomic::Ordering::SeqCst);
    backend.refresh_subscription().await.unwrap();
    backend.fetch_service_logos().await;
    let refreshed = backend.status().await.unwrap();
    assert_eq!(logo_of(&refreshed), None);
}

#[tokio::test]
#[cfg_attr(
    not(any(target_os = "linux", target_os = "android", windows)),
    ignore = "the logo path comes from /proc/<pid>/fd, which this platform lacks"
)]
async fn profile_list_sanitizes_hostile_metadata_and_retains_inactive_logo() {
    let root = tempfile::tempdir().unwrap();
    let backend = test_backend(
        ProfileStore::open(root.path(), None).unwrap(),
        SubscriptionFetcher::new(FixtureHttp::hostile()),
        |_snapshot| Ok(Arc::new(RecordingController::default())),
    )
    .unwrap();
    backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://user:password@one.invalid/sub?token=first".into(),
        })
        .await
        .unwrap();
    backend.fetch_service_logos().await;
    let first = backend.status().await.unwrap();
    let first_id = ProfileId::parse(first.active_profile_id.as_deref().unwrap()).unwrap();
    let expected_logo = std::fs::read(
        first
            .subscription
            .as_ref()
            .unwrap()
            .service_logo_path
            .as_ref()
            .unwrap(),
    )
    .unwrap();
    backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://two.invalid/raw-config?secret=second".into(),
        })
        .await
        .unwrap();
    backend.fetch_service_logos().await;

    let listed = Backend::list_subscriptions(&backend).await.unwrap();
    assert_eq!(listed.profiles.len(), 2);
    let inactive = &listed.profiles[0];
    assert_eq!(inactive.id, first_id.as_str());
    assert!(!inactive.active);
    assert!(inactive.available);
    assert_eq!(inactive.display_name, "one.invalid");
    assert!(inactive.display_name.chars().count() <= 256);
    assert_eq!(inactive.used_bytes, Some(7));
    assert_eq!(inactive.total_bytes, Some(20));
    let inactive_logo = inactive.service_logo_path.as_ref().unwrap().clone();
    assert_eq!(std::fs::read(&inactive_logo).unwrap(), expected_logo);

    let serialized = serde_json::to_string(&listed).unwrap();
    assert!(serialized.len() < 16 * 1024);
    for forbidden in [
        "https://",
        "password",
        "token",
        "secret",
        "raw-config",
        MIHOMO,
    ] {
        assert!(!serialized.contains(forbidden), "leaked {forbidden}");
    }

    Backend::refresh_subscription_profile(&backend, first_id.clone())
        .await
        .unwrap();
    backend.fetch_service_logos().await;
    let refreshed = Backend::list_subscriptions(&backend).await.unwrap();
    let refreshed_logo = refreshed.profiles[0].service_logo_path.as_ref().unwrap();
    // The same image again (paths are leases', so compared by content).
    assert_eq!(std::fs::read(refreshed_logo).unwrap(), expected_logo);

    backend.delete_subscription(&first_id).await.unwrap();
    assert_eq!(
        backend.retained_profile_logo_path_for_testing(&first_id),
        None
    );
    #[cfg(windows)]
    assert!(!std::path::Path::new(refreshed_logo).exists());
    let after_delete = Backend::list_subscriptions(&backend).await.unwrap();
    assert!(
        after_delete
            .profiles
            .iter()
            .all(|profile| profile.id != first_id.as_str())
    );
}

#[cfg(windows)]
#[tokio::test]
async fn deleting_active_logo_drops_all_leases_before_cleanup_and_recovers_capacity() {
    let root = tempfile::tempdir().unwrap();
    let backend = test_backend(
        ProfileStore::open(root.path(), None).unwrap(),
        SubscriptionFetcher::new(FixtureHttp::hostile()),
        |_snapshot| Ok(Arc::new(RecordingController::default())),
    )
    .unwrap();
    backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://active-logo.invalid/sub".into(),
        })
        .await
        .unwrap();
    backend.fetch_service_logos().await;
    let imported = backend.status().await.unwrap();
    let id = ProfileId::parse(imported.active_profile_id.as_deref().unwrap()).unwrap();
    let profile_directory = root.path().join(id.as_str());
    let logo_path = imported
        .subscription
        .as_ref()
        .unwrap()
        .service_logo_path
        .as_ref()
        .unwrap()
        .clone();
    assert!(profile_directory.is_dir());
    assert!(std::path::Path::new(&logo_path).is_file());

    backend.delete_subscription(&id).await.unwrap();

    assert!(!profile_directory.exists());
    #[cfg(windows)]
    assert!(!std::path::Path::new(&logo_path).exists());
    for index in 0..MAX_PROFILES {
        backend
            .import_subscription(ImportSubscriptionRequest {
                url: format!("https://capacity-{index}.invalid/sub"),
            })
            .await
            .unwrap();
    }
    assert_eq!(backend.list_subscriptions().unwrap().len(), MAX_PROFILES);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_list_cannot_replace_a_refreshed_inactive_logo_lease() {
    let root = tempfile::tempdir().unwrap();
    let (http, include_logo) = FixtureHttp::hostile_with_logo_toggle();
    let backend = Arc::new(
        test_backend(
            ProfileStore::open(root.path(), None).unwrap(),
            SubscriptionFetcher::new(http),
            |_snapshot| Ok(Arc::new(RecordingController::default())),
        )
        .unwrap(),
    );
    let first = backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://one.invalid/sub".into(),
        })
        .await
        .unwrap();
    let first_id = ProfileId::parse(first.active_profile_id.as_deref().unwrap()).unwrap();
    backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://two.invalid/sub".into(),
        })
        .await
        .unwrap();

    let (reached, resume) = backend.inject_list_logo_publish_hook_for_testing();
    let listing_backend = backend.clone();
    let listing = tokio::spawn(async move {
        Backend::list_subscriptions(listing_backend.as_ref())
            .await
            .unwrap()
    });
    tokio::task::spawn_blocking(move || {
        reached
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("list reached the pre-publication hook")
    })
    .await
    .unwrap();

    include_logo.store(false, std::sync::atomic::Ordering::SeqCst);
    let refreshed = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        Backend::refresh_subscription_profile(backend.as_ref(), first_id.clone()),
    )
    .await;
    resume.send(()).unwrap();
    let refreshed = refreshed
        .expect("refresh did not wait for the stale list")
        .unwrap();
    assert_eq!(refreshed.profiles[0].service_logo_path, None);
    let stale_list = listing.await.unwrap();

    assert_eq!(stale_list.profiles[0].service_logo_path, None);
    assert_eq!(
        backend.retained_profile_logo_path_for_testing(&first_id),
        None
    );
}

#[tokio::test]
async fn committed_refresh_load_failure_clears_stale_runtime_and_keeps_authoritative_id() {
    let root = tempfile::tempdir().unwrap();
    let backend = profile_backend(root.path());
    let imported = backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://one.invalid/sub".into(),
        })
        .await
        .unwrap();
    let id = ProfileId::parse(imported.active_profile_id.as_deref().unwrap()).unwrap();
    assert!(imported.profile.is_some());
    backend.inject_post_commit_load_fault_for_testing();

    let error = Backend::refresh_subscription_profile(&backend, id.clone())
        .await
        .unwrap_err();

    assert_eq!(
        error.profile_mutation(),
        Some(ProfileMutationError::Committed)
    );
    let status = backend.status().await.unwrap();
    assert_eq!(status.active_profile_id.as_deref(), Some(id.as_str()));
    assert_eq!(status.state, ConnectionState::Empty);
    assert_eq!(status.profile, None);
    assert_eq!(status.current_node, None);
    assert_eq!(status.subscription, None);
    assert!(backend.catalog().await.unwrap().groups.is_empty());
}

#[tokio::test]
async fn committed_import_load_failure_clears_runtime_and_keeps_new_authoritative_id() {
    let root = tempfile::tempdir().unwrap();
    let backend = profile_backend(root.path());
    backend.inject_post_commit_load_fault_for_testing();

    let error = backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://one.invalid/sub".into(),
        })
        .await
        .unwrap_err();

    assert_eq!(
        error.profile_mutation(),
        Some(ProfileMutationError::Committed)
    );
    let status = backend.status().await.unwrap();
    let authoritative = status
        .active_profile_id
        .as_deref()
        .expect("committed import remains authoritative");
    assert!(ProfileId::parse(authoritative).is_ok());
    assert_eq!(status.state, ConnectionState::Empty);
    assert_eq!(status.profile, None);
    assert_eq!(status.current_node, None);
    assert_eq!(status.subscription, None);
    assert!(backend.catalog().await.unwrap().groups.is_empty());
}

#[test]
fn subscription_dto_deserializes_legacy_payloads_with_safe_defaults() {
    let info: multicore_daemon::SubscriptionInfoDto = serde_json::from_value(serde_json::json!({
        "source_name": "legacy.example",
        "downloaded_bytes": 7,
        "total_bytes": 10,
        "expires_at_unix": null,
        "updated_at_unix": 123,
        "refresh_available": true
    }))
    .unwrap();

    assert_eq!(info.source_name, "legacy.example");
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

    let status: multicore_daemon::StatusDto = serde_json::from_value(serde_json::json!({
        "state": "empty",
        "profile": null,
        "current_node": null,
        "message": null,
        "degraded": false,
        "subscription": null
    }))
    .unwrap();
    assert_eq!(status.active_profile_id, None);
}

fn assert_safe_subscription_metadata_eq(
    expected: &multicore_daemon::SubscriptionInfoDto,
    expected_logo: Option<&[u8]>,
    actual: &multicore_daemon::SubscriptionInfoDto,
) {
    assert_eq!(actual.source_name, expected.source_name);
    assert_eq!(actual.display_name, expected.display_name);
    assert_eq!(actual.uploaded_bytes, expected.uploaded_bytes);
    assert_eq!(actual.downloaded_bytes, expected.downloaded_bytes);
    assert_eq!(actual.used_bytes, expected.used_bytes);
    assert_eq!(actual.total_bytes, expected.total_bytes);
    assert_eq!(actual.expires_at_unix, expected.expires_at_unix);
    assert_eq!(actual.updated_at_unix, expected.updated_at_unix);
    assert_eq!(actual.refresh_interval_secs, expected.refresh_interval_secs);
    assert_eq!(actual.announcement_text, expected.announcement_text);
    assert_eq!(
        actual.announcement_action_label,
        expected.announcement_action_label
    );
    assert_eq!(actual.announcement_tone, expected.announcement_tone);
    assert_eq!(actual.home_available, expected.home_available);
    assert_eq!(actual.support_available, expected.support_available);
    assert_eq!(
        actual.announcement_action_available,
        expected.announcement_action_available
    );
    assert_eq!(
        actual
            .service_logo_path
            .as_ref()
            .map(|path| std::fs::read(path).unwrap()),
        expected_logo.map(<[u8]>::to_vec)
    );
    #[cfg(windows)]
    assert_eq!(actual.service_logo_path, expected.service_logo_path);
    assert_eq!(actual.refresh_available, expected.refresh_available);
}

#[tokio::test]
async fn failed_refresh_keeps_last_good_profile_ready_and_safe_metadata_visible() {
    let root = tempfile::tempdir().unwrap();
    let store = ProfileStore::open(root.path(), None).unwrap();
    let (http, _, expected_group_name, expected_node_name) = maximal_fixture();
    let backend = test_backend(store, SubscriptionFetcher::new(http), |_snapshot| {
        Ok(Arc::new(RecordingController::default()))
    })
    .unwrap();
    let imported = backend
        .import_subscription(ImportSubscriptionRequest {
            url: SOURCE_URL_CANARY.into(),
        })
        .await
        .unwrap();
    let expected = imported.subscription.as_ref().unwrap().clone();
    let expected_logo = expected
        .service_logo_path
        .as_ref()
        .map(|path| std::fs::read(path).unwrap());
    assert!(expected.home_available);
    assert!(expected.support_available);
    assert!(expected.announcement_action_available);
    assert_eq!(
        imported.profile.as_deref(),
        Some(expected_group_name.as_str())
    );
    assert_eq!(
        imported.current_node.as_deref(),
        Some(expected_node_name.as_str())
    );

    drop(backend);
    let store = ProfileStore::open(root.path(), None).unwrap();
    let backend = test_backend(store, SubscriptionFetcher::new(OfflineHttp), |_snapshot| {
        Ok(Arc::new(RecordingController::default()))
    })
    .unwrap();
    let before_failure = backend.status().await.unwrap();
    assert_safe_subscription_metadata_eq(
        &expected,
        expected_logo.as_deref(),
        before_failure.subscription.as_ref().unwrap(),
    );
    assert_eq!(before_failure.profile, imported.profile);
    assert_eq!(before_failure.current_node, imported.current_node);

    assert!(backend.refresh_subscription().await.is_err());
    let after_failure = backend.status().await.unwrap();
    assert_eq!(after_failure.state, ConnectionState::Ready);
    assert_safe_subscription_metadata_eq(
        &expected,
        expected_logo.as_deref(),
        after_failure.subscription.as_ref().unwrap(),
    );
    assert_eq!(after_failure.profile, imported.profile);
    assert_eq!(after_failure.current_node, imported.current_node);
    assert!(!format!("{after_failure:?}").contains("source-password"));

    drop(backend);
    let store = ProfileStore::open(root.path(), None).unwrap();
    let restarted = test_backend(store, SubscriptionFetcher::new(OfflineHttp), |_snapshot| {
        Ok(Arc::new(RecordingController::default()))
    })
    .unwrap();
    let after_restart = restarted.status().await.unwrap();
    assert_safe_subscription_metadata_eq(
        &expected,
        expected_logo.as_deref(),
        after_restart.subscription.as_ref().unwrap(),
    );
    assert_eq!(after_restart.profile, imported.profile);
    assert_eq!(after_restart.current_node, imported.current_node);
}

#[tokio::test]
async fn legacy_persisted_subscription_without_display_name_uses_source_host_fallback() {
    let root = tempfile::tempdir().unwrap();
    let store = ProfileStore::open(root.path(), None).unwrap();
    let profile_id = store
        .create_profile(Arc::new(
            Snapshot::parse(MIHOMO.as_bytes(), br#"{}"#)
                .unwrap()
                .with_subscription_source(
                    "https://legacy-host-canary.invalid/private?token=hidden",
                    Some("download=7"),
                    1_789_405_200,
                )
                .unwrap(),
        ))
        .unwrap();
    drop(store);

    let generation = std::fs::read_dir(root.path().join(profile_id.as_str()))
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| entry.file_name().to_string_lossy().starts_with("snapshot-"))
        .unwrap();
    let subscription_path = generation.path().join("subscription.json");
    let mut persisted: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&subscription_path).unwrap()).unwrap();
    persisted["info"]
        .as_object_mut()
        .unwrap()
        .remove("display_name");
    std::fs::write(&subscription_path, serde_json::to_vec(&persisted).unwrap()).unwrap();

    let store = ProfileStore::open(root.path(), None).unwrap();
    let backend = test_backend(store, SubscriptionFetcher::new(OfflineHttp), |_snapshot| {
        Ok(Arc::new(RecordingController::default()))
    })
    .unwrap();
    let status = backend.status().await.unwrap();
    let info = status.subscription.unwrap();
    assert_eq!(info.source_name, "legacy-host-canary.invalid");
    assert_eq!(info.display_name, "legacy-host-canary.invalid");
    assert_eq!(status.profile.as_deref(), Some("Main"));
    assert_eq!(status.current_node.as_deref(), Some("Test Node"));
}

#[tokio::test]
async fn diagnostics_exposes_bounded_safe_mihomo_to_xray_mappings_only() {
    let root = tempfile::tempdir().unwrap();
    let mut proxies = String::from(
        "  - { name: 'token=must-not-leak', type: socks5, server: 127.0.0.1, port: 19999 }\n",
    );
    for index in 0..=MAX_DIAGNOSTIC_MAPPINGS {
        proxies.push_str(&format!(
            "  - {{ name: Node {index}, type: socks5, server: 127.0.0.1, port: {} }}\n",
            20_000 + index
        ));
    }
    proxies.push_str(concat!(
        "  - { name: Remote, type: socks5, server: 192.0.2.5, port: 31991 }\n",
        "  - { name: Wrong, type: http, server: 127.0.0.1, port: 31992 }\n",
    ));
    let mihomo = format!("proxies:\n{proxies}");
    let store = ProfileStore::open(root.path(), None).unwrap();
    store
        .create_profile(Arc::new(
            Snapshot::parse(mihomo.as_bytes(), br#"{ "secret": "opaque-xray" }"#).unwrap(),
        ))
        .unwrap();
    let backend = test_backend(store, SubscriptionFetcher::new(OfflineHttp), |_snapshot| {
        Ok(Arc::new(RecordingController::default()))
    })
    .unwrap();

    let diagnostics = backend.diagnostics().await.unwrap();
    assert_eq!(diagnostics.mappings.len(), MAX_DIAGNOSTIC_MAPPINGS);
    assert_eq!(diagnostics.mappings[0].proxy_name, "Proxy 1");
    assert_eq!(diagnostics.mappings[0].xray_label, "Proxy 1");
    assert_eq!(diagnostics.mappings[1].proxy_name, "Node 0");
    assert_eq!(diagnostics.mappings[1].address, "127.0.0.1:20000");
    assert_eq!(diagnostics.mappings[1].xray_label, "Node 0");
    let serialized = serde_json::to_string(&diagnostics).unwrap();
    assert!(!serialized.contains("must-not-leak"));
    assert!(!serialized.contains("192.0.2.5"));
    assert!(!serialized.contains("opaque-xray"));
}

#[derive(Default)]
struct RecordingController {
    calls: Mutex<Vec<(String, Engine)>>,
}

struct RollbackFailingController;

struct PathCheckingController {
    mihomo: std::path::PathBuf,
    xray: std::path::PathBuf,
}

impl ProcessController for PathCheckingController {
    type Error = ();

    fn start<'a>(
        &'a self,
        _engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async move {
            if self.mihomo.is_file() && self.xray.is_file() {
                Ok(())
            } else {
                Err(())
            }
        })
    }

    fn stop<'a>(
        &'a self,
        _engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

impl ProcessController for RollbackFailingController {
    type Error = ();

    fn start<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async move {
            if engine == Engine::Mihomo {
                Err(())
            } else {
                Ok(())
            }
        })
    }

    fn stop<'a>(
        &'a self,
        _engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async { Err(()) })
    }
}

#[derive(Default)]
struct RecoverableController {
    stop_calls: std::sync::atomic::AtomicUsize,
    calls: Mutex<Vec<(String, Engine)>>,
}

impl ProcessController for RecoverableController {
    type Error = ();

    fn start<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(("start".into(), engine));
            if engine == Engine::Mihomo {
                Err(())
            } else {
                Ok(())
            }
        })
    }

    fn stop<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(("stop".into(), engine));
            let call = self
                .stop_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if call == 0 { Err(()) } else { Ok(()) }
        })
    }
}

impl ProcessController for RecordingController {
    type Error = ();

    fn start<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(("start".into(), engine));
            Ok(())
        })
    }

    fn stop<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(("stop".into(), engine));
            Ok(())
        })
    }

    fn diagnostics<'a>(&'a self) -> Pin<Box<dyn Future<Output = ProcessDiagnostics> + Send + 'a>> {
        Box::pin(async {
            ProcessDiagnostics {
                xray: RuntimeCheckState::Ready,
                mihomo: RuntimeCheckState::Ready,
                tun: RuntimeCheckState::Ready,
                logs: vec![CoreLogRecord {
                    id: 7,
                    timestamp_ms: 11,
                    engine: Engine::Mihomo,
                    stream: DiagnosticStream::Stderr,
                    message: "safe core line".into(),
                }],
            }
        })
    }
}

#[tokio::test]
async fn successful_fetch_persists_snapshot_and_updates_status_before_supervision() {
    let directory = tempfile::tempdir().unwrap();
    let store = ProfileStore::open(directory.path().join("profiles"), None).unwrap();
    let http = FixtureHttp::default();
    let http_calls = http.calls.clone();
    let controller = Arc::new(RecordingController::default());
    let controller_for_factory = controller.clone();
    let backend = test_backend(
        store,
        SubscriptionFetcher::new(http),
        move |_snapshot: &Snapshot| -> Result<Arc<RecordingController>, BackendError> {
            Ok(controller_for_factory.clone())
        },
    )
    .unwrap();

    assert_eq!(
        backend.status().await.unwrap().state,
        ConnectionState::Empty
    );
    let secret_url = "https://user:password@example.invalid/sub?token=secret";
    let imported = backend
        .import_subscription(ImportSubscriptionRequest {
            url: secret_url.into(),
        })
        .await
        .unwrap();
    assert_eq!(imported.state, ConnectionState::Ready);
    assert_eq!(imported.profile.as_deref(), Some("Main"));
    assert_eq!(imported.current_node.as_deref(), Some("Test Node"));
    assert!(!imported.degraded);
    // The provider logo is not part of the import: it is downloaded in the background.
    assert_eq!(*http_calls.lock().unwrap(), vec![UA_NATIVE]);
    assert!(imported.subscription.as_ref().unwrap().service_logo_pending);
    backend.fetch_service_logos().await;
    assert_eq!(
        *http_calls.lock().unwrap(),
        vec![UA_NATIVE, UA_SERVICE_LOGO]
    );
    assert!(
        !backend
            .status()
            .await
            .unwrap()
            .subscription
            .unwrap()
            .service_logo_pending
    );

    assert_eq!(
        backend.connect().await.unwrap().state,
        ConnectionState::Connected
    );
    let diagnostics = backend.diagnostics().await.unwrap();
    assert_eq!(diagnostics.xray, RuntimeCheckDto::Ready);
    assert_eq!(diagnostics.mihomo, RuntimeCheckDto::Ready);
    assert_eq!(diagnostics.tun, RuntimeCheckDto::Ready);
    assert_eq!(diagnostics.logs.len(), 1);
    assert_eq!(diagnostics.logs[0].component, "mihomo");
    assert_eq!(diagnostics.logs[0].level, "error");
    assert_eq!(diagnostics.logs[0].message, "safe core line");
    assert_eq!(
        backend.disconnect().await.unwrap().state,
        ConnectionState::Ready
    );
    assert_eq!(
        *controller.calls.lock().unwrap(),
        vec![
            ("start".into(), Engine::Xray),
            ("start".into(), Engine::Mihomo),
            ("stop".into(), Engine::Mihomo),
            ("stop".into(), Engine::Xray),
        ]
    );

    let events = backend.events_after(0).await.unwrap();
    assert!(!events.is_empty());
    let serialized = serde_json::to_string(&events).unwrap();
    assert!(!serialized.contains(secret_url));
    assert!(!serialized.contains("password"));
    assert!(!serialized.contains("token=secret"));

    drop(backend);
    let reopened = ProfileStore::open(directory.path().join("profiles"), None).unwrap();
    assert!(reopened.active_snapshot().unwrap().is_some());
}

#[tokio::test]
async fn every_connect_reprepares_the_controller_from_the_current_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let store = ProfileStore::open(directory.path().join("profiles"), None).unwrap();
    let factory_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let factory_calls_clone = factory_calls.clone();
    let controller = Arc::new(RecordingController::default());
    let controller_clone = controller.clone();
    let backend = test_backend(
        store,
        SubscriptionFetcher::new(FixtureHttp::default()),
        move |_snapshot: &Snapshot| -> Result<Arc<RecordingController>, BackendError> {
            factory_calls_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(controller_clone.clone())
        },
    )
    .unwrap();

    backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://example.invalid/sub".into(),
        })
        .await
        .unwrap();
    assert_eq!(factory_calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    backend.connect().await.unwrap();
    assert_eq!(factory_calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    backend.disconnect().await.unwrap();
    backend.connect().await.unwrap();
    assert_eq!(factory_calls.load(std::sync::atomic::Ordering::SeqCst), 3);
}

#[tokio::test]
async fn persisted_profile_loads_without_network_preparation_until_connect() {
    let directory = tempfile::tempdir().unwrap();
    let store = ProfileStore::open(directory.path().join("profiles"), None).unwrap();
    store
        .create_profile(Arc::new(
            Snapshot::parse(MIHOMO.as_bytes(), br#"{}"#).unwrap(),
        ))
        .unwrap();
    let factory_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let factory_calls_clone = factory_calls.clone();
    let backend = test_backend(
        store,
        SubscriptionFetcher::new(OfflineHttp),
        move |_snapshot: &Snapshot| -> Result<Arc<RecordingController>, BackendError> {
            factory_calls_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(BackendError::new())
        },
    )
    .unwrap();

    assert_eq!(
        backend.status().await.unwrap().state,
        ConnectionState::Ready
    );
    assert_eq!(factory_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(backend.connect().await.is_err());
    assert_eq!(factory_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        backend.status().await.unwrap().state,
        ConnectionState::Ready
    );
}

#[tokio::test]
async fn default_constructor_connects_without_replaying_an_implicit_catalog_default() {
    let directory = tempfile::tempdir().unwrap();
    let store = ProfileStore::open(directory.path().join("profiles"), None).unwrap();
    store
        .create_profile(Arc::new(
            Snapshot::parse(MIHOMO.as_bytes(), br#"{}"#).unwrap(),
        ))
        .unwrap();
    let backend = CoreBackend::new(store, SubscriptionFetcher::new(OfflineHttp), |_snapshot| {
        Ok(Arc::new(RecordingController::default()))
    })
    .unwrap();

    assert!(backend.catalog().await.unwrap().groups[0].nodes[0].selected);
    assert_eq!(
        backend.connect().await.unwrap().state,
        ConnectionState::Connected
    );
}

#[tokio::test]
async fn default_transactional_constructor_connects_without_replaying_an_implicit_default() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = directory.path().join("runtime");
    let store = ProfileStore::open(directory.path().join("profiles"), None).unwrap();
    store
        .create_profile(Arc::new(
            Snapshot::parse(MIHOMO.as_bytes(), br#"{}"#).unwrap(),
        ))
        .unwrap();
    let control =
        MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "ephemeral").unwrap();
    let factory = transactional_controller_factory(move |snapshot: &Snapshot| {
        let staged = stage_runtime_with_mihomo_control(snapshot, &runtime, &control)
            .map_err(|_| BackendError::new())?;
        Ok(PreparedController::with_runtime(
            Arc::new(RecordingController::default()),
            staged,
        ))
    });
    let backend =
        CoreBackend::new_transactional(store, SubscriptionFetcher::new(OfflineHttp), factory)
            .unwrap();

    assert!(backend.catalog().await.unwrap().groups[0].nodes[0].selected);
    assert_eq!(
        backend.connect().await.unwrap().state,
        ConnectionState::Connected
    );
}

struct NoSelector;

#[async_trait]
impl multicore_daemon::MihomoSelector for NoSelector {
    async fn select(
        &self,
        _raw_group: &str,
        _raw_node: &str,
    ) -> Result<(), multicore_daemon::SelectorError> {
        Err(multicore_daemon::SelectorError::new())
    }
}

#[tokio::test]
async fn connect_options_reach_the_controller_factory() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = directory.path().join("runtime");
    let store = ProfileStore::open(directory.path().join("profiles"), None).unwrap();
    store
        .create_profile(Arc::new(
            Snapshot::parse(MIHOMO.as_bytes(), br#"{}"#).unwrap(),
        ))
        .unwrap();
    let control =
        MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "ephemeral").unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = seen.clone();
    let factory = multicore_daemon::options_controller_factory(
        move |snapshot: &Snapshot, options: multicore_daemon::ConnectOptions| {
            recorded.lock().unwrap().push(options.tun);
            let staged = stage_runtime_with_mihomo_control(snapshot, &runtime, &control)
                .map_err(|_| BackendError::new())?;
            Ok(PreparedController::with_runtime(
                Arc::new(RecordingController::default()),
                staged,
            ))
        },
    );
    let backend = CoreBackend::new_with_connect_options(
        store,
        SubscriptionFetcher::new(OfflineHttp),
        factory,
        Arc::new(NoSelector),
        None,
    )
    .unwrap();

    let status = backend
        .connect_with(multicore_daemon::ConnectOptions {
            tun: false,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(status.state, ConnectionState::Connected);
    backend.disconnect().await.unwrap();
    backend.connect().await.unwrap();
    assert_eq!(*seen.lock().unwrap(), [false, true]);
}

#[test]
fn status_states_serialize_exactly_as_the_desktop_contract() {
    let cases = [
        (ConnectionState::Empty, "empty"),
        (ConnectionState::Ready, "ready"),
        (ConnectionState::Connecting, "connecting"),
        (ConnectionState::Connected, "connected"),
        (ConnectionState::Error, "error"),
    ];
    for (state, expected) in cases {
        assert_eq!(serde_json::to_value(state).unwrap(), expected);
    }
}

#[tokio::test]
async fn rollback_failure_is_reported_as_error_and_degraded_status() {
    let directory = tempfile::tempdir().unwrap();
    let store = ProfileStore::open(directory.path().join("profiles"), None).unwrap();
    let backend = test_backend(
        store,
        SubscriptionFetcher::new(FixtureHttp::default()),
        |_snapshot: &Snapshot| -> Result<Arc<RollbackFailingController>, BackendError> {
            Ok(Arc::new(RollbackFailingController))
        },
    )
    .unwrap();
    backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://example.invalid/sub".into(),
        })
        .await
        .unwrap();

    assert!(backend.connect().await.is_err());
    let status = backend.status().await.unwrap();
    assert_eq!(status.state, ConnectionState::Error);
    assert!(status.degraded);
    assert!(status.message.is_some());
}

#[tokio::test]
async fn successful_rollback_returns_to_ready_without_degraded_mode() {
    let directory = tempfile::tempdir().unwrap();
    let store = ProfileStore::open(directory.path().join("profiles"), None).unwrap();
    struct SafeRollbackController;
    impl ProcessController for SafeRollbackController {
        type Error = ();
        fn start<'a>(
            &'a self,
            engine: Engine,
        ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
            Box::pin(async move { (engine != Engine::Mihomo).then_some(()).ok_or(()) })
        }
        fn stop<'a>(
            &'a self,
            _engine: Engine,
        ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
            Box::pin(async { Ok(()) })
        }
    }
    let backend = test_backend(
        store,
        SubscriptionFetcher::new(FixtureHttp::default()),
        |_snapshot: &Snapshot| -> Result<Arc<SafeRollbackController>, BackendError> {
            Ok(Arc::new(SafeRollbackController))
        },
    )
    .unwrap();
    backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://example.invalid/sub".into(),
        })
        .await
        .unwrap();

    assert!(backend.connect().await.is_err());
    let status = backend.status().await.unwrap();
    assert_eq!(status.state, ConnectionState::Ready);
    assert!(!status.degraded);
    assert!(status.message.is_some());
}

struct CausedFailureController {
    engine: Engine,
    cause: StartFailureCause,
}

impl ProcessController for CausedFailureController {
    type Error = StartFailureCause;

    fn start<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async move {
            if engine == self.engine {
                Err(self.cause)
            } else {
                Ok(())
            }
        })
    }

    fn stop<'a>(
        &'a self,
        _engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }

    fn start_failure_cause(&self, error: &Self::Error) -> StartFailureCause {
        *error
    }
}

#[tokio::test]
async fn connect_failure_message_explains_the_classified_cause() {
    for (engine, cause, expected) in [
        (
            Engine::Xray,
            StartFailureCause::ElevationCancelled,
            "Подключение отменено: не выдано разрешение администратора.",
        ),
        (
            Engine::Mihomo,
            StartFailureCause::CoreFailed,
            "Не удалось запустить Mihomo. Подробности — в логе на странице «Состояние».",
        ),
        (
            Engine::Mihomo,
            StartFailureCause::CoreMissing,
            "Не удалось запустить Mihomo: файл ядра не найден или заблокирован. Проверьте карантин антивируса и переустановите MULTICORE.",
        ),
        (
            Engine::Xray,
            StartFailureCause::CoreExited,
            "Xray завершился сразу после запуска. Причина — в логе на странице «Состояние».",
        ),
        (
            Engine::Mihomo,
            StartFailureCause::ReadinessTimedOut,
            "Mihomo не успел запуститься вовремя. Повторите попытку.",
        ),
        (
            Engine::Mihomo,
            StartFailureCause::TunFailed,
            "Не удалось включить TUN-адаптер. Повторите попытку; если не поможет — перезагрузите компьютер или подключитесь без TUN.",
        ),
        (
            Engine::Mihomo,
            StartFailureCause::TunTimedOut,
            "TUN-адаптер не включился вовремя. Повторите попытку; если не поможет — перезагрузите компьютер или подключитесь без TUN.",
        ),
        (
            Engine::Mihomo,
            StartFailureCause::SystemProxyFailed,
            "Не удалось включить системный прокси. Повторите попытку.",
        ),
        (
            Engine::Mihomo,
            StartFailureCause::Unknown,
            "Не удалось запустить подключение. Повторите попытку.",
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let store = ProfileStore::open(directory.path().join("profiles"), None).unwrap();
        let backend = test_backend(
            store,
            SubscriptionFetcher::new(FixtureHttp::default()),
            move |_snapshot: &Snapshot| -> Result<Arc<CausedFailureController>, BackendError> {
                Ok(Arc::new(CausedFailureController { engine, cause }))
            },
        )
        .unwrap();
        backend
            .import_subscription(ImportSubscriptionRequest {
                url: "https://example.invalid/sub".into(),
            })
            .await
            .unwrap();

        let error = backend.connect().await.unwrap_err();
        // The HTTP layer answers with the cause, not a bare "backend_error".
        assert_eq!(error.start_failure(), Some((engine, cause)));
        let status = backend.status().await.unwrap();
        assert_eq!(status.state, ConnectionState::Ready);
        assert_eq!(status.message.as_deref(), Some(expected));
    }
}

/// Fails `engine`'s start; its diagnostics log alternates lines of `engine` and Xray.
struct LoggingFailureController {
    engine: Engine,
    lines: Vec<String>,
}

impl ProcessController for LoggingFailureController {
    type Error = StartFailureCause;

    fn start<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async move {
            if engine == self.engine {
                Err(StartFailureCause::CoreExited)
            } else {
                Ok(())
            }
        })
    }

    fn stop<'a>(
        &'a self,
        _engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }

    fn diagnostics<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = multicore_core::ProcessDiagnostics> + Send + 'a>> {
        Box::pin(async move {
            let logs = self
                .lines
                .iter()
                .enumerate()
                .map(|(index, line)| multicore_core::CoreLogRecord {
                    id: index as u64,
                    timestamp_ms: 0,
                    engine: if index % 2 == 0 {
                        self.engine
                    } else {
                        Engine::Xray
                    },
                    stream: multicore_core::DiagnosticStream::Stderr,
                    message: line.clone(),
                })
                .collect();
            multicore_core::ProcessDiagnostics {
                logs,
                ..Default::default()
            }
        })
    }

    fn start_failure_cause(&self, error: &Self::Error) -> StartFailureCause {
        *error
    }
}

#[tokio::test]
async fn a_failed_start_logs_the_last_lines_of_the_failed_core() {
    let directory = tempfile::tempdir().unwrap();
    let store = ProfileStore::open(directory.path().join("profiles"), None).unwrap();
    let mut lines: Vec<String> = (0..20).map(|index| format!("line {index}")).collect();
    lines.push("level=fatal msg=\"Start TUN listening error: configure tun interface\"".into());
    let backend = test_backend(
        store,
        SubscriptionFetcher::new(FixtureHttp::default()),
        move |_snapshot: &Snapshot| -> Result<Arc<LoggingFailureController>, BackendError> {
            Ok(Arc::new(LoggingFailureController {
                engine: Engine::Mihomo,
                lines: lines.clone(),
            }))
        },
    )
    .unwrap();
    backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://example.invalid/sub".into(),
        })
        .await
        .unwrap();
    assert!(backend.connect().await.is_err());

    let events = backend.events_after(0).await.unwrap();
    let failure = events
        .iter()
        .rev()
        .find(|event| event.message == "Запуск подключения завершился ошибкой.")
        .expect("failure event");
    let tail: Vec<String> = failure.details.as_ref().unwrap()["core_log_tail"]
        .as_array()
        .expect("core log tail")
        .iter()
        .map(|line| line.as_str().unwrap().to_owned())
        .collect();
    // Only the failed core's lines, oldest first, the newest eight of them.
    assert_eq!(
        tail,
        [
            "line 6",
            "line 8",
            "line 10",
            "line 12",
            "line 14",
            "line 16",
            "line 18",
            "level=fatal msg=\"Start TUN listening error: configure tun interface\"",
        ]
    );
}

#[tokio::test]
async fn degraded_mode_blocks_import_without_replacing_controller_until_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let store = ProfileStore::open(directory.path().join("profiles"), None).unwrap();
    let factory_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let factory_calls_clone = factory_calls.clone();
    let controller = Arc::new(RecoverableController::default());
    let controller_clone = controller.clone();
    let backend = test_backend(
        store,
        SubscriptionFetcher::new(FixtureHttp::default()),
        move |_snapshot: &Snapshot| -> Result<Arc<RecoverableController>, BackendError> {
            factory_calls_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(controller_clone.clone())
        },
    )
    .unwrap();
    let imported = backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://example.invalid/first".into(),
        })
        .await
        .unwrap();
    assert!(backend.connect().await.is_err());
    assert!(backend.status().await.unwrap().degraded);
    let active_id = profile_id(imported.active_profile_id.as_deref().unwrap());
    assert_eq!(
        backend.activate_subscription(&active_id).await,
        Err(ProfileMutationError::Connected)
    );
    assert_eq!(
        backend.delete_subscription(&active_id).await,
        Err(ProfileMutationError::Connected)
    );

    assert!(
        backend
            .import_subscription(ImportSubscriptionRequest {
                url: "https://example.invalid/blocked".into(),
            })
            .await
            .is_err()
    );
    assert_eq!(factory_calls.load(std::sync::atomic::Ordering::SeqCst), 2);

    let cleaned = backend.disconnect().await.unwrap();
    assert_eq!(cleaned.state, ConnectionState::Ready);
    assert!(!cleaned.degraded);
    backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://example.invalid/allowed".into(),
        })
        .await
        .unwrap();
    assert_eq!(factory_calls.load(std::sync::atomic::Ordering::SeqCst), 3);
}

#[tokio::test]
async fn degraded_mode_blocks_reconnect_until_successful_disconnect_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let store = ProfileStore::open(directory.path().join("profiles"), None).unwrap();
    let controller = Arc::new(RecoverableController::default());
    let controller_clone = controller.clone();
    let backend = test_backend(
        store,
        SubscriptionFetcher::new(FixtureHttp::default()),
        move |_snapshot: &Snapshot| -> Result<Arc<RecoverableController>, BackendError> {
            Ok(controller_clone.clone())
        },
    )
    .unwrap();
    backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://example.invalid/sub".into(),
        })
        .await
        .unwrap();

    assert!(backend.connect().await.is_err());
    assert!(backend.status().await.unwrap().degraded);
    assert!(backend.connect().await.is_err());
    assert_eq!(
        *controller.calls.lock().unwrap(),
        vec![
            ("start".into(), Engine::Xray),
            ("start".into(), Engine::Mihomo),
            ("stop".into(), Engine::Xray),
        ]
    );

    let cleaned = backend.disconnect().await.unwrap();
    assert_eq!(cleaned.state, ConnectionState::Ready);
    assert!(!cleaned.degraded);

    assert!(backend.connect().await.is_err());
    assert_eq!(
        *controller.calls.lock().unwrap(),
        vec![
            ("start".into(), Engine::Xray),
            ("start".into(), Engine::Mihomo),
            ("stop".into(), Engine::Xray),
            ("stop".into(), Engine::Mihomo),
            ("stop".into(), Engine::Xray),
            ("start".into(), Engine::Xray),
            ("start".into(), Engine::Mihomo),
            ("stop".into(), Engine::Xray),
        ]
    );
}

#[tokio::test]
async fn preparation_failure_keeps_the_new_import_and_the_previous_profile() {
    // Preparing the connection runtime used to gate the import itself, so an
    // environmental failure (no default route, another VPN owning it) meant a
    // downloaded subscription was silently not saved. Preparation is redone on
    // every connect, so the import is now kept and connect reports the cause;
    // the previously active profile stays available to switch back to.
    const REPLACEMENT: &str = r#"
proxies:
  - name: Replacement Node
    type: socks5
proxy-groups:
  - name: Replacement
    type: select
    proxies: [Replacement Node]
"#;
    let directory = tempfile::tempdir().unwrap();
    let snapshots = directory.path().join("snapshots");
    let store = ProfileStore::open(&snapshots, None).unwrap();
    let backend = test_backend(
        store,
        SubscriptionFetcher::new(FixtureHttp::default()),
        |_snapshot: &Snapshot| -> Result<Arc<RecordingController>, BackendError> {
            Ok(Arc::new(RecordingController::default()))
        },
    )
    .unwrap();
    backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://example.invalid/original".into(),
        })
        .await
        .unwrap();
    drop(backend);

    let store = ProfileStore::open(&snapshots, None).unwrap();
    let backend = test_backend(
        store,
        SubscriptionFetcher::new(FixtureHttp::with_mihomo(REPLACEMENT)),
        |snapshot: &Snapshot| -> Result<Arc<RecordingController>, BackendError> {
            if snapshot.mihomo.groups()[0].name == "Replacement" {
                Err(BackendError::new())
            } else {
                Ok(Arc::new(RecordingController::default()))
            }
        },
    )
    .unwrap();
    backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://example.invalid/replacement".into(),
        })
        .await
        .expect("a preparation failure no longer rejects the import");
    assert!(
        backend.connect().await.is_err(),
        "connect reports it instead"
    );
    drop(backend);

    let reopened = ProfileStore::open(&snapshots, None).unwrap();
    assert_eq!(
        reopened
            .active_snapshot()
            .unwrap()
            .unwrap()
            .2
            .mihomo
            .groups()[0]
            .name,
        "Replacement"
    );
    assert_eq!(
        reopened.list().unwrap().len(),
        2,
        "the original profile is kept"
    );
}

#[tokio::test]
async fn snapshot_commit_failure_aborts_new_runtime_and_keeps_old_controller_launchable() {
    const REPLACEMENT: &str = r#"
proxies:
  - {name: Replacement Node, type: socks5}
proxy-groups:
  - {name: Replacement, type: select, proxies: [Replacement Node]}
"#;
    let directory = tempfile::tempdir().unwrap();
    let snapshots = directory.path().join("snapshots");
    let runtime = directory.path().join("runtime");
    let store = ProfileStore::open(&snapshots, None).unwrap();
    let store_id = store
        .create_profile(Arc::new(
            Snapshot::parse(MIHOMO.as_bytes(), br#"{}"#)
                .unwrap()
                .with_subscription_source(
                    "https://example.invalid/replacement",
                    None,
                    1_789_405_200,
                )
                .unwrap(),
        ))
        .unwrap();
    let control =
        MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "ephemeral").unwrap();
    let factory = transactional_controller_factory(move |snapshot: &Snapshot| {
        let staged = stage_runtime_with_mihomo_control(snapshot, &runtime, &control)
            .map_err(|_| BackendError::new())?;
        let paths = staged.paths();
        let controller = Arc::new(PathCheckingController {
            mihomo: paths.mihomo_config.clone(),
            xray: paths.xray_config.clone(),
        });
        Ok(PreparedController::with_runtime(controller, staged))
    });
    let backend = CoreBackend::new_transactional_with_selector(
        store,
        SubscriptionFetcher::new(FixtureHttp::with_mihomo(REPLACEMENT)),
        factory,
        Arc::new(ConfirmingSelector),
    )
    .unwrap();
    assert!(backend.connect().await.is_ok());
    assert!(backend.disconnect().await.is_ok());
    let active_before: Vec<_> = std::fs::read_dir(directory.path().join("runtime"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("runtime-"))
        .map(|entry| entry.path())
        .collect();
    assert_eq!(active_before.len(), 1);

    std::fs::create_dir(snapshots.join(store_id.as_str()).join(format!(
        ".snapshot-00000000000000000002.staging-{}",
        std::process::id()
    )))
    .unwrap();
    assert!(
        backend
            .import_subscription(ImportSubscriptionRequest {
                url: "https://example.invalid/replacement".into(),
            })
            .await
            .is_err()
    );
    let active_after: Vec<_> = std::fs::read_dir(directory.path().join("runtime"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("runtime-"))
        .map(|entry| entry.path())
        .collect();
    assert_eq!(active_after, active_before);
    assert_eq!(
        backend.status().await.unwrap().state,
        ConnectionState::Ready
    );
    assert!(backend.connect().await.is_ok());
}

#[tokio::test]
async fn activation_storage_failure_leaves_a_diagnostic_event() {
    // Regression coverage: only import used to log the Storage-bucket
    // failures behind a generic "backend_error"; activate/refresh/delete
    // failed with no event at all ("Событий пока нет" in the copied log).
    let root = tempfile::tempdir().unwrap();
    let backend = profile_backend(root.path());
    let first_id = backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://one.invalid/sub".into(),
        })
        .await
        .unwrap()
        .active_profile_id
        .unwrap();
    backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://two.invalid/sub".into(),
        })
        .await
        .unwrap();
    std::fs::remove_dir_all(root.path().join(&first_id)).unwrap();

    assert!(
        backend
            .activate_subscription(&profile_id(&first_id))
            .await
            .is_err()
    );
    let events = backend.events_after(0).await.unwrap();
    assert!(
        events.iter().any(|event| {
            event
                .details
                .as_ref()
                .and_then(|details| details.get("phase"))
                .and_then(serde_json::Value::as_str)
                == Some("activate_load_failed")
        }),
        "{events:?}"
    );
}

#[tokio::test]
async fn import_succeeds_even_when_connection_preparation_fails() {
    // Regression coverage: import used to prepare the connection runtime up
    // front and fail as a whole when that failed (no default network route,
    // another VPN owning it) - the subscription was downloaded fine but never
    // saved. Preparation is redone on connect anyway, so only connect may fail.
    let root = tempfile::tempdir().unwrap();
    let factory_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let factory_calls_clone = factory_calls.clone();
    let backend = test_backend(
        ProfileStore::open(root.path(), None).unwrap(),
        SubscriptionFetcher::new(ProfileHttp::default()),
        move |_snapshot: &Snapshot| -> Result<Arc<RecordingController>, BackendError> {
            factory_calls_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(BackendError::new())
        },
    )
    .unwrap();

    let imported = backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://one.invalid/sub".into(),
        })
        .await
        .expect("a downloaded subscription is kept even if preparation fails");
    assert_eq!(imported.state, ConnectionState::Ready);
    assert!(imported.active_profile_id.is_some());
    assert_eq!(backend.list_subscriptions().unwrap().len(), 1);

    let events = backend.events_after(0).await.unwrap();
    assert!(events.iter().any(|event| {
        event
            .details
            .as_ref()
            .and_then(|details| details.get("phase"))
            .and_then(serde_json::Value::as_str)
            == Some("import_prepare_deferred")
    }));

    // Connect prepares again and reports the real failure there.
    let before = factory_calls.load(std::sync::atomic::Ordering::SeqCst);
    assert!(backend.connect().await.is_err());
    assert_eq!(
        factory_calls.load(std::sync::atomic::Ordering::SeqCst),
        before + 1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn profile_store_work_runs_off_the_async_workers_on_the_production_runtime() {
    // The daemon runs on the multi-threaded runtime, where store I/O goes
    // through `block_in_place`; this exercises exactly that path (the other
    // tests use the current-thread runtime, where it runs inline).
    let root = tempfile::tempdir().unwrap();
    let backend = profile_backend(root.path());
    let id = backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://one.invalid/sub".into(),
        })
        .await
        .unwrap()
        .active_profile_id
        .unwrap();
    assert_eq!(backend.list_subscriptions().unwrap().len(), 1);
    backend
        .refresh_subscription_profile(&profile_id(&id))
        .await
        .unwrap();
    backend.delete_subscription(&profile_id(&id)).await.unwrap();
    assert!(backend.list_subscriptions().unwrap().is_empty());
}
