use std::{
    collections::VecDeque,
    fs,
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use multicore_core::{
    AtomicSnapshot, FetchError, HttpClient, HttpResponse, PersistentSnapshotStore,
    ReqwestHttpClient, Snapshot, SubscriptionFetcher, UA_MIHOMO, UA_NATIVE, UA_XRAY,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const MIHOMO: &str = "proxies:\n  - name: Node\n    type: socks5\n    server: 127.0.0.1\n    port: 30001\nproxy-groups:\n  - name: Proxy\n    type: select\n    proxies: [Node]\nrules: [MATCH,Proxy]\n";
const XRAY: &str = "{\n  \"provider-owned\": true\n}";
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new() -> Self {
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "multicore-metadata-test-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone)]
struct FakeHttp {
    responses: Arc<Mutex<VecDeque<Result<HttpResponse, FetchError>>>>,
    agents: Arc<Mutex<Vec<&'static str>>>,
}

impl FakeHttp {
    fn new(responses: Vec<Result<HttpResponse, FetchError>>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into())),
            agents: Arc::default(),
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
            self.agents.lock().unwrap().push(user_agent);
            self.responses.lock().unwrap().pop_front().unwrap()
        })
    }
}

fn response(
    status: u16,
    body: impl Into<Vec<u8>>,
    headers: impl IntoIterator<Item = (&'static str, &'static str)>,
) -> Result<HttpResponse, FetchError> {
    Ok(HttpResponse::new(status, body.into(), headers))
}

fn bundle() -> Vec<u8> {
    format!("[{},{}]", serde_json::to_string(MIHOMO).unwrap(), XRAY).into_bytes()
}

async fn native_info(
    headers: impl IntoIterator<Item = (&'static str, &'static str)>,
) -> Arc<multicore_core::Snapshot> {
    let store = AtomicSnapshot::default();
    SubscriptionFetcher::new(FakeHttp::new(vec![response(200, bundle(), headers)]))
        .refresh("https://example.invalid/private-token", &store)
        .await
        .unwrap();
    store.current().unwrap()
}

#[tokio::test]
async fn title_decodes_plain_standard_and_urlsafe_base64_with_service_name_fallback() {
    let plain = native_info([("PrOfIlE-TiTlE", "  North   Star  ")]).await;
    assert_eq!(
        plain.subscription_info().unwrap().display_name,
        "North Star"
    );

    let standard = native_info([("profile-title", "base64:0J/RgNC+0LLQsNC50LTQtdGA")]).await;
    assert_eq!(
        standard.subscription_info().unwrap().display_name,
        "Провайдер"
    );

    let url_safe = native_info([("profile-title", "base64:8J-SqfCfjI3wn4ev")]).await;
    assert_eq!(url_safe.subscription_info().unwrap().display_name, "💩🌍🇯");

    let fallback = native_info([
        ("profile-title", "base64:not valid!"),
        ("flclashx-servicename", "Fallback ISP"),
    ])
    .await;
    assert_eq!(
        fallback.subscription_info().unwrap().display_name,
        "Fallback ISP"
    );
}

#[tokio::test]
async fn reqwest_transport_accepts_raw_utf8_header_bytes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let body = bundle();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = vec![0_u8; 4096];
        let _ = stream.read(&mut request).await.unwrap();
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nprofile-title: ",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice("Провайдер".as_bytes());
        response.extend_from_slice(b"\r\nConnection: close\r\n\r\n");
        response.extend_from_slice(&body);
        stream.write_all(&response).await.unwrap();
    });
    let store = AtomicSnapshot::default();
    SubscriptionFetcher::new(ReqwestHttpClient::new().unwrap())
        .refresh(&format!("http://{address}/subscription"), &store)
        .await
        .unwrap();
    server.await.unwrap();
    assert_eq!(
        store
            .current()
            .unwrap()
            .subscription_info()
            .unwrap()
            .display_name,
        "Провайдер"
    );
}

