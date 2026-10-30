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

#[derive(Clone)]
struct VendorLogoHttp {
    calls: Arc<Mutex<Vec<(String, &'static str)>>>,
}

impl HttpClient for VendorLogoHttp {
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
                    [
                        ("ajsdjddj-servicelogo", "https://logos.example/vendor.png"),
                        (
                            "ajsdjddj-background",
                            "https://logos.example/must-ignore.png",
                        ),
                    ],
                ))
            } else {
                Ok(HttpResponse::new(
                    200,
                    encoded(ImageFormat::Png),
                    [] as [(&str, &str); 0],
                ))
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
#[cfg_attr(
    not(any(target_os = "linux", target_os = "android", windows)),
    ignore = "the logo path comes from /proc/<pid>/fd, which this platform lacks"
)]
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
    let path = snapshot.service_logo_path().unwrap().to_owned();
    let expected_logo = std::fs::read(&path).unwrap();
    assert_canonical_png(&expected_logo);
    let persisted =
        std::fs::read_to_string(root.join("snapshot-00000000000000000001/subscription.json"))
            .unwrap();
    assert!(!persisted.contains("logos.example"));

    drop(snapshot);
    drop(store);
    let reopened = PersistentSnapshotStore::open(&root).unwrap();
    let reopened_snapshot = reopened.current().unwrap();
    assert_eq!(
        std::fs::read(reopened_snapshot.service_logo_path().unwrap()).unwrap(),
        expected_logo
    );
    drop(reopened_snapshot);
    drop(reopened);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
