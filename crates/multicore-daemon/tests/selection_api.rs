use std::{
    future::Future,
    io::{Read, Write},
    net::TcpListener,
    pin::Pin,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use multicore_core::{
    Engine, FetchError, HttpClient, HttpResponse, PersistentSnapshotStore, ProcessController,
    Snapshot, SubscriptionFetcher,
};
use multicore_daemon::{
    Backend, BackendError, CoreBackend, MihomoHttpSelector, MihomoSelector, SelectionRequest,
    SelectorError, router,
};
use serde_json::{Value, json};
use tower::ServiceExt;

const TOKEN: &str = "selection-token";
const MIHOMO: &str = r#"
proxies:
  - name: "node / ? # %"
    type: socks5
  - name: second
    type: socks5
proxy-groups:
  - name: "group / ? # %"
    type: select
    proxies: ["node / ? # %", second]
"#;

const MULTI_GROUP_MIHOMO: &str = r#"
proxies:
  - {name: server-a, type: socks5}
  - {name: server-b, type: socks5}
  - {name: ru-a, type: socks5}
  - {name: ru-b, type: socks5}
  - {name: game-a, type: socks5}
  - {name: game-b, type: socks5}
proxy-groups:
  - {name: Server, type: select, proxies: [server-a, server-b]}
  - {name: Russian sites, type: select, proxies: [ru-a, ru-b]}
  - {name: Games, type: select, proxies: [game-a, game-b]}
"#;

struct NeverHttp;
impl HttpClient for NeverHttp {
    fn get<'a>(
        &'a self,
        _: &'a str,
        _: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, FetchError>> + Send + 'a>> {
        Box::pin(async { Err(FetchError::Network) })
    }
}

struct NoopController;
impl ProcessController for NoopController {
    type Error = ();
    fn start<'a>(
        &'a self,
        _: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
    fn stop<'a>(
        &'a self,
        _: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Default)]
struct RecordingSelector {
    calls: Mutex<Vec<(String, String)>>,
    fail: bool,
}

struct BlockingSelector {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl MihomoSelector for BlockingSelector {
    async fn select(&self, _group: &str, _node: &str) -> Result<(), SelectorError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }
}

#[async_trait]
impl MihomoSelector for RecordingSelector {
    async fn select(&self, group: &str, node: &str) -> Result<(), SelectorError> {
        self.calls
            .lock()
            .unwrap()
            .push((group.to_owned(), node.to_owned()));
        if self.fail {
            Err(SelectorError::new())
        } else {
            Ok(())
        }
    }
}

fn backend(
    selector: Arc<RecordingSelector>,
) -> CoreBackend<
    NeverHttp,
    NoopController,
    impl Fn(&Snapshot) -> Result<Arc<NoopController>, BackendError> + use<>,
> {
    backend_with_mihomo(selector, MIHOMO)
}

fn backend_with_mihomo(
    selector: Arc<RecordingSelector>,
    mihomo: &str,
) -> CoreBackend<
    NeverHttp,
    NoopController,
    impl Fn(&Snapshot) -> Result<Arc<NoopController>, BackendError> + use<>,