#[tokio::test]
async fn parses_prefixed_usage_saturates_used_bytes_and_clamps_interval() {
    let low = native_info([
        (
            "vendor-subscription-userinfo",
            "upload=18446744073709551610; download=42; total=18446744073709551615; expire=0",
        ),
        ("profile-update-interval", "0"),
    ])
    .await;
    let info = low.subscription_info().unwrap();
    assert_eq!(info.uploaded_bytes, Some(u64::MAX - 5));
    assert_eq!(info.downloaded_bytes, Some(42));
    assert_eq!(info.used_bytes(), Some(u64::MAX));
    assert_eq!(info.total_bytes, Some(u64::MAX));
    assert_eq!(info.expires_at_unix, None);
    assert_eq!(info.refresh_interval_secs, Some(15 * 60));

    let high = native_info([("profile-update-interval", "99999999999999999999")]).await;
    assert_eq!(
        high.subscription_info().unwrap().refresh_interval_secs,
        Some(30 * 24 * 60 * 60)
    );
}

#[tokio::test]
async fn announcements_are_aliased_sanitized_bounded_and_semantically_toned() {
    let long = "x".repeat(600);
    let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, long);
    let leaked: &'static str = Box::leak(format!("base64:{encoded}").into_boxed_str());
    let snapshot = native_info([
        ("announce", "base64:%%%"),
        ("sub-info-text", leaked),
        (
            "sub-info-button-text",
            "  Open\u{202e}   support now please  ",
        ),
        ("sub-info-color", "ReD"),
    ])
    .await;
    let info = snapshot.subscription_info().unwrap();
    assert_eq!(
        info.announcement_text.as_ref().unwrap().chars().count(),
        512
    );
    assert_eq!(
        info.announcement_action_label.as_deref(),
        Some("Open support now please")
    );
    assert_eq!(
        info.announcement_tone.map(|tone| tone.as_str()),
        Some("danger")
    );

    let aliases = native_info([
        ("banner-text", " Maintenance    tonight "),
        ("sub-info-color", "pink"),
    ])
    .await;
    let info = aliases.subscription_info().unwrap();
    assert_eq!(
        info.announcement_text.as_deref(),
        Some("Maintenance tonight")
    );
    assert_eq!(info.announcement_tone, None);
}

#[tokio::test]
async fn validates_provider_support_action_and_https_logo_urls() {
    let snapshot = native_info([
        ("profile-web-page-url", "https://provider.example/home"),
        ("support-url", "tg://resolve?domain=provider_help"),
        ("announce-url", "http://provider.example/news"),
        ("flclashx-servicelogo", "https://cdn.example/logo.png"),
    ])
    .await;
    assert_eq!(
        snapshot.subscription_home_url(),
        Some("https://provider.example/home")
    );
    assert_eq!(
        snapshot.subscription_support_url(),
        Some("tg://resolve?domain=provider_help")
    );
    assert_eq!(
        snapshot.subscription_announcement_url(),
        Some("http://provider.example/news")
    );
    assert_eq!(
        snapshot.subscription_logo_url(),
        Some("https://cdn.example/logo.png")
    );

    let unsafe_urls = native_info([
        (
            "profile-web-page-url",
            "https://user:secret@provider.example",
        ),
        ("support-url", "javascript:alert(1)"),
        ("banner-button-url", "file:///etc/passwd"),
        ("flclashx-servicelogo", "http://cdn.example/logo.png"),
    ])
    .await;
    assert_eq!(unsafe_urls.subscription_home_url(), None);
    assert_eq!(unsafe_urls.subscription_support_url(), None);
    assert_eq!(unsafe_urls.subscription_announcement_url(), None);
    assert_eq!(unsafe_urls.subscription_logo_url(), None);
}

#[tokio::test]
async fn malformed_mihomo_fields_fall_back_individually_to_xray_fields() {
    let http = FakeHttp::new(vec![
        response(503, Vec::new(), []),
        response(
            200,
            MIHOMO,
            [
                ("profile-title", "base64:%%%"),
                ("support-url", "javascript:bad"),
                ("announce", "Mihomo announcement"),
            ],
        ),
        response(
            200,
            XRAY,
            [
                ("profile-title", "Xray title"),
                ("support-url", "https://support.example/help"),
                ("announce", "Xray announcement"),
            ],
        ),
    ]);
    let store = AtomicSnapshot::default();
    SubscriptionFetcher::new(http)
        .refresh("https://fallback.example/sub", &store)
        .await
        .unwrap();
    let snapshot = store.current().unwrap();
    let info = snapshot.subscription_info().unwrap();
    assert_eq!(info.display_name, "Xray title");
    assert_eq!(
        info.announcement_text.as_deref(),
        Some("Mihomo announcement")
    );
    assert_eq!(
        snapshot.subscription_support_url(),
        Some("https://support.example/help")
    );
}

