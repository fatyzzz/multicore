use std::{
    future::Future,
    io::Cursor,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex},
};

use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use multicore_core::{
    FetchError, HttpClient, HttpResponse, MAX_SERVICE_LOGO_OUTPUT_BYTES, PersistentSnapshotStore,
    SubscriptionFetcher, UA_MIHOMO, UA_NATIVE, UA_SERVICE_LOGO, UA_XRAY, normalize_service_logo,
};

const MIHOMO: &str = "proxies: []\n";
const XRAY: &str = "{}";

fn bundle() -> Vec<u8> {
    format!("[{},{}]", serde_json::to_string(MIHOMO).unwrap(), XRAY).into_bytes()
}

fn encoded(format: ImageFormat) -> Vec<u8> {
    let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(32, 20, Rgba([1, 2, 3, 200])));
    let mut bytes = Cursor::new(Vec::new());
    image.write_to(&mut bytes, format).unwrap();
    bytes.into_inner()
}

fn assert_canonical_png(input: &[u8]) {
    let normalized = normalize_service_logo(input).expect("logo should be accepted");
    assert!(normalized.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert!(normalized.len() <= MAX_SERVICE_LOGO_OUTPUT_BYTES);
    let output = image::load_from_memory_with_format(&normalized, ImageFormat::Png).unwrap();
    assert!(output.width() <= 256);
    assert!(output.height() <= 256);
    assert_eq!(output.color(), image::ColorType::Rgba8);
}

#[test]
fn raster_formats_are_detected_by_content_and_normalized() {
    for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::WebP] {
        assert_canonical_png(&encoded(format));
    }
}