#[cfg_attr(
    not(any(target_os = "linux", target_os = "android", windows)),
    ignore = "the logo path comes from /proc/<pid>/fd, which this platform lacks"
)]
async fn a_deferred_logo_is_not_downloaded_during_the_refresh_and_the_previous_one_is_kept() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let logo = Ok(HttpResponse::new(
        200,
        encoded(ImageFormat::Png),
        [] as [(&str, &str); 0],
    ));
    let root = temp_root("deferred");
    let store = PersistentSnapshotStore::open(&root).unwrap();
    let fetcher = SubscriptionFetcher::new(LogoHttp {
        calls: calls.clone(),
        logo,
    });
    let previous = fetcher
        .refresh("https://subscription.example/private", &store)
        .await
        .unwrap()
        .into_committed();
    calls.lock().unwrap().clear();

    let staging = multicore_core::AtomicSnapshot::default();
    let (deferred, _) = fetcher
        .refresh_deferring_logo("https://subscription.example/private", &staging)
        .await;
    let deferred = deferred.unwrap().into_committed();
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        &[("https://subscription.example/private".to_owned(), UA_NATIVE)]
    );
    // The URL waits in memory for the background download...
    assert_eq!(
        deferred.subscription_logo_url(),
        Some("https://logos.example/logo.any")
    );
    // ...and the previous logo is kept meanwhile.
    let carried = deferred.clone().with_service_logo_carried_from(&previous);
    let persisted = multicore_core::SnapshotSink::save(&store, (*carried).clone())
        .unwrap()
        .into_committed();
    assert_eq!(
        std::fs::read(persisted.service_logo_path().unwrap()).unwrap(),
        std::fs::read(previous.service_logo_path().unwrap()).unwrap()
    );
    assert!(matches!(
        fetcher
            .download_service_logo("https://logos.example/logo.any")
            .await,
        multicore_core::ServiceLogoDownload::Logo(_)
    ));
    drop((previous, carried, deferred, persisted, store));
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
#[cfg_attr(
    not(any(target_os = "linux", target_os = "android", windows)),
    ignore = "the logo path comes from /proc/<pid>/fd, which this platform lacks"
)]
async fn arbitrary_vendor_prefix_is_accepted_only_for_safe_service_logo_suffix() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let root = temp_root("vendor-prefix");
    let store = PersistentSnapshotStore::open(&root).unwrap();
    let snapshot = SubscriptionFetcher::new(VendorLogoHttp {
        calls: calls.clone(),
    })
    .refresh("https://subscription.example/private", &store)
    .await
    .unwrap();

    assert!(snapshot.service_logo_path().is_some());
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        &[
            ("https://subscription.example/private".to_owned(), UA_NATIVE),
            (
                "https://logos.example/vendor.png".to_owned(),
                UA_SERVICE_LOGO
            ),
        ]
    );

    drop(snapshot);
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[tokio::test]
async fn exposed_logo_path_remains_bound_to_original_after_root_replacement() {
    let root = temp_root("root-replacement");
    let original = root.with_extension("original");
    let store = PersistentSnapshotStore::open(&root).unwrap();
    let snapshot = SubscriptionFetcher::new(LogoHttp {
        calls: Arc::default(),
        logo: Ok(HttpResponse::new(
            200,
            encoded(ImageFormat::Png),
            [] as [(&str, &str); 0],
        )),
    })
    .refresh("https://subscription.example/private", &store)
    .await
    .unwrap();
    let exposed = snapshot.service_logo_path().unwrap().to_owned();
    let trusted = std::fs::read(&exposed).unwrap();

    std::fs::rename(&root, &original).unwrap();
    std::fs::create_dir(&root).unwrap();
    let replacement_generation = root.join("snapshot-00000000000000000001");
    std::fs::create_dir(&replacement_generation).unwrap();
    std::fs::write(
        replacement_generation.join("service-logo.png"),
        b"attacker replacement bytes",
    )
    .unwrap();

    assert_eq!(std::fs::read(&exposed).unwrap(), trusted);
    assert_ne!(
        std::fs::read(replacement_generation.join("service-logo.png")).unwrap(),
        trusted
    );

    drop(snapshot);
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(original).unwrap();
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
        drop(snapshot);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
#[cfg_attr(
    not(any(target_os = "linux", target_os = "android", windows)),
    ignore = "the logo path comes from /proc/<pid>/fd, which this platform lacks"
)]
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
    assert!(matches!(
        SubscriptionFetcher::new(http)
            .refresh("https://subscription.example/private", &store)
            .await,
        Err(FetchError::Persistence)
    ));
    assert_eq!(
        store.current().unwrap().service_logo_path(),
        Some(last_good.as_path())
    );
    drop(store);
    let reopened = PersistentSnapshotStore::open(&root).unwrap();
    assert_canonical_png(
        &std::fs::read(reopened.current().unwrap().service_logo_path().unwrap()).unwrap(),
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
    drop(reopened);
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
    assert!(matches!(result, Err(FetchError::NotSubscription)));
    assert_eq!(*calls.lock().unwrap(), [UA_NATIVE, UA_MIHOMO, UA_XRAY]);
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
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn small_svg_viewbox_is_rasterized_at_the_full_output_bound() {
    let normalized = normalize_service_logo(
        br#"<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 12' width='24' height='12'><rect width='24' height='12' fill='#123456'/></svg>"#,
    )
    .unwrap();
    let output = image::load_from_memory_with_format(&normalized, ImageFormat::Png).unwrap();
    assert_eq!((output.width(), output.height()), (256, 128));

    let icon = multicore_core::normalize_group_icon(
        br#"<svg xmlns='http://www.w3.org/2000/svg' width='16' height='16'><circle cx='8' cy='8' r='8' fill='#fff'/></svg>"#,
    )
    .unwrap();
    let output = image::load_from_memory_with_format(&icon, ImageFormat::Png).unwrap();
    assert_eq!((output.width(), output.height()), (128, 128));
}

#[test]
fn group_icons_are_bounded_to_their_own_smaller_output() {
    let large = RgbaImage::from_pixel(600, 300, Rgba([9, 8, 7, 255]));
    let mut bytes = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(large)
        .write_to(&mut bytes, ImageFormat::Png)
        .unwrap();
    let icon = multicore_core::normalize_group_icon(&bytes.into_inner()).unwrap();
    let output = image::load_from_memory_with_format(&icon, ImageFormat::Png).unwrap();
    assert_eq!((output.width(), output.height()), (128, 64));
    assert!(icon.len() <= multicore_core::MAX_GROUP_ICON_OUTPUT_BYTES);
}

const ICON_MIHOMO: &str = "proxies:\n  - {name: A, type: socks5, server: 127.0.0.1, port: 30001}\nproxy-groups:\n  - name: Main\n    type: select\n    icon: https://icons.example/main.png\n    proxies: [Auto, A]\n  - name: Auto\n    type: url-test\n    icon: https://icons.example/auto.svg\n    proxies: [A]\n  - name: Plain\n    type: select\n    icon: http://icons.example/insecure.png\n    proxies: [A]\n  - name: Dup\n    type: select\n    icon: https://icons.example/main.png\n    proxies: [A]\n  - name: Broken\n    type: select\n    icon: https://icons.example/broken.png\n    proxies: [A]\n";

#[derive(Clone)]
struct IconHttp {
    calls: Arc<Mutex<Vec<(String, &'static str)>>>,
    broken_fails: bool,
    /// Icon requests never answer (a dead icon host).
    icons_hang: bool,
}

impl HttpClient for IconHttp {
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
                let body = format!("[{},{}]", serde_json::to_string(ICON_MIHOMO).unwrap(), XRAY);
                return Ok(HttpResponse::new(
                    200,
                    body.into_bytes(),
                    [] as [(&str, &str); 0],
                ));
            }
            if self.icons_hang {
                std::future::pending::<()>().await;
            }
            if url.ends_with("auto.svg") {
                return Ok(HttpResponse::new(
                    200,
                    br#"<svg xmlns='http://www.w3.org/2000/svg' width='16' height='16'><rect width='16' height='16' fill='#0f0'/></svg>"#.to_vec(),
                    [] as [(&str, &str); 0],
                ));
            }
            if url.ends_with("broken.png") && self.broken_fails {
                return Err(FetchError::Timeout);
            }
            Ok(HttpResponse::new(
                200,
                encoded(ImageFormat::Png),
                [] as [(&str, &str); 0],
            ))
        })
    }
}