#[tokio::test]
async fn explicit_mihomo_zeroes_suppress_xray_announcement_total_and_expiry() {
    let http = FakeHttp::new(vec![
        response(503, Vec::new(), []),
        response(
            200,
            MIHOMO,
            [
                ("announce", "0"),
                ("subscription-userinfo", "total=0; expire=0"),
            ],
        ),
        response(
            200,
            XRAY,
            [
                ("announce", "Must stay hidden"),
                ("subscription-userinfo", "total=999; expire=1999999999"),
            ],
        ),
    ]);
    let store = AtomicSnapshot::default();
    SubscriptionFetcher::new(http)
        .refresh("https://fallback.example/sub", &store)
        .await
        .unwrap();
    let snapshot = store.current().unwrap();
    let info = snapshot.subscription_info().unwrap();
    assert_eq!(info.announcement_text, None);
    assert_eq!(info.total_bytes, None);
    assert_eq!(info.expires_at_unix, None);
}

#[tokio::test]
async fn credential_like_title_falls_through_to_service_name() {
    let snapshot = native_info([
        ("profile-title", "user:secret@provider.example"),
        ("flclashx-servicename", "Safe provider"),
    ])
    .await;
    assert_eq!(
        snapshot.subscription_info().unwrap().display_name,
        "Safe provider"
    );
}

#[tokio::test]
async fn control_heavy_decoded_title_falls_through_to_service_name() {
    let hostile = format!("{}X", "\u{202e}".repeat(300));
    let encoded = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        hostile.as_bytes(),
    );
    let title: &'static str = Box::leak(format!("base64:{encoded}").into_boxed_str());
    let snapshot = native_info([
        ("profile-title", title),
        ("flclashx-servicename", "Safe provider"),
    ])
    .await;
    assert_eq!(
        snapshot.subscription_info().unwrap().display_name,
        "Safe provider"
    );
}

#[tokio::test]
async fn credential_like_content_disposition_filename_falls_back_to_source_host() {
    let snapshot = native_info([(
        "content-disposition",
        "attachment; filename=\"user:secret@provider.example\"",
    )])
    .await;
    assert_eq!(
        snapshot.subscription_info().unwrap().display_name,
        "example.invalid"
    );
}

#[tokio::test]
async fn control_heavy_content_disposition_filename_falls_back_to_source_host() {
    let hostile = format!("attachment; filename=\"{}X\"", "\u{202e}".repeat(300));
    let header: &'static str = Box::leak(hostile.into_boxed_str());
    let snapshot = native_info([("content-disposition", header)]).await;
    assert_eq!(
        snapshot.subscription_info().unwrap().display_name,
        "example.invalid"
    );
}

#[tokio::test]
async fn metadata_limits_content_disposition_and_ignored_control_headers_are_enforced() {
    let oversized: &'static str = Box::leak("x".repeat(4097).into_boxed_str());
    let per_field = native_info([
        ("profile-title", oversized),
        ("flclashx-servicename", "Bounded fallback"),
    ])
    .await;
    assert_eq!(
        per_field.subscription_info().unwrap().display_name,
        "Bounded fallback"
    );

    let filler: &'static str =
        Box::leak(format!("base64:{}%%%", "A".repeat(4080)).into_boxed_str());
    let aggregate = native_info([
        ("announce", filler),
        ("sub-info-text", filler),
        ("banner-text", filler),
        ("sub-info-button-text", filler),
        ("profile-title", "Dropped by aggregate budget"),
    ])
    .await;
    assert_eq!(
        aggregate.subscription_info().unwrap().display_name,
        "example.invalid"
    );

    let filename = native_info([(
        "content-disposition",
        "attachment; filename=\"  Provider   Export  \"",
    )])
    .await;
    assert_eq!(
        filename.subscription_info().unwrap().display_name,
        "Provider Export"
    );

    let ignored = native_info([
        (
            "flclashx-background",
            "https://attacker.example/background.png",
        ),
        ("flclashx-settings", "enable-dangerous-mode"),
        ("routing", "MATCH,attacker"),
    ])
    .await;
    let info = ignored.subscription_info().unwrap();
    assert_eq!(info.display_name, "example.invalid");
    assert_eq!(info.announcement_text, None);
    assert_eq!(ignored.subscription_home_url(), None);
}