#[test]
fn simple_svg_is_rendered_and_nested_resources_are_rejected() {
    assert_canonical_png(br#"<svg xmlns='http://www.w3.org/2000/svg' width='40' height='20'><rect width='40' height='20' fill='#123456'/></svg>"#);

    for href in [
        "https://example.invalid/a.png",
        "file:///private/a.png",
        "data:image/png;base64,aGVsbG8=",
    ] {
        let svg = format!(
            "<svg xmlns='http://www.w3.org/2000/svg' width='20' height='20'><image href='{href}' width='20' height='20'/></svg>"
        );
        assert!(normalize_service_logo(svg.as_bytes()).is_none());
    }
}

#[test]
fn svg_streaming_preflight_accepts_limit_and_rejects_depth_xml_and_entities() {
    for groups in [61, 62] {
        let svg = format!(
            "<svg xmlns='http://www.w3.org/2000/svg' width='1' height='1'>{}<path d='M0 0L1 1'/>{}</svg>",
            "<g>".repeat(groups),
            "</g>".repeat(groups)
        );
        assert!(
            normalize_service_logo(svg.as_bytes()).is_some(),
            "groups={groups}"
        );
    }
    let over_depth = format!(
        "<svg xmlns='http://www.w3.org/2000/svg' width='1' height='1'>{}<path d='M0 0L1 1'/>{}</svg>",
        "<g>".repeat(63),
        "</g>".repeat(63)
    );
    assert!(normalize_service_logo(over_depth.as_bytes()).is_none());
    assert!(normalize_service_logo(b"<svg><g></svg>").is_none());
    assert!(normalize_service_logo(
        br#"<!DOCTYPE svg [<!ENTITY x "boom">]><svg xmlns='http://www.w3.org/2000/svg' width='1' height='1'>&x;</svg>"#
    )
    .is_none());
}

#[test]
fn svg_streaming_preflight_enforces_element_limit_before_usvg() {
    for children in [4094, 4095] {
        let svg = format!(
            "<svg xmlns='http://www.w3.org/2000/svg' width='1' height='1'>{}</svg>",
            "<g/>".repeat(children)
        );
        assert!(
            normalize_service_logo(svg.as_bytes()).is_some(),
            "children={children}"
        );
    }
    let over_limit = format!(
        "<svg xmlns='http://www.w3.org/2000/svg' width='1' height='1'>{}</svg>",
        "<g/>".repeat(4096)
    );
    assert!(normalize_service_logo(over_limit.as_bytes()).is_none());
}

#[test]
fn svg_reference_and_marker_expansion_is_rejected_under_raw_limits() {
    let referenced = "<path d='M0 0L1 1'/>".repeat(1000);
    let uses = "<use href='#large'/>".repeat(1000);
    let svg = format!(
        "<svg xmlns='http://www.w3.org/2000/svg' width='1' height='1'><defs><g id='large'>{referenced}</g><marker id='m'>{referenced}</marker></defs>{uses}<path marker-start='url(#m)' marker-mid='url(#m)' marker-end='url(#m)' d='M0 0L1 1'/></svg>"
    );
    assert!(svg.len() < multicore_core::MAX_SERVICE_LOGO_BYTES);
    assert!(normalize_service_logo(svg.as_bytes()).is_none());

    for forbidden in [
        "<use href='#x'/>",
        "<marker id='x'/>",
        "<path marker='url(#x)' d='M0 0'/>",
        "<path marker-start='url(#x)' d='M0 0'/>",
        "<path style='marker-mid:url(#x)' d='M0 0'/>",
    ] {
        let svg = format!(
            "<svg xmlns='http://www.w3.org/2000/svg' width='1' height='1'>{forbidden}</svg>"
        );
        assert!(normalize_service_logo(svg.as_bytes()).is_none());
    }
}

#[test]
fn svg_filter_expansion_is_rejected_but_simple_gradients_are_accepted() {
    let fan_out = format!(
        "<svg xmlns='http://www.w3.org/2000/svg' width='32' height='32'><defs><filter id='f'>{}</filter></defs>{}</svg>",
        "<feImage href='data:image/png;base64,aGVsbG8='/ >".repeat(1000),
        "<rect width='32' height='32' filter='url(#f)'/>".repeat(1000),
    );
    assert!(fan_out.len() < multicore_core::MAX_SERVICE_LOGO_BYTES);
    assert!(normalize_service_logo(fan_out.as_bytes()).is_none());

    let filter_chain = format!(
        "<svg xmlns='http://www.w3.org/2000/svg' width='32' height='32'><filter id='f'>{}</filter><rect width='32' height='32' filter='url(#f)'/></svg>",
        "<feGaussianBlur stdDeviation='1'/>".repeat(3000),
    );
    assert!(filter_chain.len() < multicore_core::MAX_SERVICE_LOGO_BYTES);
    assert!(normalize_service_logo(filter_chain.as_bytes()).is_none());

    assert_canonical_png(
        br#"<svg xmlns='http://www.w3.org/2000/svg' width='32' height='32'><defs><linearGradient id='g' x1='0' y1='0' x2='1' y2='1'><stop offset='0' stop-color='#123456'/><stop offset='1' stop-color='#abcdef'/></linearGradient></defs><rect width='32' height='32' fill='url(#g)'/></svg>"#,
    );
}

#[test]
fn corrupt_spoofed_animated_and_pathological_inputs_are_rejected() {
    assert!(normalize_service_logo(b"logo.png but not really").is_none());
    assert!(normalize_service_logo(b"\x89PNG\r\n\x1a\ntruncated").is_none());

    let mut animated_webp = b"RIFF\x20\0\0\0WEBPVP8X\0\0\0\0\x02\0\0\0ANIM".to_vec();
    animated_webp.resize(40, 0);
    assert!(normalize_service_logo(&animated_webp).is_none());

    let mut apng_marker = encoded(ImageFormat::Png);
    apng_marker.extend_from_slice(b"acTL");
    assert!(normalize_service_logo(&apng_marker).is_none());

    let pathological = format!(
        "<svg xmlns='http://www.w3.org/2000/svg' width='1' height='1'>{}</svg>",
        "<path d='M0 0'/>".repeat(5000)
    );
    assert!(normalize_service_logo(pathological.as_bytes()).is_none());

    let segments = "L1 1".repeat(16_385);
    let path_heavy = format!(
        "<svg xmlns='http://www.w3.org/2000/svg' width='1' height='1'><path d='M0 0{segments}'/></svg>"
    );
    assert!(normalize_service_logo(path_heavy.as_bytes()).is_none());

    let deeply_nested = format!(
        "<svg xmlns='http://www.w3.org/2000/svg' width='1' height='1'>{}<path d='M0 0L1 1'/>{}</svg>",
        "<g>".repeat(66),
        "</g>".repeat(66)
    );
    assert!(normalize_service_logo(deeply_nested.as_bytes()).is_none());
}

#[test]
fn source_dimensions_and_source_size_are_bounded() {
    assert!(normalize_service_logo(&encoded(ImageFormat::Png)).is_some());
    let oversized = RgbaImage::new(2049, 1);
    let mut bytes = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(oversized)
        .write_to(&mut bytes, ImageFormat::Png)
        .unwrap();
    assert!(normalize_service_logo(&bytes.into_inner()).is_none());
    let boundary = RgbaImage::new(2048, 2048);
    let mut bytes = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(boundary)
        .write_to(&mut bytes, ImageFormat::Png)
        .unwrap();
    assert!(normalize_service_logo(&bytes.into_inner()).is_some());
    assert!(normalize_service_logo(&vec![b' '; 512 * 1024 + 1]).is_none());
}

#[derive(Clone)]
struct LogoHttp {
    calls: Arc<Mutex<Vec<(String, &'static str)>>>,
    logo: Result<HttpResponse, FetchError>,
}

impl HttpClient for LogoHttp {
    fn get<'a>(
        &'a self,
        url: &'a str,
        user_agent: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, FetchError>> + Send + 'a>> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push((url.to_owned(), user_agent));
            if user_agent == UA_NATIVE {
                Ok(HttpResponse::new(
                    200,
                    bundle(),
                    [("flclashx-servicelogo", "https://logos.example/logo.any")],
                ))
            } else {
                self.logo.clone()
            }
        })
    }
}

