use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use multicore_core::{
    AtomicSnapshot, Component, ConnectionState, Engine, Event, FetchError, HttpClient,
    HttpResponse, ProcessController, Severity, StateError, StateMachine, SubscriptionFetcher,
    Supervisor, SupervisorError, UA_NATIVE,
};
use serde_json::json;

const MIHOMO: &str = "proxies:\n  - name: Node\n    type: socks5\n    server: 127.0.0.1\n    port: 30001\nproxy-groups:\n  - name: Proxy\n    type: select\n    proxies: [Node]\nrules: [MATCH,Proxy]\n";
const XRAY: &str = "{\n  \"provider-owned\": true\n}\n";

#[derive(Clone)]
struct FakeHttp {
    responses: Arc<Mutex<VecDeque<Result<HttpResponse, FetchError>>>>,
    user_agents: Arc<Mutex<Vec<&'static str>>>,
}

impl FakeHttp {
    fn new(responses: Vec<Result<HttpResponse, FetchError>>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into())),
            user_agents: Arc::default(),
        }
    }
}

impl HttpClient for FakeHttp {
    fn get<'a>(
        &'a self,
        _url: &'a str,
        user_agent: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, FetchError>> + Send + 'a>> {
        Box::pin(async move {
            self.user_agents.lock().unwrap().push(user_agent);
            self.responses.lock().unwrap().pop_front().unwrap()
        })
    }
}

fn ok(body: impl Into<Vec<u8>>) -> Result<HttpResponse, FetchError> {
    Ok(HttpResponse::new(
        200,
        body.into(),
        std::iter::empty::<(&str, &str)>(),
    ))
}

fn ok_with_userinfo(
    body: impl Into<Vec<u8>>,
    subscription_userinfo: &str,
) -> Result<HttpResponse, FetchError> {
    Ok(HttpResponse::new(
        200,
        body.into(),
        [("subscription-userinfo", subscription_userinfo)],
    ))
}

#[tokio::test]
async fn successful_fetch_attaches_bounded_safe_subscription_metadata_and_source() {
    let bundle = format!("[{},{}]", serde_json::to_string(MIHOMO).unwrap(), XRAY);
    let http = FakeHttp::new(vec![ok_with_userinfo(
        bundle.into_bytes(),
        "upload=0; download=938375741110; total=0; expire=1792851157",
    )]);
    let store = AtomicSnapshot::default();

    SubscriptionFetcher::new(http)
        .refresh("https://example.invalid/secret-token", &store)
        .await
        .unwrap();

    let snapshot = store.current().unwrap();
    assert_eq!(
        snapshot.subscription_source_url(),
        Some("https://example.invalid/secret-token")
    );
    let info = snapshot.subscription_info().unwrap();
    assert_eq!(info.source_host, "example.invalid");
    assert_eq!(info.downloaded_bytes, Some(938_375_741_110));
    assert_eq!(info.total_bytes, None);
    assert_eq!(info.expires_at_unix, Some(1_792_851_157));
    assert!(info.updated_at_unix > 0);
}

#[tokio::test]
async fn answered_but_invalid_massive_response_never_triggers_more_requests() {
    let invalid_native = serde_json::to_vec(&json!([MIHOMO, [{ "config": 1 }]])).unwrap();
    let http = FakeHttp::new(vec![
        ok(invalid_native),
        ok(MIHOMO.as_bytes().to_vec()),
        ok(XRAY.as_bytes().to_vec()),
    ]);
    let agents = http.user_agents.clone();
    let store = AtomicSnapshot::default();

    let result = SubscriptionFetcher::new(http)
        .refresh(
            "https://user:secret@example.invalid/sub?token=secret",
            &store,
        )
        .await;

    assert_eq!(result.unwrap_err(), FetchError::InvalidBundle);
    assert_eq!(*agents.lock().unwrap(), [UA_NATIVE]);
    assert!(store.current().is_none());
}

#[tokio::test]
async fn successful_native_bundle_preserves_exact_xray_object_lexeme() {
    let raw_xray = "{\n  \"z-last\": 1,\n  \"a-first\": { \"spacing\" : true }\n}";
    let bundle = format!("[{},{}]", serde_json::to_string(MIHOMO).unwrap(), raw_xray);
    let http = FakeHttp::new(vec![ok(bundle.into_bytes())]);
    let agents = http.user_agents.clone();
    let store = AtomicSnapshot::default();

    SubscriptionFetcher::new(http)
        .refresh("https://example.invalid/native", &store)
        .await
        .unwrap();

    assert_eq!(*agents.lock().unwrap(), [UA_NATIVE]);
    assert_eq!(store.current().unwrap().xray.raw_json(), raw_xray);
}

#[tokio::test]
async fn failed_refresh_never_replaces_last_good_snapshot() {
    let store = AtomicSnapshot::default();
    let good = FakeHttp::new(vec![
        Err(FetchError::Network),
        ok(MIHOMO.as_bytes().to_vec()),
        ok(XRAY.as_bytes().to_vec()),
    ]);
    SubscriptionFetcher::new(good)
        .refresh("https://example.invalid/sub", &store)
        .await
        .unwrap();
    let before = store.current().unwrap();

    let bad = FakeHttp::new(vec![
        Err(FetchError::Network),
        ok(b"not yaml".to_vec()),
        ok(XRAY.as_bytes().to_vec()),
    ]);
    assert!(
        SubscriptionFetcher::new(bad)
            .refresh("https://example.invalid/sub", &store)
            .await
            .is_err()
    );
    let after = store.current().unwrap();
    assert!(Arc::ptr_eq(&before, &after));
}