#[test]
fn old_subscription_json_defaults_new_fields_and_uses_host_as_display_name() {
    let root = TempDirectory::new();
    let store = PersistentSnapshotStore::open(&root.0).unwrap();
    store
        .commit(
            Snapshot::parse(MIHOMO.as_bytes(), XRAY.as_bytes())
                .unwrap()
                .with_subscription_source(
                    "https://legacy.example/private",
                    Some("download=7; total=100"),
                    123,
                )
                .unwrap(),
        )
        .unwrap();
    drop(store);
    let path = root
        .0
        .join("snapshot-00000000000000000001")
        .join("subscription.json");
    fs::write(
        path,
        br#"{"source_url":"https://legacy.example/private","info":{"source_host":"legacy.example","downloaded_bytes":7,"total_bytes":100,"expires_at_unix":null,"updated_at_unix":123}}"#,
    )
    .unwrap();

    let reopened = PersistentSnapshotStore::open(&root.0).unwrap();
    let current = reopened.current().unwrap();
    let info = current.subscription_info().unwrap();
    assert_eq!(info.display_name, "legacy.example");
    assert_eq!(info.uploaded_bytes, None);
    assert_eq!(info.announcement_text, None);
    assert_eq!(info.downloaded_bytes, Some(7));
}

#[test]
fn http_response_debug_redacts_body_and_metadata_values() {
    let response = HttpResponse::new(
        200,
        b"body-secret-token".to_vec(),
        [(
            "support-url",
            "https://support.example/help?token=metadata-secret-token",
        )],
    );
    let debug = format!("{response:?}");
    assert!(debug.contains("status: 200"));
    assert!(debug.contains("body_len: 17"));
    assert!(debug.contains("metadata_count: 1"));
    assert!(!debug.contains("body-secret-token"));
    assert!(!debug.contains("metadata-secret-token"));
    assert!(!debug.contains("support.example"));
}

#[tokio::test]
async fn multibyte_scalar_limit_and_duplicate_header_precedence_are_stable() {
    let announcement: &'static str = Box::leak("🦀".repeat(600).into_boxed_str());
    let snapshot = native_info([
        ("profile-title", "First title"),
        ("profile-title", "Second title"),
        ("announce", announcement),
    ])
    .await;
    let info = snapshot.subscription_info().unwrap();
    assert_eq!(info.display_name, "First title");
    let announcement = info.announcement_text.as_ref().unwrap();
    assert_eq!(announcement.chars().count(), 512);
    assert!(announcement.chars().all(|character| character == '🦀'));
}

#[tokio::test]
async fn successful_massive_response_is_terminal_and_preserves_config_bytes() {
    let http = FakeHttp::new(vec![
        response(200, bundle(), [("profile-title", "Massive")]),
        response(200, MIHOMO, [("profile-title", "Wrong")]),
        response(200, XRAY, []),
    ]);
    let agents = http.agents.clone();
    let store = AtomicSnapshot::default();
    SubscriptionFetcher::new(http)
        .refresh("https://massive.example/sub", &store)
        .await
        .unwrap();
    let snapshot = store.current().unwrap();
    assert_eq!(&*agents.lock().unwrap(), &[UA_NATIVE]);
    assert_eq!(snapshot.mihomo.raw_yaml(), MIHOMO);
    assert_eq!(snapshot.xray.raw_bytes(), XRAY.as_bytes());
    assert_eq!(
        snapshot.subscription_info().unwrap().display_name,
        "Massive"
    );
    assert_ne!(UA_MIHOMO, UA_XRAY);
}
