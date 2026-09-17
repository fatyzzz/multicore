use std::{future::Future, pin::Pin, sync::Arc};

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use multicore_core::{
    Engine, FetchError, HttpClient, HttpResponse, PersistentSnapshotStore, ProcessController,
    Snapshot, SubscriptionFetcher, UA_MIHOMO, UA_NATIVE, UA_XRAY,
};
use multicore_daemon::{
    Backend, CatalogDto, CoreBackend, ImportSubscriptionRequest, MAX_CATALOG_GROUPS,
    MAX_CATALOG_NAME_CHARS, MAX_CATALOG_NODES_PER_GROUP, MemoryBackend, router,
};
use serde_json::{Value, json};
use tower::ServiceExt;

const TOKEN: &str = "catalog-test-token";

struct NeverHttp;

impl HttpClient for NeverHttp {
    fn get<'a>(
        &'a self,
        _url: &'a str,
        _user_agent: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, FetchError>> + Send + 'a>> {
        Box::pin(async { Err(FetchError::Network) })
    }
}

struct FixtureHttp {
    mihomo: String,
}

impl HttpClient for FixtureHttp {
    fn get<'a>(
        &'a self,
        _url: &'a str,
        user_agent: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, FetchError>> + Send + 'a>> {
        Box::pin(async move {
            Ok(match user_agent {
                UA_NATIVE => HttpResponse::new(404, Vec::new(), std::iter::empty::<(&str, &str)>()),
                UA_MIHOMO => HttpResponse::new(
                    200,
                    self.mihomo.as_bytes().to_vec(),
                    std::iter::empty::<(&str, &str)>(),
                ),
                UA_XRAY => {
                    HttpResponse::new(200, br#"{}"#.to_vec(), std::iter::empty::<(&str, &str)>())
                }
                _ => return Err(FetchError::Network),
            })
        })
    }
}

struct NoopController;

impl ProcessController for NoopController {
    type Error = ();

    fn start<'a>(
        &'a self,
        _engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }

    fn stop<'a>(
        &'a self,
        _engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

fn core_backend(
    mihomo: &str,
) -> CoreBackend<
    NeverHttp,
    NoopController,
    impl Fn(&Snapshot) -> Result<Arc<NoopController>, multicore_daemon::BackendError> + use<>,