#[tokio::test]
async fn a_refresh_never_waits_for_group_icons() {
    // A user waited "forever" for a new subscription: every refresh downloaded up to 32 group
    // icons (20 s budget) before the subscription was saved.
    let calls = Arc::new(Mutex::new(Vec::new()));
    let staging = multicore_core::AtomicSnapshot::default();
    let snapshot = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        SubscriptionFetcher::new(IconHttp {
            calls: calls.clone(),
            broken_fails: false,
            icons_hang: true,
        })
        .refresh("https://subscription.example/private", &staging),
    )
    .await
    .expect("the refresh does not wait for icon hosts")
    .unwrap()
    .into_committed();
    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .all(|(_, agent)| *agent != multicore_core::UA_GROUP_ICON)
    );
    // The icons are named, just not downloaded yet.
    assert_eq!(snapshot.group_icon_urls().len(), 3);
    assert!(snapshot.group_icon_id("Main").is_none());
}

#[tokio::test]
async fn group_icons_are_downloaded_once_per_https_url_and_attached_by_content_id() {
    use multicore_core::{UA_GROUP_ICON, group_icon_key};
    let calls = Arc::new(Mutex::new(Vec::new()));
    let staging = multicore_core::AtomicSnapshot::default();
    let fetcher = SubscriptionFetcher::new(IconHttp {
        calls: calls.clone(),
        broken_fails: true,
        icons_hang: false,
    });
    let snapshot = fetcher
        .refresh("https://subscription.example/private", &staging)
        .await
        .unwrap()
        .into_committed();

    let urls = snapshot.group_icon_urls();
    let downloaded = fetcher.download_group_icons(&urls).await;
    let mut icon_calls: Vec<_> = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, agent)| *agent == UA_GROUP_ICON)
        .map(|(url, _)| url.clone())
        .collect();
    icon_calls.sort();
    assert_eq!(
        icon_calls,
        [
            "https://icons.example/auto.svg",
            "https://icons.example/broken.png",
            "https://icons.example/main.png",
        ]
    );
    // The failed download is simply missing.
    assert_eq!(downloaded.len(), 2);
    assert!(
        downloaded
            .keys()
            .all(|key| *key != group_icon_key("https://icons.example/broken.png"))
    );

    let with_icons = snapshot.clone().with_group_icons(&downloaded);
    let main = with_icons.group_icon_id("Main").expect("main icon");
    assert_eq!(main.len(), 32);
    assert_ne!(main, group_icon_key("https://icons.example/main.png"));
    assert_eq!(
        with_icons.group_icon_id("Dup").as_deref(),
        Some(main.as_str())
    );
    assert!(with_icons.group_icon_id("Auto").is_some());
    assert!(with_icons.group_icon_id("Plain").is_none());
    assert!(with_icons.group_icon_id("Broken").is_none());
    assert!(with_icons.group_icon_id("Missing").is_none());
    let svg_icon = with_icons
        .group_icon_png(&with_icons.group_icon_id("Auto").unwrap())
        .unwrap();
    let decoded = image::load_from_memory_with_format(svg_icon, ImageFormat::Png).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (128, 128));
    // Only the icons the config names are attached, and the original is untouched.
    let mut foreign = downloaded.clone();
    foreign.insert(
        group_icon_key("https://elsewhere.example/x.png"),
        Vec::new(),
    );
    assert_eq!(
        snapshot.clone().with_group_icons(&foreign).group_icon_ids(),
        with_icons.group_icon_ids()
    );
    assert!(snapshot.group_icon_id("Main").is_none());
    // What the snapshot shows can seed an icon cache (key -> canonical PNG).
    assert_eq!(with_icons.group_icon_pngs().len(), 2);
}