#[test]
fn state_machine_enforces_connection_lifecycle() {
    let mut state = StateMachine::default();
    assert_eq!(state.current(), ConnectionState::Empty);
    assert_eq!(
        state.transition(ConnectionState::Connected),
        Err(StateError::InvalidTransition {
            from: ConnectionState::Empty,
            to: ConnectionState::Connected
        })
    );
    state.transition(ConnectionState::Ready).unwrap();
    state.transition(ConnectionState::Connecting).unwrap();
    state.transition(ConnectionState::Connected).unwrap();
    state.transition(ConnectionState::Ready).unwrap();
}

#[test]
fn structured_events_are_redacted_before_they_exist() {
    let event = Event::new(
        42,
        Severity::Error,
        Component::Subscription,
        "refresh",
        "corr-safe",
        "failed https://user:pass@example.invalid/private/secret-path?token=abc Authorization: Bearer abc 00000000-0000-4000-8000-000000000001",
        json!({
            "url": "https://example.invalid/private/secret-path?token=abc",
            "subscription_url": "https://provider.invalid/credential-in-path",
            "password": "top-secret",
            "nested": {"cookie": "session=abc"}
        }),
    );
    let encoded = serde_json::to_string(&event).unwrap();
    for secret in [
        "user:pass",
        "token=abc",
        "Bearer abc",
        "00000000-0000-4000-8000-000000000001",
        "top-secret",
        "session=abc",
        "example.invalid",
        "provider.invalid",
        "secret-path",
        "credential-in-path",
    ] {
        assert!(!encoded.contains(secret), "leaked {secret}");
    }
    assert!(encoded.contains("[redacted]"));
}

#[derive(Default)]
struct FakeProcesses {
    calls: Mutex<Vec<(String, Engine)>>,
    fail_start: Mutex<Option<Engine>>,
}

impl ProcessController for FakeProcesses {
    type Error = ();

    fn start<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), ()>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(("start".into(), engine));
            if *self.fail_start.lock().unwrap() == Some(engine) {
                Err(())
            } else {
                Ok(())
            }
        })
    }

    fn stop<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), ()>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(("stop".into(), engine));
            Ok(())
        })
    }
}

#[tokio::test]
async fn supervisor_orders_sidecars_and_rolls_back_partial_start() {
    let processes = Arc::new(FakeProcesses::default());
    let supervisor = Supervisor::new(processes.clone());
    supervisor.connect().await.unwrap();
    supervisor.disconnect().await.unwrap();
    assert_eq!(
        *processes.calls.lock().unwrap(),
        [
            ("start".into(), Engine::Xray),
            ("start".into(), Engine::Mihomo),
            ("stop".into(), Engine::Mihomo),
            ("stop".into(), Engine::Xray),
        ]
    );

    let failing = Arc::new(FakeProcesses {
        fail_start: Mutex::new(Some(Engine::Mihomo)),
        ..FakeProcesses::default()
    });
    let error = Supervisor::new(failing.clone())
        .connect()
        .await
        .unwrap_err();
    assert_eq!(
        error,
        SupervisorError::StartFailed {
            engine: Engine::Mihomo,
            rollback_failed: false
        }
    );
    assert_eq!(
        *failing.calls.lock().unwrap(),
        [
            ("start".into(), Engine::Xray),
            ("start".into(), Engine::Mihomo),
            ("stop".into(), Engine::Xray),
        ]
    );
}

#[derive(Default)]
struct BlockingProcesses {
    calls: Mutex<Vec<(String, Engine)>>,
    block_mihomo_start: AtomicBool,
    mihomo_start_entered: tokio::sync::Notify,
    release_mihomo_start: tokio::sync::Notify,
}

impl ProcessController for BlockingProcesses {
    type Error = ();

    fn start<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), ()>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(("start".into(), engine));
            if engine == Engine::Mihomo && self.block_mihomo_start.swap(false, Ordering::SeqCst) {
                self.mihomo_start_entered.notify_one();
                self.release_mihomo_start.notified().await;
            }
            Ok(())
        })
    }

    fn stop<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), ()>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(("stop".into(), engine));
            Ok(())
        })
    }
}

#[tokio::test]
async fn supervisor_serializes_complete_opposing_operations() {
    let processes = Arc::new(BlockingProcesses {
        block_mihomo_start: AtomicBool::new(true),
        ..BlockingProcesses::default()
    });
    let supervisor = Arc::new(Supervisor::new(processes.clone()));

    let connecting = tokio::spawn({
        let supervisor = supervisor.clone();
        async move { supervisor.connect().await }
    });
    processes.mihomo_start_entered.notified().await;
    let disconnecting = tokio::spawn({
        let supervisor = supervisor.clone();
        async move { supervisor.disconnect().await }
    });
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }

    assert_eq!(
        *processes.calls.lock().unwrap(),
        [
            ("start".into(), Engine::Xray),
            ("start".into(), Engine::Mihomo),
        ]
    );
    processes.release_mihomo_start.notify_one();
    connecting.await.unwrap().unwrap();
    disconnecting.await.unwrap().unwrap();
    assert_eq!(
        *processes.calls.lock().unwrap(),
        [
            ("start".into(), Engine::Xray),
            ("start".into(), Engine::Mihomo),
            ("stop".into(), Engine::Mihomo),
            ("stop".into(), Engine::Xray),
        ]
    );
}