> {
    let directory = tempfile::tempdir().unwrap();
    let store = PersistentSnapshotStore::open(directory.path().join("snapshots")).unwrap();
    store
        .commit(Snapshot::parse(mihomo.as_bytes(), br#"{}"#).unwrap())
        .unwrap();
    CoreBackend::new(store, SubscriptionFetcher::new(NeverHttp), |_snapshot| {
        Ok(Arc::new(NoopController))
    })
    .unwrap()
}

fn authenticated_request(uri: &str, body: Body) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(body)
        .unwrap()
}

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn catalog_is_authenticated_and_empty_without_a_profile() {
    let app = router(MemoryBackend::default(), TOKEN);
    let unauthorized = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/catalog")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let response = app
        .oneshot(authenticated_request("/v1/catalog", Body::empty()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(response).await,
        json!({"revision": 0, "groups": []})
    );
}

#[tokio::test]
async fn catalog_returns_only_bounded_mihomo_names_and_selection_state() {
    let secret = "xray-secret-must-not-leak";
    let mihomo = r#"
proxies:
  - name: NL One
    type: socks5
    server: secret.provider.invalid
    password: subscription-secret
  - name: US Two
    type: socks5
    server: other.invalid
proxy-groups:
  - name: Main
    type: select
    proxies: [NL One, US Two]
  - name: Backup
    type: select
    proxies: [US Two]
rules:
  - MATCH,Main
"#;
    let directory = tempfile::tempdir().unwrap();
    let store = PersistentSnapshotStore::open(directory.path().join("snapshots")).unwrap();
    store
        .commit(
            Snapshot::parse(
                mihomo.as_bytes(),
                format!(r#"{{"secret":"{secret}"}}"#).as_bytes(),
            )
            .unwrap(),
        )
        .unwrap();
    let backend = CoreBackend::new(store, SubscriptionFetcher::new(NeverHttp), |_snapshot| {
        Ok(Arc::new(NoopController))
    })
    .unwrap();

    let catalog = backend.catalog().await.unwrap();
    assert_eq!(catalog.revision, 1);
    assert_eq!(catalog.groups.len(), 2);
    assert!(catalog.groups[0].selected);
    assert_eq!(catalog.groups[0].id, "g-0000000000000001-0000");
    assert_eq!(catalog.groups[0].label, "Main");
    assert_eq!(catalog.groups[0].nodes.len(), 2);
    assert_eq!(
        catalog.groups[0].nodes[0].id,
        "n-0000000000000001-0000-0000"
    );
    assert_eq!(catalog.groups[0].nodes[0].label, "NL One");
    assert!(catalog.groups[0].nodes[0].selected);
    assert_eq!(catalog.groups[0].nodes[0].delay_ms, None);
    assert!(!catalog.groups[0].nodes[1].selected);
    assert!(!catalog.groups[1].selected);
    assert!(catalog.groups[1].nodes[0].selected);

    let response = router(backend, TOKEN)
        .oneshot(authenticated_request("/v1/catalog", Body::empty()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(
        body,
        json!({
            "revision": 1,
            "groups": [
                {
                    "id": "g-0000000000000001-0000",
                    "label": "Main",
                    "selected": true,
                    "nodes": [
                        {"id": "n-0000000000000001-0000-0000", "label": "NL One", "selected": true},
                        {"id": "n-0000000000000001-0000-0001", "label": "US Two", "selected": false}
                    ]
                },
                {
                    "id": "g-0000000000000001-0001",
                    "label": "Backup",
                    "selected": false,
                    "nodes": [
                        {"id": "n-0000000000000001-0001-0000", "label": "US Two", "selected": true}
                    ]
                }
            ]
        })
    );
    let serialized = body.to_string();
    for forbidden in [
        "secret.provider.invalid",
        "subscription-secret",
        "MATCH,Main",
        secret,
        "xray",
        "raw",
        "url",
    ] {
        assert!(!serialized.contains(forbidden), "leaked {forbidden}");
    }
}

#[tokio::test]
async fn catalog_conversion_caps_untrusted_counts_and_unicode_names() {
    assert_eq!(MAX_CATALOG_GROUPS, 64);
    assert_eq!(MAX_CATALOG_NODES_PER_GROUP, 256);
    let long_name = "я".repeat(MAX_CATALOG_NAME_CHARS + 7);
    let nodes = (0..MAX_CATALOG_NODES_PER_GROUP + 7)
        .map(|index| format!("{long_name}-{index}"))
        .collect::<Vec<_>>();
    let mut mihomo = String::from("proxy-groups:\n");
    for group_index in 0..MAX_CATALOG_GROUPS + 7 {
        mihomo.push_str(&format!(
            "  - name: '{long_name}-{group_index}'\n    type: select\n    proxies:\n"
        ));
        let group_nodes: &[String] = if group_index == 0 {
            &nodes
        } else {
            &nodes[..1]
        };
        for node in group_nodes {
            mihomo.push_str(&format!("      - '{node}'\n"));
        }
    }
    let backend = core_backend(&mihomo);

    let CatalogDto { revision, groups } = backend.catalog().await.unwrap();
    assert_eq!(revision, 1);
    assert_eq!(groups.len(), MAX_CATALOG_GROUPS);
    assert_eq!(groups[0].nodes.len(), MAX_CATALOG_NODES_PER_GROUP);
    assert_eq!(groups[0].label.chars().count(), MAX_CATALOG_NAME_CHARS);
    assert_eq!(
        groups[0].nodes[0].label.chars().count(),
        MAX_CATALOG_NAME_CHARS
    );
}

#[tokio::test]
async fn catalog_rejects_request_bodies_like_other_read_only_routes() {
    let response = router(MemoryBackend::default(), TOKEN)
        .oneshot(authenticated_request(
            "/v1/catalog",
            Body::from("unexpected"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = json_body(response).await;
    assert_eq!(body["code"], "invalid_request");
    assert!(
        body["correlation_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
}

#[tokio::test]
async fn catalog_genericizes_suspicious_presentation_names() {
    let uuid = "550e8400-e29b-41d4-a716-446655440000";
    let mihomo = format!(
        r#"
proxies:
  - name: 'ftp://credential-user:credential-pass@node-secret.invalid/file'
    type: socks5
  - name: 'Authorization: Basic dXNlcjpwYXNz'
    type: socks5
  - name: 'token="quoted multiword secret"'
    type: socks5
  - name: "Control\aName"
    type: socks5
  - name: "TrailingControl\t"
    type: socks5
  - name: '{uuid}'
    type: socks5
  - name: "Line\LSeparator"
    type: socks5
  - name: "Paragraph\PSeparator"
    type: socks5
  - name: "New\nLine"
    type: socks5
proxy-groups:
  - name: 'https://group-secret.invalid/sub?token=query-secret&uuid={uuid}'
    type: select
    proxies:
      - 'ftp://credential-user:credential-pass@node-secret.invalid/file'
      - 'Authorization: Basic dXNlcjpwYXNz'
      - 'token="quoted multiword secret"'
      - "Control\aName"
      - "TrailingControl\t"
      - '{uuid}'
      - "Line\LSeparator"
      - "Paragraph\PSeparator"
      - "New\nLine"
"#
    );
    let backend = core_backend(&mihomo);

    let catalog = backend.catalog().await.unwrap();
    assert_eq!(catalog.groups[0].label, "Группа 1");
    assert_eq!(
        catalog.groups[0]
            .nodes
            .iter()
            .map(|node| node.label.as_str())
            .collect::<Vec<_>>(),
        [
            "Узел 1",
            "Узел 2",
            "Узел 3",
            "Узел 4",
            "Узел 5",
            "Узел 6",
            "Узел 7",
            "Узел 8",
            "Узел 9",
        ]
    );
    let serialized = serde_json::to_string(&catalog).unwrap();
    for forbidden in [
        "group-secret.invalid",
        "node-secret.invalid",
        "ftp://",
        "Basic",
        "credential-user",
        "credential-pass",
        "query-secret",
        "quoted multiword secret",
        "Control",
        "TrailingControl",
        "Line",
        "Paragraph",
        "New",
        uuid,
    ] {
        assert!(!serialized.contains(forbidden), "leaked {forbidden}");
    }
}

#[tokio::test]
async fn catalog_blocks_allowed_character_credential_bypasses_but_keeps_ordinary_names() {
    let mihomo = r#"
proxies:
  - {name: Authorization Basic dXNlcjpwYXNz, type: socks5}
  - {name: proxy-authorization bearer abcdef, type: socks5}
  - {name: token abcdef, type: socks5}
  - {name: password hunter2, type: socks5}
  - {name: passwd hunter2, type: socks5}
  - {name: secret abcdef, type: socks5}
  - {name: api-key abcdef, type: socks5}
  - {name: subscription abcdef, type: socks5}
  - {name: dXNlcjpwYXNz, type: socks5}
  - {name: Singapore Premium, type: socks5}
  - {name: Tokyo 01, type: socks5}
proxy-groups:
  - name: Main
    type: select
    proxies:
      - Authorization Basic dXNlcjpwYXNz
      - proxy-authorization bearer abcdef
      - token abcdef
      - password hunter2
      - passwd hunter2
      - secret abcdef
      - api-key abcdef
      - subscription abcdef
      - dXNlcjpwYXNz
      - Singapore Premium
      - Tokyo 01
"#;
    let catalog = core_backend(mihomo).catalog().await.unwrap();
    let labels: Vec<_> = catalog.groups[0]
        .nodes
        .iter()
        .map(|node| node.label.as_str())
        .collect();
    for (index, label) in labels.iter().take(9).enumerate() {
        assert_eq!(*label, format!("Узел {}", index + 1));
    }
    assert_eq!(labels[9], "Singapore Premium");
    assert_eq!(labels[10], "Tokyo 01");
}

#[tokio::test]
async fn catalog_preserves_realistic_mihomo_flags_emoji_and_group_names() {
    let mihomo = r#"
proxies:
  - {name: youtube-selected-loopback-in, type: socks5}
proxy-groups:
  - name: "🇦🇱 Albania [al]"
    type: select
    proxies: [youtube-selected-loopback-in]
  - name: "🌍 Сервер"
    type: select
    proxies: ["🇦🇱 Albania [al]", "🎮 Игры", "Без VPN"]
  - name: "🇷🇺 Российские сайты"
    type: select
    proxies: ["Без VPN"]
  - name: "Minecraft (javaw.exe)"
    type: select
    proxies: ["🎮 Игры"]
"#;

    let catalog = core_backend(mihomo).catalog().await.unwrap();
    assert_eq!(
        catalog
            .groups
            .iter()
            .map(|group| group.label.as_str())
            .collect::<Vec<_>>(),
        [
            "🇦🇱 Albania [al]",
            "🌍 Сервер",
            "🇷🇺 Российские сайты",
            "Minecraft (javaw.exe)",
        ]
    );
    assert_eq!(
        catalog.groups[1]
            .nodes
            .iter()
            .map(|node| node.label.as_str())
            .collect::<Vec<_>>(),
        ["🇦🇱 Albania [al]", "🎮 Игры", "Без VPN"]
    );
}

#[tokio::test]
async fn catalog_detects_bounded_embedded_base64_credential_tokens() {
    let huge_suffix = "A".repeat(1024 * 1024);
    let mihomo = format!(
        r#"
proxies:
  - {{name: Edge-dXNlcjpwYXNz, type: socks5}}
  - {{name: Prefix(dXNlcjpwYXNz)Suffix, type: socks5}}
  - {{name: neutral-dXNlcjpwYXNz-node, type: socks5}}
  - {{name: ordinary-edge-node, type: socks5}}
  - {{name: Edge-dXNlcjpwYXNz-{huge_suffix}, type: socks5}}
proxy-groups:
  - name: Main
    type: select
    proxies:
      - Edge-dXNlcjpwYXNz
      - Prefix(dXNlcjpwYXNz)Suffix
      - neutral-dXNlcjpwYXNz-node
      - ordinary-edge-node
      - Edge-dXNlcjpwYXNz-{huge_suffix}
"#
    );
    let catalog = core_backend(&mihomo).catalog().await.unwrap();
    let labels: Vec<_> = catalog.groups[0]
        .nodes
        .iter()
        .map(|node| node.label.as_str())
        .collect();
    assert_eq!(&labels[..3], ["Узел 1", "Узел 2", "Узел 3"]);
    assert_eq!(labels[3], "ordinary-edge-node");
    assert_eq!(labels[4], "Узел 5");
}

#[tokio::test]
async fn catalog_blocks_base64url_hyphen_candidates_and_short_credentials() {
    let mihomo = r#"
proxies:
  - {name: Edge-fn5-On5-fg, type: socks5}
  - {name: YTpi, type: socks5}
  - {name: ordinary-edge-node, type: socks5}
proxy-groups:
  - name: Main
    type: select
    proxies:
      - Edge-fn5-On5-fg
      - YTpi
      - ordinary-edge-node
"#;
    let catalog = core_backend(mihomo).catalog().await.unwrap();
    let labels: Vec<_> = catalog.groups[0]
        .nodes
        .iter()
        .map(|node| node.label.as_str())
        .collect();
    assert_eq!(labels, ["Узел 1", "Узел 2", "ordinary-edge-node"]);
}

#[tokio::test]
async fn catalog_blocks_embedded_credentials_without_punctuation_boundaries() {
    let mihomo = r#"
proxies:
  - {name: Edge_dXNlcjpwYXNz_node, type: socks5}
  - {name: EdgedXNlcjpwYXNzNode, type: socks5}
  - {name: Edge+dXNlcjpwYXNz+node, type: socks5}
  - {name: ordinary_edge_node, type: socks5}
proxy-groups:
  - name: Main
    type: select
    proxies:
      - Edge_dXNlcjpwYXNz_node
      - EdgedXNlcjpwYXNzNode
      - Edge+dXNlcjpwYXNz+node
      - ordinary_edge_node
"#;
    let catalog = core_backend(mihomo).catalog().await.unwrap();
    let labels: Vec<_> = catalog.groups[0]
        .nodes
        .iter()
        .map(|node| node.label.as_str())
        .collect();
    assert_eq!(labels, ["Узел 1", "Узел 2", "Узел 3", "ordinary_edge_node"]);
}

#[tokio::test]
async fn presentation_preserves_ordinary_major_labels() {
    let mihomo = r#"
proxies:
  - {name: Major, type: socks5}
  - {name: major, type: socks5}
  - {name: Majors, type: socks5}
  - {name: Major-Node, type: socks5}
proxy-groups:
  - name: Major
    type: select
    proxies: [Major, major, Majors, Major-Node]
"#;
    let catalog = core_backend(mihomo).catalog().await.unwrap();
    assert_eq!(catalog.groups[0].label, "Major");
    assert_eq!(
        catalog.groups[0]
            .nodes
            .iter()
            .map(|node| node.label.as_str())
            .collect::<Vec<_>>(),
        ["Major", "major", "Majors", "Major-Node"]
    );

    let status_backend = core_backend(&mihomo_with_selected_scalar("'Major'"));
    let status_response = router(status_backend, TOKEN)
        .oneshot(authenticated_request("/v1/status", Body::empty()))
        .await
        .unwrap();
    let status = json_body(status_response).await;
    assert_eq!(status["profile"], "Major");
    assert_eq!(status["current_node"], "Major");

    let directory = tempfile::tempdir().unwrap();
    let store = PersistentSnapshotStore::open(directory.path().join("snapshots")).unwrap();
    let import_backend = CoreBackend::new(
        store,
        SubscriptionFetcher::new(FixtureHttp {
            mihomo: mihomo_with_selected_scalar("'Major'"),
        }),
        |_snapshot| Ok(Arc::new(NoopController)),
    )
    .unwrap();
    let import_response = router(import_backend, TOKEN)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/subscriptions/import")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"url":"https://example.invalid/sub"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let imported = json_body(import_response).await;
    assert_eq!(imported["profile"], "Major");
    assert_eq!(imported["current_node"], "Major");
}

#[tokio::test]
async fn presentation_preserves_tokyo_labels_with_binary_base64_decodes() {
    let mihomo = r#"
proxies:
  - {name: UltraTokyo01, type: socks5}
  - {name: UltraTokyo, type: socks5}
  - {name: CloudTokyo, type: socks5}
  - {name: UnlimitedTokyo, type: socks5}
proxy-groups:
  - name: UltraTokyo01
    type: select
    proxies: [UltraTokyo01, UltraTokyo, CloudTokyo, UnlimitedTokyo]
"#;
    let catalog = core_backend(mihomo).catalog().await.unwrap();
    assert_eq!(catalog.groups[0].label, "UltraTokyo01");
    assert_eq!(
        catalog.groups[0]
            .nodes
            .iter()
            .map(|node| node.label.as_str())
            .collect::<Vec<_>>(),
        ["UltraTokyo01", "UltraTokyo", "CloudTokyo", "UnlimitedTokyo"]
    );

    for raw_label in ["UltraTokyo01", "UltraTokyo", "CloudTokyo", "UnlimitedTokyo"] {
        let yaml_scalar = format!("'{raw_label}'");
        let status_backend = core_backend(&mihomo_with_selected_scalar(&yaml_scalar));
        let status_response = router(status_backend, TOKEN)
            .oneshot(authenticated_request("/v1/status", Body::empty()))
            .await
            .unwrap();
        let status = json_body(status_response).await;
        assert_eq!(status["profile"], raw_label);
        assert_eq!(status["current_node"], raw_label);

        let directory = tempfile::tempdir().unwrap();
        let store = PersistentSnapshotStore::open(directory.path().join("snapshots")).unwrap();
        let import_backend = CoreBackend::new(
            store,
            SubscriptionFetcher::new(FixtureHttp {
                mihomo: mihomo_with_selected_scalar(&yaml_scalar),
            }),
            |_snapshot| Ok(Arc::new(NoopController)),
        )
        .unwrap();
        let import_response = router(import_backend, TOKEN)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/subscriptions/import")
                    .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"url":"https://example.invalid/sub"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let imported = json_body(import_response).await;
        assert_eq!(imported["profile"], raw_label);
        assert_eq!(imported["current_node"], raw_label);
    }
}

fn mihomo_with_selected_scalar(yaml_scalar: &str) -> String {
    format!(
        "proxies:\n  - name: {yaml_scalar}\n    type: socks5\nproxy-groups:\n  - name: {yaml_scalar}\n    type: select\n    proxies:\n      - {yaml_scalar}\n"
    )
}

#[tokio::test]
async fn authenticated_status_never_exposes_raw_selected_names() {
    for yaml_scalar in [
        "'ftp://credential:password@example.invalid/path'",
        "'Authorization: Basic dXNlcjpwYXNz'",
        "'token=\"quoted multiword secret\"'",
        "'Edge-fn5-On5-fg'",
        "'YTpi'",
        "'Edge_dXNlcjpwYXNz_node'",
        "'EdgedXNlcjpwYXNzNode'",
        "\"Control\\tName\"",
        "'550e8400-e29b-41d4-a716-446655440000'",
    ] {
        let backend = core_backend(&mihomo_with_selected_scalar(yaml_scalar));
        let response = router(backend, TOKEN)
            .oneshot(authenticated_request("/v1/status", Body::empty()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["profile"], "Группа 1");
        assert_eq!(body["current_node"], "Узел 1");
    }
}

#[tokio::test]
async fn authenticated_import_response_never_exposes_raw_selected_names() {
    for yaml_scalar in [
        "'ftp://credential:password@example.invalid/path'",
        "'Authorization: Basic dXNlcjpwYXNz'",
        "'token=\"quoted multiword secret\"'",
        "'Edge-fn5-On5-fg'",
        "'YTpi'",
        "'Edge_dXNlcjpwYXNz_node'",
        "'EdgedXNlcjpwYXNzNode'",
        "\"Control\\tName\"",
        "'550e8400-e29b-41d4-a716-446655440000'",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let store = PersistentSnapshotStore::open(directory.path().join("snapshots")).unwrap();
        let backend = CoreBackend::new(
            store,
            SubscriptionFetcher::new(FixtureHttp {
                mihomo: mihomo_with_selected_scalar(yaml_scalar),
            }),
            |_snapshot| Ok(Arc::new(NoopController)),
        )
        .unwrap();
        let response = router(backend, TOKEN)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/subscriptions/import")
                    .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"url":"https://example.invalid/sub"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["profile"], "Группа 1");
        assert_eq!(body["current_node"], "Узел 1");
    }
}

#[tokio::test]
async fn duplicate_controller_names_emit_only_the_first_addressable_identity() {
    let mihomo = r#"
proxies:
  - name: Same Node
    type: socks5
proxy-groups:
  - name: Same Group
    type: select
    proxies: [Same Node, Same Node]
  - name: Same Group
    type: select
    proxies: [Same Node]
"#;
    let catalog = core_backend(mihomo).catalog().await.unwrap();

    assert_eq!(catalog.groups.len(), 1);
    assert!(catalog.groups[0].selected);
    assert_eq!(catalog.groups[0].nodes.len(), 1);
    assert!(catalog.groups[0].nodes[0].selected);
}

#[tokio::test]
async fn catalog_includes_only_interactive_select_groups() {
    let mihomo = r#"
proxies:
  - {name: One, type: socks5}
proxy-groups:
  - {name: Latency, type: url-test, proxies: [One]}
  - {name: Failover, type: fallback, proxies: [One]}
  - {name: Balance, type: load-balance, proxies: [One]}
  - {name: Interactive, type: SeLeCt, proxies: [One]}
"#;
    let backend = core_backend(mihomo);
    let catalog = backend.catalog().await.unwrap();
    assert_eq!(catalog.groups.len(), 1);
    assert_eq!(catalog.groups[0].label, "Interactive");
}

#[tokio::test]
async fn first_raw_group_name_wins_before_select_type_filtering() {
    for first_type in ["url-test", "fallback", "load-balance"] {
        let mihomo = format!(
            r#"
proxies:
  - {{name: One, type: socks5}}
proxy-groups:
  - {{name: Duplicate, type: {first_type}, proxies: [One]}}
  - {{name: Duplicate, type: select, proxies: [One]}}
  - {{name: Kept, type: select, proxies: [One]}}
"#
        );
        let catalog = core_backend(&mihomo).catalog().await.unwrap();
        assert_eq!(catalog.groups.len(), 1);
        assert_eq!(catalog.groups[0].label, "Kept");
    }
}

#[tokio::test]
async fn truncated_label_collisions_keep_distinct_opaque_identity() {
    let prefix = "A".repeat(MAX_CATALOG_NAME_CHARS);
    let first = format!("{prefix}X");
    let second = format!("{prefix}Y");
    let mihomo = format!(
        r#"
proxies:
  - name: '{first}'
    type: socks5
  - name: '{second}'
    type: socks5
proxy-groups:
  - name: '{first}'
    type: select
    proxies: ['{first}', '{second}']
  - name: '{second}'
    type: select
    proxies: ['{second}']
"#
    );
    let catalog = core_backend(&mihomo).catalog().await.unwrap();

    assert_eq!(catalog.groups[0].label, catalog.groups[1].label);
    assert_ne!(catalog.groups[0].id, catalog.groups[1].id);
    assert_eq!(
        catalog.groups[0].nodes[0].label,
        catalog.groups[0].nodes[1].label
    );
    assert_ne!(catalog.groups[0].nodes[0].id, catalog.groups[0].nodes[1].id);
    assert!(catalog.groups[0].selected);
    assert!(catalog.groups[0].nodes[0].selected);
    assert!(!catalog.groups[0].nodes[1].selected);
}

#[tokio::test]
async fn activating_a_new_snapshot_increments_revision_and_rekeys_ids() {
    let initial = r#"
proxies:
  - name: Initial Node
    type: socks5
proxy-groups:
  - name: Initial Group
    type: select
    proxies: [Initial Node]
"#;
    let replacement = r#"
proxies:
  - name: Replacement Node
    type: socks5
proxy-groups:
  - name: Replacement Group
    type: select
    proxies: [Replacement Node]
"#;
    let directory = tempfile::tempdir().unwrap();
    let store = PersistentSnapshotStore::open(directory.path().join("snapshots")).unwrap();
    store
        .commit(Snapshot::parse(initial.as_bytes(), br#"{}"#).unwrap())
        .unwrap();
    let backend = CoreBackend::new(
        store,
        SubscriptionFetcher::new(FixtureHttp {
            mihomo: replacement.to_owned(),
        }),
        |_snapshot| Ok(Arc::new(NoopController)),
    )
    .unwrap();

    let before = backend.catalog().await.unwrap();
    backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://example.invalid/sub".into(),
        })
        .await
        .unwrap();
    let after = backend.catalog().await.unwrap();

    assert_eq!(before.revision, 1);
    assert_eq!(after.revision, 2);
    assert_ne!(before.groups[0].id, after.groups[0].id);
    assert_ne!(before.groups[0].nodes[0].id, after.groups[0].nodes[0].id);
    assert_eq!(after.groups[0].label, "Replacement Group");
    assert_eq!(after.groups[0].nodes[0].label, "Replacement Node");
}

#[tokio::test]
async fn reconstructed_daemon_uses_persisted_generation_for_revision_and_ids() {
    let first = mihomo_with_selected_scalar("'First Node'");
    let second = mihomo_with_selected_scalar("'Second Node'");
    let directory = tempfile::tempdir().unwrap();
    let snapshots = directory.path().join("snapshots");

    let store = PersistentSnapshotStore::open(&snapshots).unwrap();
    store
        .commit(Snapshot::parse(first.as_bytes(), br#"{}"#).unwrap())
        .unwrap();
    let first_backend = CoreBackend::new(store, SubscriptionFetcher::new(NeverHttp), |_snapshot| {
        Ok(Arc::new(NoopController))
    })
    .unwrap();
    let first_catalog = first_backend.catalog().await.unwrap();
    drop(first_backend);

    let store = PersistentSnapshotStore::open(&snapshots).unwrap();
    store
        .commit(Snapshot::parse(second.as_bytes(), br#"{}"#).unwrap())
        .unwrap();
    drop(store);
    let store = PersistentSnapshotStore::open(&snapshots).unwrap();
    let second_backend =
        CoreBackend::new(store, SubscriptionFetcher::new(NeverHttp), |_snapshot| {
            Ok(Arc::new(NoopController))
        })
        .unwrap();
    let second_catalog = second_backend.catalog().await.unwrap();

    assert_eq!(first_catalog.revision, 1);
    assert_eq!(second_catalog.revision, 2);
    assert_ne!(first_catalog.groups[0].id, second_catalog.groups[0].id);
    assert_ne!(
        first_catalog.groups[0].nodes[0].id,
        second_catalog.groups[0].nodes[0].id
    );
}