fn temp_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "multicore-service-logo-{name}-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    std::fs::create_dir(&root).unwrap();
    root
}

fn rand_suffix() -> u64 {
    let mut bytes = [0; 8];
    getrandom::fill(&mut bytes).unwrap();
    u64::from_ne_bytes(bytes)
}

#[tokio::test]
async fn valid_native_bundle_is_terminal_and_fetches_logo_once_after_parsing() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let http = LogoHttp {
        calls: calls.clone(),
        logo: Ok(HttpResponse::new(
            200,
            encoded(ImageFormat::WebP),
            [] as [(&str, &str); 0],
        )),
    };
    let root = temp_root("roundtrip");
    let store = PersistentSnapshotStore::open(&root).unwrap();
    let snapshot = SubscriptionFetcher::new(http)
        .refresh("https://subscription.example/private", &store)
        .await
        .unwrap();

    assert_eq!(
        calls.lock().unwrap().as_slice(),
        &[
            ("https://subscription.example/private".to_owned(), UA_NATIVE),
            ("https://logos.example/logo.any".to_owned(), UA_SERVICE_LOGO),
        ]
    );
    let path = snapshot.service_logo_path().unwrap();
    assert_eq!(path.file_name().unwrap(), "service-logo.png");
    assert_canonical_png(&std::fs::read(path).unwrap());
    let persisted =
        std::fs::read_to_string(root.join("snapshot-00000000000000000001/subscription.json"))
            .unwrap();
    assert!(!persisted.contains("logos.example"));

    drop(store);
    let reopened = PersistentSnapshotStore::open(&root).unwrap();
    assert_eq!(reopened.current().unwrap().service_logo_path(), Some(path));
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn logo_failure_is_nonfatal_and_persists_no_logo() {
    for logo in [
        Err(FetchError::Network),
        Ok(HttpResponse::new(404, Vec::new(), [] as [(&str, &str); 0])),
        Ok(HttpResponse::new(
            200,
            b"spoofed".to_vec(),
            [] as [(&str, &str); 0],
        )),
    ] {
        let root = temp_root("failure");
        let store = PersistentSnapshotStore::open(&root).unwrap();
        let snapshot = SubscriptionFetcher::new(LogoHttp {
            calls: Arc::default(),
            logo,
        })
        .refresh("https://subscription.example/private", &store)
        .await
        .unwrap();
        assert!(snapshot.service_logo_path().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn failed_logo_generation_staging_keeps_the_last_good_logo_path() {
    let root = temp_root("staging");
    let store = PersistentSnapshotStore::open(&root).unwrap();
    let http = LogoHttp {
        calls: Arc::default(),
        logo: Ok(HttpResponse::new(
            200,
            encoded(ImageFormat::Png),
            [] as [(&str, &str); 0],
        )),
    };
    SubscriptionFetcher::new(http.clone())
        .refresh("https://subscription.example/private", &store)
        .await
        .unwrap();
    let last_good = store
        .current()
        .unwrap()
        .service_logo_path()
        .unwrap()
        .to_owned();
    std::fs::create_dir(root.join(format!(
        ".snapshot-00000000000000000002.staging-{}",
        std::process::id()
    )))
    .unwrap();
    assert_eq!(
        SubscriptionFetcher::new(http)
            .refresh("https://subscription.example/private", &store)
            .await,
        Err(FetchError::Persistence)
    );
    assert_eq!(
        store.current().unwrap().service_logo_path(),
        Some(last_good.as_path())
    );
    drop(store);
    let reopened = PersistentSnapshotStore::open(&root).unwrap();
    assert_eq!(
        reopened.current().unwrap().service_logo_path(),
        Some(last_good.as_path())
    );
    drop(reopened);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn invalid_persisted_optional_logo_is_ignored_without_losing_config() {
    let root = temp_root("invalid-load");
    let store = PersistentSnapshotStore::open(&root).unwrap();
    store
        .commit(multicore_core::Snapshot::parse(MIHOMO.as_bytes(), XRAY.as_bytes()).unwrap())
        .unwrap();
    std::fs::write(
        root.join("snapshot-00000000000000000001/service-logo.png"),
        b"corrupt",
    )
    .unwrap();
    drop(store);
    let reopened = PersistentSnapshotStore::open(&root).unwrap();
    assert!(reopened.current().is_some());
    assert!(reopened.current().unwrap().service_logo_path().is_none());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn oversized_persisted_optional_logo_is_not_read_or_exposed() {
    let root = temp_root("oversized-load");
    let store = PersistentSnapshotStore::open(&root).unwrap();
    store
        .commit(multicore_core::Snapshot::parse(MIHOMO.as_bytes(), XRAY.as_bytes()).unwrap())
        .unwrap();
    let path = root.join("snapshot-00000000000000000001/service-logo.png");
    let file = std::fs::File::create(path).unwrap();
    file.set_len(4 * 1024 * 1024 * 1024).unwrap();
    drop(file);
    drop(store);
    let reopened = PersistentSnapshotStore::open(&root).unwrap();
    assert!(reopened.current().is_some());
    assert!(reopened.current().unwrap().service_logo_path().is_none());
    drop(reopened);
    std::fs::remove_dir_all(root).unwrap();
}

#[derive(Clone, Default)]
struct InvalidConfigHttp {
    calls: Arc<Mutex<Vec<&'static str>>>,
}

impl HttpClient for InvalidConfigHttp {
    fn get<'a>(
        &'a self,
        _url: &'a str,
        user_agent: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, FetchError>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(user_agent);
            Ok(HttpResponse::new(
                200,
                b"invalid bundle".to_vec(),
                [("flclashx-servicelogo", "https://logos.example/logo")],
            ))
        })
    }
}

#[tokio::test]
async fn invalid_config_never_triggers_a_logo_request() {
    let http = InvalidConfigHttp::default();
    let calls = http.calls.clone();
    let result = SubscriptionFetcher::new(http)
        .refresh(
            "https://subscription.example/private",
            &multicore_core::AtomicSnapshot::default(),
        )
        .await;
    assert_eq!(result, Err(FetchError::InvalidBundle));
    assert_eq!(*calls.lock().unwrap(), [UA_NATIVE]);
}

#[derive(Clone, Default)]
struct FallbackHttp {
    calls: Arc<Mutex<Vec<(String, &'static str)>>>,
}

impl HttpClient for FallbackHttp {
    fn get<'a>(
        &'a self,
        url: &'a str,
        user_agent: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, FetchError>> + Send + 'a>> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push((url.to_owned(), user_agent));
            Ok(match user_agent {
                UA_NATIVE => HttpResponse::new(503, Vec::new(), [] as [(&str, &str); 0]),
                UA_MIHOMO => HttpResponse::new(
                    200,
                    MIHOMO.as_bytes().to_vec(),
                    [("flclashx-servicelogo", "https://mihomo.example/logo")],
                ),
                UA_XRAY => HttpResponse::new(
                    200,
                    XRAY.as_bytes().to_vec(),
                    [("flclashx-servicelogo", "https://xray.example/logo")],
                ),
                UA_SERVICE_LOGO => {
                    HttpResponse::new(200, encoded(ImageFormat::Png), [] as [(&str, &str); 0])
                }
                _ => unreachable!(),
            })
        })
    }
}

#[tokio::test]
async fn fallback_uses_mihomo_logo_precedence_and_fetches_only_that_url_once() {
    let http = FallbackHttp::default();
    let calls = http.calls.clone();
    let root = temp_root("fallback");
    let store = PersistentSnapshotStore::open(&root).unwrap();
    SubscriptionFetcher::new(http)
        .refresh("https://subscription.example/private", &store)
        .await
        .unwrap();
    let calls = calls.lock().unwrap();
    let logo_calls = calls
        .iter()
        .filter(|(_, agent)| *agent == UA_SERVICE_LOGO)
        .collect::<Vec<_>>();
    assert_eq!(logo_calls.len(), 1);
    assert_eq!(logo_calls[0].0, "https://mihomo.example/logo");
    drop(calls);
    std::fs::remove_dir_all(root).unwrap();
}
