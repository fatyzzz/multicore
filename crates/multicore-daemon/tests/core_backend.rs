use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

use multicore_core::{
    CoreLogRecord, DiagnosticStream, Engine, FetchError, HttpClient, HttpResponse,
    MihomoRuntimeControl, PersistentSnapshotStore, ProcessController, ProcessDiagnostics,
    RuntimeCheckState, Snapshot, SubscriptionFetcher, UA_MIHOMO, UA_NATIVE, UA_XRAY,
    stage_runtime_with_mihomo_control,
};
use multicore_daemon::{
    Backend, BackendError, ConnectionState, CoreBackend, ImportSubscriptionRequest,
    MAX_DIAGNOSTIC_MAPPINGS, PreparedController, RuntimeCheckDto, transactional_controller_factory,
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

#[derive(Clone)]
struct FixtureHttp {
    calls: Arc<Mutex<Vec<&'static str>>>,
    mihomo: &'static str,
}

impl Default for FixtureHttp {
    fn default() -> Self {
        Self {
            calls: Arc::default(),
            mihomo: MIHOMO,
        }
    }
}

impl FixtureHttp {
    fn with_mihomo(mihomo: &'static str) -> Self {
        Self {
            calls: Arc::default(),
            mihomo,
        }
    }
}

impl HttpClient for FixtureHttp {
    fn get<'a>(
        &'a self,
        _url: &'a str,
        user_agent: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, FetchError>> + Send + 'a>> {
        self.calls.lock().unwrap().push(user_agent);
        let mihomo = self.mihomo;
        Box::pin(async move {
            let response = match user_agent {
                UA_NATIVE => HttpResponse::new(
                    200,
                    format!("[{},{{}}]", serde_json::to_string(mihomo).unwrap()).into_bytes(),
                    [(
                        "subscription-userinfo",
                        "upload=0; download=938375741110; total=0; expire=1792851157",
                    )],
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
                    HttpResponse::new(200, br#"{}"#.to_vec(), std::iter::empty::<(&str, &str)>())
                }
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

#[tokio::test]
async fn imported_source_exposes_safe_metadata_and_can_refresh_without_resubmitting_url() {
    let root = tempfile::tempdir().unwrap();
    let store = PersistentSnapshotStore::open(root.path()).unwrap();
    let http = FixtureHttp::default();
    let calls = http.calls.clone();
    let backend = CoreBackend::new(store, SubscriptionFetcher::new(http), |_snapshot| {
        Ok(Arc::new(RecordingController::default()))
    })
    .unwrap();

    let imported = backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://user:secret@example.invalid/sub?token=private".into(),
        })
        .await
        .unwrap();
    let info = imported.subscription.unwrap();
    assert_eq!(info.source_name, "example.invalid");
    assert_eq!(info.downloaded_bytes, Some(938_375_741_110));
    assert_eq!(info.total_bytes, None);
    assert_eq!(info.expires_at_unix, Some(1_792_851_157));
    assert!(info.updated_at_unix > 0);
    assert!(info.refresh_available);

    let refreshed = backend.refresh_subscription().await.unwrap();
    assert!(refreshed.subscription.as_ref().unwrap().refresh_available);
    assert_eq!(*calls.lock().unwrap(), [UA_NATIVE, UA_NATIVE]);
    assert!(!format!("{refreshed:?}").contains("private"));
}

#[tokio::test]
async fn failed_refresh_keeps_last_good_profile_ready_and_safe_metadata_visible() {
    let root = tempfile::tempdir().unwrap();
    let store = PersistentSnapshotStore::open(root.path()).unwrap();
    let snapshot = Snapshot::parse(MIHOMO.as_bytes(), br#"{}"#)
        .unwrap()
        .with_subscription_source(
            "https://example.invalid/private?token=secret",
            Some("download=1073741824; total=0; expire=1792851157"),
            1_789_405_200,
        )
        .unwrap();
    store.commit(snapshot).unwrap();
    let backend = CoreBackend::new(store, SubscriptionFetcher::new(OfflineHttp), |_snapshot| {
        Ok(Arc::new(RecordingController::default()))
    })
    .unwrap();

    assert!(backend.refresh_subscription().await.is_err());
    let status = backend.status().await.unwrap();
    assert_eq!(status.state, ConnectionState::Ready);
    assert_eq!(status.profile.as_deref(), Some("Main"));
    assert_eq!(
        status.subscription.as_ref().unwrap().downloaded_bytes,
        Some(1_073_741_824)
    );
    assert!(!format!("{status:?}").contains("secret"));
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
    let store = PersistentSnapshotStore::open(root.path()).unwrap();
    store
        .commit(Snapshot::parse(mihomo.as_bytes(), br#"{ "secret": "opaque-xray" }"#).unwrap())
        .unwrap();
    let backend = CoreBackend::new(store, SubscriptionFetcher::new(OfflineHttp), |_snapshot| {
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
    let store = PersistentSnapshotStore::open(directory.path().join("snapshots")).unwrap();
    let http = FixtureHttp::default();
    let http_calls = http.calls.clone();
    let controller = Arc::new(RecordingController::default());
    let controller_for_factory = controller.clone();
    let backend = CoreBackend::new(
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
    assert_eq!(*http_calls.lock().unwrap(), vec![UA_NATIVE]);

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

    let reopened = PersistentSnapshotStore::open(directory.path().join("snapshots")).unwrap();
    assert!(reopened.current().is_some());
}

#[tokio::test]
async fn every_connect_reprepares_the_controller_from_the_current_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let store = PersistentSnapshotStore::open(directory.path().join("snapshots")).unwrap();
    let factory_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let factory_calls_clone = factory_calls.clone();
    let controller = Arc::new(RecordingController::default());
    let controller_clone = controller.clone();
    let backend = CoreBackend::new(
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
    let store = PersistentSnapshotStore::open(directory.path().join("snapshots")).unwrap();
    store
        .commit(Snapshot::parse(MIHOMO.as_bytes(), br#"{}"#).unwrap())
        .unwrap();
    let factory_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let factory_calls_clone = factory_calls.clone();
    let backend = CoreBackend::new(
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
    let store = PersistentSnapshotStore::open(directory.path().join("snapshots")).unwrap();
    let backend = CoreBackend::new(
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
    let store = PersistentSnapshotStore::open(directory.path().join("snapshots")).unwrap();
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
    let backend = CoreBackend::new(
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

#[tokio::test]
async fn degraded_mode_blocks_import_without_replacing_controller_until_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let store = PersistentSnapshotStore::open(directory.path().join("snapshots")).unwrap();
    let factory_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let factory_calls_clone = factory_calls.clone();
    let controller = Arc::new(RecoverableController::default());
    let controller_clone = controller.clone();
    let backend = CoreBackend::new(
        store,
        SubscriptionFetcher::new(FixtureHttp::default()),
        move |_snapshot: &Snapshot| -> Result<Arc<RecoverableController>, BackendError> {
            factory_calls_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(controller_clone.clone())
        },
    )
    .unwrap();
    backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://example.invalid/first".into(),
        })
        .await
        .unwrap();
    assert!(backend.connect().await.is_err());
    assert!(backend.status().await.unwrap().degraded);

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
    let store = PersistentSnapshotStore::open(directory.path().join("snapshots")).unwrap();
    let controller = Arc::new(RecoverableController::default());
    let controller_clone = controller.clone();
    let backend = CoreBackend::new(
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
async fn publication_failure_does_not_commit_new_snapshot_across_restart() {
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
    let store = PersistentSnapshotStore::open(&snapshots).unwrap();
    let backend = CoreBackend::new(
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

    let store = PersistentSnapshotStore::open(&snapshots).unwrap();
    let backend = CoreBackend::new(
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
    assert!(
        backend
            .import_subscription(ImportSubscriptionRequest {
                url: "https://example.invalid/replacement".into(),
            })
            .await
            .is_err()
    );
    drop(backend);

    let reopened = PersistentSnapshotStore::open(&snapshots).unwrap();
    assert_eq!(reopened.current().unwrap().mihomo.groups()[0].name, "Main");
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
    let store = PersistentSnapshotStore::open(&snapshots).unwrap();
    store
        .commit(Snapshot::parse(MIHOMO.as_bytes(), br#"{}"#).unwrap())
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
    let backend = CoreBackend::new_transactional(
        store,
        SubscriptionFetcher::new(FixtureHttp::with_mihomo(REPLACEMENT)),
        factory,
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

    std::fs::create_dir(snapshots.join(format!(
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
