use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use multicore_daemon::{
    ApiErrorBody, ConnectionState, DiagnosticsDto, MemoryBackend, RuntimeCheckDto, StatusDto,
    router, router_with_event_epoch,
};
use serde_json::{Value, json};
use tower::ServiceExt;

const TOKEN: &str = "test-secret-token";

fn app() -> axum::Router {
    router_with_event_epoch(MemoryBackend::default(), TOKEN, "test-epoch")
}

fn request(method: &str, uri: &str, body: Body) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .unwrap()
}

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn all_v1_routes_require_bearer_authentication() {
    let routes = [
        ("GET", "/v1/status", Body::empty()),
        (
            "POST",
            "/v1/subscriptions/import",
            Body::from(r#"{"url":"https://example.invalid/sub"}"#),
        ),
        ("POST", "/v1/subscriptions/refresh", Body::empty()),
        ("POST", "/v1/connect", Body::empty()),
        ("POST", "/v1/disconnect", Body::empty()),
        ("GET", "/v1/events?after=0", Body::empty()),
        ("GET", "/v1/diagnostics", Body::empty()),
    ];

    for (method, uri, body) in routes {
        let response = app()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
        let error: ApiErrorBody = serde_json::from_value(json_body(response).await).unwrap();
        assert!(!error.message.is_empty());
        assert!(!error.correlation_id.is_empty());
    }
}

#[tokio::test]
async fn diagnostics_route_is_authenticated_and_has_stable_safe_shape() {
    let response = app()
        .oneshot(request("GET", "/v1/diagnostics", Body::empty()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let diagnostics: DiagnosticsDto = serde_json::from_value(json_body(response).await).unwrap();
    assert_eq!(diagnostics.xray, RuntimeCheckDto::Stopped);
    assert_eq!(diagnostics.mihomo, RuntimeCheckDto::Stopped);
    assert_eq!(diagnostics.tun, RuntimeCheckDto::Stopped);
    assert!(diagnostics.logs.is_empty());
    assert!(diagnostics.mappings.is_empty());
}

#[tokio::test]
async fn rejected_token_is_never_echoed() {
    let rejected_token = "token-that-must-stay-secret";
    let response = app()
        .oneshot(
            Request::builder()
                .uri("/v1/status")
                .header(header::AUTHORIZATION, format!("Bearer {rejected_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = json_body(response).await.to_string();
    assert!(!body.contains(rejected_token));
}

#[tokio::test]
async fn status_connect_disconnect_and_events_form_a_testable_contract() {
    let app = app();
    let status = app
        .clone()
        .oneshot(request("GET", "/v1/status", Body::empty()))
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::OK);
    let status: StatusDto = serde_json::from_value(json_body(status).await).unwrap();
    assert_eq!(status.state, ConnectionState::Empty);
    assert_eq!(status.profile, None);
    assert_eq!(status.current_node, None);
    assert_eq!(status.message, None);
    assert!(!status.degraded);

    let imported = app
        .clone()
        .oneshot(request(
            "POST",
            "/v1/subscriptions/import",
            Body::from(r#"{"url":"https://example.invalid/sub"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(imported.status(), StatusCode::OK);
    let imported: StatusDto = serde_json::from_value(json_body(imported).await).unwrap();
    assert_eq!(imported.state, ConnectionState::Ready);
    assert!(imported.profile.is_some());

    let connected = app
        .clone()
        .oneshot(request("POST", "/v1/connect", Body::empty()))
        .await
        .unwrap();
    assert_eq!(connected.status(), StatusCode::OK);
    let connected: StatusDto = serde_json::from_value(json_body(connected).await).unwrap();
    assert_eq!(connected.state, ConnectionState::Connected);

    let events = app
        .clone()
        .oneshot(request("GET", "/v1/events?after=0", Body::empty()))
        .await
        .unwrap();
    assert_eq!(events.status(), StatusCode::OK);
    let events = json_body(events).await;
    assert_eq!(events["epoch"], "test-epoch");
    let events = events["events"].as_array().expect("events is an array");
    assert_eq!(events.len(), 2);
    let fields = events[0].as_object().unwrap();
    assert_eq!(fields.len(), 4);
    assert_eq!(fields["id"], 1);
    assert_eq!(events[1]["id"], 2);
    assert!(fields.contains_key("timestamp"));
    assert!(fields.contains_key("level"));
    assert!(fields.contains_key("message"));

    let events_after_first = app
        .clone()
        .oneshot(request("GET", "/v1/events?after=1", Body::empty()))
        .await
        .unwrap();
    let events_after_first = json_body(events_after_first).await;
    assert_eq!(events_after_first["epoch"], "test-epoch");
    assert_eq!(events_after_first["events"].as_array().unwrap().len(), 1);
    assert_eq!(events_after_first["events"][0]["id"], 2);

    let disconnected = app
        .oneshot(request("POST", "/v1/disconnect", Body::empty()))
        .await
        .unwrap();
    assert_eq!(disconnected.status(), StatusCode::OK);
}

#[tokio::test]
async fn event_epoch_is_unique_per_router_and_can_be_injected_for_tests() {
    let first = router(MemoryBackend::default(), TOKEN)
        .oneshot(request("GET", "/v1/events?after=0", Body::empty()))
        .await
        .unwrap();
    let second = router(MemoryBackend::default(), TOKEN)
        .oneshot(request("GET", "/v1/events?after=0", Body::empty()))
        .await
        .unwrap();
    let first = json_body(first).await;
    let second = json_body(second).await;
    assert_ne!(first["epoch"], second["epoch"]);

    let injected = router_with_event_epoch(MemoryBackend::default(), TOKEN, "fixed-test-epoch")
        .oneshot(request("GET", "/v1/events?after=0", Body::empty()))
        .await
        .unwrap();
    assert_eq!(json_body(injected).await["epoch"], "fixed-test-epoch");
}

#[tokio::test]
async fn events_are_bounded_paginated_and_keep_monotonic_ids_after_eviction() {
    let app = app();
    for _ in 0..513 {
        let response = app
            .clone()
            .oneshot(request("POST", "/v1/connect", Body::empty()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    let first_page = app
        .clone()
        .oneshot(request("GET", "/v1/events?after=0", Body::empty()))
        .await
        .unwrap();
    let first_page = json_body(first_page).await;
    let first_page = first_page["events"].as_array().unwrap();
    assert_eq!(first_page.len(), 256);
    assert_eq!(first_page.first().unwrap()["id"], 2);
    assert_eq!(first_page.last().unwrap()["id"], 257);

    let second_page = app
        .clone()
        .oneshot(request("GET", "/v1/events?after=257", Body::empty()))
        .await
        .unwrap();
    let second_page = json_body(second_page).await;
    let second_page = second_page["events"].as_array().unwrap();
    assert_eq!(second_page.len(), 256);
    assert_eq!(second_page.first().unwrap()["id"], 258);
    assert_eq!(second_page.last().unwrap()["id"], 513);

    let exhausted = app
        .oneshot(request("GET", "/v1/events?after=513", Body::empty()))
        .await
        .unwrap();
    assert!(
        json_body(exhausted).await["events"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn mismatched_event_epoch_restarts_from_oldest_retained_event() {
    let app = app();
    for _ in 0..200 {
        let response = app
            .clone()
            .oneshot(request("POST", "/v1/connect", Body::empty()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    let restarted = app
        .oneshot(request(
            "GET",
            "/v1/events?after=100&epoch=old%2Ddaemon%2Depoch",
            Body::empty(),
        ))
        .await
        .unwrap();
    let restarted = json_body(restarted).await;
    assert_eq!(restarted["epoch"], "test-epoch");
    let events = restarted["events"].as_array().unwrap();
    assert_eq!(events.len(), 200);
    assert_eq!(events.first().unwrap()["id"], 1);
    assert_eq!(events.last().unwrap()["id"], 200);
}

#[tokio::test]
async fn matching_event_epoch_honors_the_cursor() {
    let app = app();
    for _ in 0..3 {
        app.clone()
            .oneshot(request("POST", "/v1/connect", Body::empty()))
            .await
            .unwrap();
    }

    let response = app
        .oneshot(request(
            "GET",
            "/v1/events?after=2&epoch=test%2Depoch",
            Body::empty(),
        ))
        .await
        .unwrap();
    let response = json_body(response).await;
    let events = response["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["id"], 3);
}

#[tokio::test]
async fn import_accepts_url_but_does_not_return_it() {
    let secret_url = "https://user:password@example.invalid/private?token=secret";
    let response = app()
        .oneshot(request(
            "POST",
            "/v1/subscriptions/import",
            Body::from(json!({"url": secret_url}).to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["state"], "ready");
    let text = body.to_string();
    assert!(!text.contains(secret_url));
    assert!(!text.contains("password"));
    assert!(!text.contains("secret"));
}

#[tokio::test]
async fn malformed_input_has_russian_actionable_error_and_correlation_id() {
    let response = app()
        .oneshot(request(
            "POST",
            "/v1/subscriptions/import",
            Body::from(r#"{"url":42}"#),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: ApiErrorBody = serde_json::from_value(json_body(response).await).unwrap();
    assert!(error.message.contains("Проверьте"));
    assert!(!error.correlation_id.is_empty());
}

#[tokio::test]
async fn body_larger_than_one_mebibyte_is_rejected() {
    let oversized = format!(r#"{{"url":"{}"}}"#, "x".repeat(1024 * 1024));
    let response = app()
        .oneshot(request(
            "POST",
            "/v1/subscriptions/import",
            Body::from(oversized),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let error: ApiErrorBody = serde_json::from_value(json_body(response).await).unwrap();
    assert!(error.message.contains("Уменьшите"));
    assert!(!error.correlation_id.is_empty());
}

#[tokio::test]
async fn body_limit_applies_to_routes_without_body_extractors() {
    let oversized = "x".repeat(1024 * 1024 + 1);
    let response = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/connect")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(header::CONTENT_LENGTH, oversized.len())
                .body(Body::from(oversized))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let error: ApiErrorBody = serde_json::from_value(json_body(response).await).unwrap();
    assert!(error.message.contains("Уменьшите"));
    assert!(!error.correlation_id.is_empty());
}

#[tokio::test]
async fn events_requires_after_query_parameter() {
    let response = app()
        .oneshot(request("GET", "/v1/events", Body::empty()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: ApiErrorBody = serde_json::from_value(json_body(response).await).unwrap();
    assert!(error.message.contains("after"));
    assert!(!error.correlation_id.is_empty());
}

#[tokio::test]
async fn bodyless_routes_reject_body_without_content_length() {
    let routes = [
        ("GET", "/v1/status"),
        ("POST", "/v1/connect"),
        ("POST", "/v1/disconnect"),
        ("GET", "/v1/events?after=0"),
    ];

    for (method, uri) in routes {
        let response = app()
            .oneshot(request(method, uri, Body::from("unexpected")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
        let error: ApiErrorBody = serde_json::from_value(json_body(response).await).unwrap();
        assert!(error.message.contains("тело"), "{uri}: {}", error.message);
        assert!(!error.correlation_id.is_empty());
    }
}

#[tokio::test]
async fn method_errors_are_localized_and_no_cors_headers_are_added() {
    let response = app()
        .oneshot(request("PUT", "/v1/status", Body::empty()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert!(
        response
            .headers()
            .get("access-control-allow-origin")
            .is_none()
    );
    let error: ApiErrorBody = serde_json::from_value(json_body(response).await).unwrap();
    assert!(error.message.contains("Проверьте"));
    assert!(!error.correlation_id.is_empty());
}

#[tokio::test]
async fn bind_helper_rejects_non_loopback_addresses() {
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
    let error = multicore_daemon::bind_loopback(address).await.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[tokio::test]
async fn bind_helper_accepts_loopback_addresses() {
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let listener = multicore_daemon::bind_loopback(address).await.unwrap();
    assert!(listener.local_addr().unwrap().ip().is_loopback());
}