> {
    let root = tempfile::tempdir().unwrap();
    let store = PersistentSnapshotStore::open(root.path().join("snapshots")).unwrap();
    store
        .commit(Snapshot::parse(mihomo.as_bytes(), br#"{ "xray": "exact" }"#).unwrap())
        .unwrap();
    CoreBackend::new_with_selector(
        store,
        SubscriptionFetcher::new(NeverHttp),
        |_snapshot| Ok(Arc::new(NoopController)),
        selector,
    )
    .unwrap()
}

#[tokio::test]
async fn duplicate_node_names_do_not_create_false_controller_identities() {
    let mihomo = r#"
proxies:
  - {name: duplicate, type: socks5}
proxy-groups:
  - {name: Main, type: select, proxies: [duplicate, duplicate]}
"#;
    let selector = Arc::new(RecordingSelector::default());
    let backend = backend_with_mihomo(selector, mihomo);
    let catalog = backend.catalog().await.unwrap();
    assert_eq!(catalog.groups.len(), 1);
    assert_eq!(catalog.groups[0].nodes.len(), 1);
}

#[tokio::test]
async fn select_and_import_are_serialized_by_the_shared_operation_mutex() {
    let root = tempfile::tempdir().unwrap();
    let store = PersistentSnapshotStore::open(root.path().join("snapshots")).unwrap();
    store
        .commit(Snapshot::parse(MIHOMO.as_bytes(), br#"{}"#).unwrap())
        .unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let entered_wait = entered.notified();
    let backend = Arc::new(
        CoreBackend::new_with_selector(
            store,
            SubscriptionFetcher::new(NeverHttp),
            |_snapshot| Ok(Arc::new(NoopController)),
            Arc::new(BlockingSelector {
                entered: entered.clone(),
                release: release.clone(),
            }),
        )
        .unwrap(),
    );
    backend.connect().await.unwrap();
    let catalog = backend.catalog().await.unwrap();
    let select_backend = backend.clone();
    let selection = SelectionRequest {
        revision: catalog.revision,
        group_id: catalog.groups[0].id.clone(),
        node_id: catalog.groups[0].nodes[1].id.clone(),
    };
    let select_task = tokio::spawn(async move { select_backend.select(selection).await });
    entered_wait.await;

    let import_backend = backend.clone();
    let mut import_task = tokio::spawn(async move {
        import_backend
            .import_subscription(multicore_daemon::ImportSubscriptionRequest {
                url: "https://example.invalid/sub".to_owned(),
            })
            .await
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut import_task)
            .await
            .is_err()
    );

    release.notify_one();
    assert!(select_task.await.unwrap().is_ok());
    // Import acquires the mutex only afterwards and correctly observes that the cores remain
    // connected, so it does not begin a refresh.
    assert!(import_task.await.unwrap().is_err());
}

fn request(uri: &str, body: Value, authenticated: bool) -> Request<Body> {
    let mut builder = Request::builder()
        .method("PUT")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    if authenticated {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

async fn json_body(response: axum::response::Response) -> Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[tokio::test]
async fn selection_requires_auth_connection_and_current_opaque_ids() {
    let selector = Arc::new(RecordingSelector::default());
    let backend = backend(selector);
    let catalog = backend.catalog().await.unwrap();
    let group = &catalog.groups[0];
    let second = &group.nodes[1];
    let app = router(backend, TOKEN);
    let body = json!({"revision": catalog.revision, "node_id": second.id});

    let unauthorized = app
        .clone()
        .oneshot(request(
            &format!("/v1/selections/{}", group.id),
            json!({"revision": catalog.revision, "node_id": "submitted-private-node"}),
            false,
        ))
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    let unauthorized_body = json_body(unauthorized).await.to_string();
    assert!(!unauthorized_body.contains(TOKEN));
    assert!(!unauthorized_body.contains("submitted-private-node"));
    let disconnected = app
        .clone()
        .oneshot(request(
            &format!("/v1/selections/{}", group.id),
            body.clone(),
            true,
        ))
        .await
        .unwrap();
    assert_eq!(disconnected.status(), StatusCode::CONFLICT);
    assert_eq!(json_body(disconnected).await["code"], "not_connected");
}

#[tokio::test]
async fn successful_selection_uses_raw_names_and_updates_only_selected_flags() {
    let selector = Arc::new(RecordingSelector::default());
    let backend = backend(selector.clone());
    backend.connect().await.unwrap();
    let before = backend.catalog().await.unwrap();
    let group = &before.groups[0];
    let target = &group.nodes[1];
    let updated = backend
        .select(SelectionRequest {
            revision: before.revision,
            group_id: group.id.clone(),
            node_id: target.id.clone(),
        })
        .await
        .unwrap();

    assert_eq!(
        selector.calls.lock().unwrap().as_slice(),
        &[("group / ? # %".into(), "second".into())]
    );
    assert_eq!(updated.revision, before.revision);
    assert!(updated.groups[0].selected);
    assert!(!updated.groups[0].nodes[0].selected);
    assert!(updated.groups[0].nodes[1].selected);
}

#[tokio::test]
async fn distinct_selector_groups_keep_independent_selected_nodes() {
    let selector = Arc::new(RecordingSelector::default());
    let backend = backend_with_mihomo(selector.clone(), MULTI_GROUP_MIHOMO);
    backend.connect().await.unwrap();

    for group_index in 0..3 {
        let catalog = backend.catalog().await.unwrap();
        backend
            .select(SelectionRequest {
                revision: catalog.revision,
                group_id: catalog.groups[group_index].id.clone(),
                node_id: catalog.groups[group_index].nodes[1].id.clone(),
            })
            .await
            .unwrap();
    }

    let catalog = backend.catalog().await.unwrap();
    assert_eq!(catalog.groups.len(), 3);
    for group in &catalog.groups {
        assert!(
            !group.nodes[0].selected,
            "{} reset to its first node",
            group.label
        );
        assert!(
            group.nodes[1].selected,
            "{} lost its own selection",
            group.label
        );
    }
    let status = backend.status().await.unwrap();
    assert_eq!(status.profile.as_deref(), Some("Server"));
    assert_eq!(status.current_node.as_deref(), Some("server-b"));
    assert_eq!(
        selector.calls.lock().unwrap().as_slice(),
        &[
            ("Server".into(), "server-b".into()),
            ("Russian sites".into(), "ru-b".into()),
            ("Games".into(), "game-b".into()),
        ]
    );
}

#[tokio::test]
async fn selection_endpoint_returns_the_updated_safe_catalog() {
    let selector = Arc::new(RecordingSelector::default());
    let backend = backend(selector.clone());
    backend.connect().await.unwrap();
    let before = backend.catalog().await.unwrap();
    let group_id = before.groups[0].id.clone();
    let node_id = before.groups[0].nodes[1].id.clone();
    let app = router(backend, TOKEN);

    let response = app
        .oneshot(request(
            &format!("/v1/selections/{group_id}"),
            json!({"revision": before.revision, "node_id": node_id}),
            true,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let catalog = json_body(response).await;
    assert_eq!(catalog["revision"], before.revision);
    assert_eq!(catalog["groups"][0]["nodes"][0]["selected"], false);
    assert_eq!(catalog["groups"][0]["nodes"][1]["selected"], true);
    assert_eq!(
        selector.calls.lock().unwrap().as_slice(),
        &[("group / ? # %".into(), "second".into())]
    );
    assert!(!catalog.to_string().contains("group / ? # %"));
}

#[tokio::test]
async fn stale_unknown_and_selector_failure_are_stable_and_rollback_selection() {
    let selector = Arc::new(RecordingSelector {
        calls: Mutex::new(Vec::new()),
        fail: true,
    });
    let backend = backend(selector);
    backend.connect().await.unwrap();
    let before = backend.catalog().await.unwrap();

    let stale = backend
        .select(SelectionRequest {
            revision: before.revision + 1,
            group_id: before.groups[0].id.clone(),
            node_id: before.groups[0].nodes[1].id.clone(),
        })
        .await
        .unwrap_err();
    assert!(matches!(
        stale,
        multicore_daemon::SelectionError::StaleRevision
    ));
    let unknown = backend
        .select(SelectionRequest {
            revision: before.revision,
            group_id: "g-unknown".into(),
            node_id: before.groups[0].nodes[1].id.clone(),
        })
        .await
        .unwrap_err();
    assert!(matches!(
        unknown,
        multicore_daemon::SelectionError::UnknownGroup
    ));
    let failed = backend
        .select(SelectionRequest {
            revision: before.revision,
            group_id: before.groups[0].id.clone(),
            node_id: before.groups[0].nodes[1].id.clone(),
        })
        .await
        .unwrap_err();
    assert!(matches!(
        failed,
        multicore_daemon::SelectionError::SelectorFailed
    ));
    assert_eq!(backend.catalog().await.unwrap(), before);
}

#[tokio::test]
async fn selection_http_errors_have_stable_status_and_safe_codes() {
    let selector = Arc::new(RecordingSelector {
        calls: Mutex::new(Vec::new()),
        fail: true,
    });
    let backend = backend(selector);
    backend.connect().await.unwrap();
    let catalog = backend.catalog().await.unwrap();
    let group = &catalog.groups[0];
    let node = &group.nodes[1];
    let app = router(backend, TOKEN);
    let sensitive_group_id = "submitted-private-group-marker";
    let sensitive_node_id = "submitted-private-node-marker";

    for (group_id, revision, node_id, expected_status, expected_code) in [
        (
            group.id.as_str(),
            catalog.revision + 1,
            sensitive_node_id,
            StatusCode::CONFLICT,
            "stale_revision",
        ),
        (
            sensitive_group_id,
            catalog.revision,
            sensitive_node_id,
            StatusCode::NOT_FOUND,
            "unknown_group",
        ),
        (
            group.id.as_str(),
            catalog.revision,
            sensitive_node_id,
            StatusCode::NOT_FOUND,
            "unknown_node",
        ),
        (
            group.id.as_str(),
            catalog.revision,
            node.id.as_str(),
            StatusCode::BAD_GATEWAY,
            "selector_failed",
        ),
    ] {
        let response = app
            .clone()
            .oneshot(request(
                &format!("/v1/selections/{group_id}"),
                json!({"revision": revision, "node_id": node_id}),
                true,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), expected_status);
        let body = json_body(response).await;
        assert_eq!(body["code"], expected_code);
        assert!(!body.to_string().contains("group / ? # %"));
        assert!(!body.to_string().contains("node / ? # %"));
        assert!(!body.to_string().contains(TOKEN));
        assert!(!body.to_string().contains(sensitive_group_id));
        assert!(!body.to_string().contains(sensitive_node_id));
    }
}

#[tokio::test]
async fn selection_errors_never_reflect_submitted_sensitive_ids_or_bearer_token() {
    let app = router(backend(Arc::new(RecordingSelector::default())), TOKEN);
    let sensitive_group = "submitted-private-group-marker";
    let sensitive_node = "submitted-private-node-marker";
    let response = app
        .oneshot(request(
            &format!("/v1/selections/{sensitive_group}"),
            json!({"revision": 1, "node_id": sensitive_node}),
            true,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = json_body(response).await.to_string();
    for forbidden in [TOKEN, sensitive_group, sensitive_node, "Bearer"] {
        assert!(
            !body.contains(forbidden),
            "error reflected {forbidden}: {body}"
        );
    }
}

#[tokio::test]
async fn selection_rejects_oversized_json_with_safe_structured_error() {
    let sensitive_marker = "oversized-private-node-marker";
    let oversized_node = format!("{sensitive_marker}{}", "x".repeat(1024 * 1024));
    let builder = Request::builder()
        .method("PUT")
        .uri("/v1/selections/submitted-private-group-marker")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
    let response = router(backend(Arc::new(RecordingSelector::default())), TOKEN)
        .oneshot(
            builder
                .body(Body::from(
                    json!({"revision": 1, "node_id": oversized_node}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = json_body(response).await;
    assert_eq!(body["code"], "payload_too_large");
    assert!(
        body["correlation_id"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    let serialized = body.to_string();
    for forbidden in [TOKEN, sensitive_marker, "submitted-private-group-marker"] {
        assert!(
            !serialized.contains(forbidden),
            "oversize error reflected {forbidden}"
        );
    }
}

#[tokio::test]
async fn http_selector_percent_encodes_group_and_sends_secret_and_raw_node_as_json() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let captured_server = captured.clone();
    let server = thread::spawn(move || {
        for request_index in 0..3 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 2048];
            loop {
                let count = stream.read(&mut buffer).unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                let Some(headers_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..headers_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|value| value.parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if request.len() >= headers_end + 4 + content_length {
                    break;
                }
            }
            if request_index == 0 {
                *captured_server.lock().unwrap() = request;
                stream
                    .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                    .unwrap();
            } else if request_index == 1 {
                let body = r#"{"now":"node / ? # %"}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            } else {
                stream
                    .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                    .unwrap();
            }
        }
    });

    let selector = MihomoHttpSelector::new(address, "private-secret").unwrap();
    selector
        .select("group / ? # %", "node / ? # %")
        .await
        .unwrap();
    server.join().unwrap();
    let request = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
    assert!(
        request.starts_with("PUT /proxies/group%20%2F%20%3F%20%23%20%25 HTTP/1.1\r\n"),
        "{request}"
    );
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer private-secret\r\n")
    );
    assert!(request.ends_with(r#"{"name":"node / ? # %"}"#));
}

#[tokio::test]
async fn http_selector_rejects_an_ack_without_live_selection_confirmation() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 2048];
        let _ = stream.read(&mut request).unwrap();
        stream
            .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
            .unwrap();
    });

    let selector = MihomoHttpSelector::new(address, "private-secret").unwrap();
    let result = selector.select("group", "requested-node").await;

    server.join().unwrap();
    assert!(
        result.is_err(),
        "a bare 204 must not prove the live route changed"
    );
}

#[tokio::test]
async fn http_selector_closes_existing_connections_after_confirmed_selection() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        for response in [
            "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".to_owned(),
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 24\r\nConnection: close\r\n\r\n{\"now\":\"requested-node\"}".to_owned(),
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).unwrap();
            stream.write_all(response.as_bytes()).unwrap();
        }

        listener.set_nonblocking(true).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_millis(750);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut request = [0_u8; 2048];
                    let count = stream.read(&mut request).unwrap();
                    stream
                        .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                        .unwrap();
                    return Some(String::from_utf8_lossy(&request[..count]).into_owned());
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept cleanup request: {error}"),
            }
        }
    });

    let selector = MihomoHttpSelector::new(address, "private-secret").unwrap();
    selector.select("group", "requested-node").await.unwrap();

    let cleanup = server.join().unwrap();
    assert!(
        cleanup
            .as_deref()
            .is_some_and(|request| request.starts_with("DELETE /connections HTTP/1.1\r\n")),
        "route changes must retire connections that still use the old selector"
    );
}

#[tokio::test]
async fn confirmed_selection_survives_connection_cleanup_failure() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        for response in [
            "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".to_owned(),
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 24\r\nConnection: close\r\n\r\n{\"now\":\"requested-node\"}".to_owned(),
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).unwrap();
            stream.write_all(response.as_bytes()).unwrap();
        }
    });

    let selector = MihomoHttpSelector::new(address, "private-secret").unwrap();
    let result = selector.select("group", "requested-node").await;

    server.join().unwrap();
    assert!(
        result.is_ok(),
        "confirmed live selection must remain successful"
    );
}

#[test]
fn http_selector_rejects_non_loopback_address() {
    assert!(MihomoHttpSelector::new("192.0.2.1:19090".parse().unwrap(), "secret").is_err());
}
