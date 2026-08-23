//! Unit tests for the proxy module's routing/models endpoints, ETag/304 handling,
//! `truncate_for_log`, first-content-event markers, and stream-metrics error
//! classification, extracted from `proxy.rs` (module-split spec Phase 2).
//! Session-capture tests live in `proxy/capture/tests.rs`.
use super::*;
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::util::ServiceExt;

#[test]
/// `truncate_for_log` keeps short strings verbatim and truncates long
/// ones at a char boundary (multibyte-safe) with a cut-size marker.
fn truncate_for_log_short_and_multibyte_boundary() {
    assert_eq!(truncate_for_log("short", 10), "short");
    // Exactly at the limit: unchanged.
    assert_eq!(truncate_for_log("0123456789", 10), "0123456789");

    // Multibyte input: truncation must not panic on a char boundary and
    // must keep exactly the first `max_chars` chars.
    let long = "é".repeat(3000);
    let out = truncate_for_log(&long, 1024);
    assert!(out.starts_with(&"é".repeat(1024)), "prefix preserved");
    assert!(
        out.chars().count() < 1100,
        "output must be truncated, got {} chars",
        out.chars().count()
    );
    assert!(out.contains("1976"), "marker notes the cut size");
}

/// Build a test router with the given route keys (all under provider "ds").
async fn test_router_with_routes(route_keys: &[&str]) -> Router {
    let mut toml = String::from(
        r#"
[providers.ds]
base_url = "http://x"
api_key = "k"
format = "chat"
"#,
    );
    for key in route_keys {
        toml.push_str(&format!(
            r#"[routes."{key}"]
model = "m"
context_window = 1000
"#
        ));
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, &toml).unwrap();
    let raw = crate::config::Config::parse_file(&path).unwrap();
    let validated = raw.validate().unwrap();

    let sessions = crate::session::SessionStore::new(168, 100, 16);
    let client = reqwest::Client::new();
    let state = Arc::new(AppState {
        config: Arc::new(RwLock::new(validated)),
        sessions,
        client,
        models: crate::models_cache::CatalogCache::new(),
        metrics: crate::metrics::Metrics::new(),
        version_tracker: Arc::new(crate::version::CodexVersionTracker::new()),
    });

    Router::new()
        .route("/v1/models", get(handle_models))
        .route("/models", get(handle_models))
        .route("/healthz", get(handle_healthz))
        .fallback(handle_fallback)
        .with_state(state)
}

#[tokio::test]
async fn models_chat_shape_unchanged_without_client_version() {
    let app = test_router_with_routes(&["ds/a", "ds/b"]).await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(Body::default())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().contains_key("etag"));
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["object"], "list");
    assert_eq!(v["data"].as_array().unwrap().len(), 2);
    assert_eq!(v["data"][0]["id"], "ds/a");
}

#[tokio::test]
async fn models_catalog_shape_with_client_version_and_304() {
    let app = test_router_with_routes(&["ds/a"]).await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/models?client_version=0.0.0")
                .body(Body::default())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let etag = resp.headers()["etag"].clone();
    let etag_str = etag.to_str().unwrap();
    assert!(
        etag_str.starts_with('"') && etag_str.ends_with('"'),
        "ETag must be an HTTP quoted-string: {etag_str}"
    );
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert!(v["models"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["slug"] == "ds/a"));

    // 304 on If-None-Match (same app state, same cache)
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/models?client_version=0.0.0")
                .header("if-none-match", &etag)
                .body(Body::default())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 304);
}

/// Pin the "unverified on any `None`" half of the doctor tripwire: two
/// values that both fail normalization must NOT compare equal and report a
/// false green (spec §3.2). Mirrors the archived
/// `doctor_verified_never_treats_two_unparseables_as_equal`.
#[test]
fn doctor_verified_never_treats_two_unparseables_as_equal() {
    // Both sides unnormalizable: must NOT be verified.
    assert!(!version_is_doctor_verified(None, Some("no digits here")));
    assert!(!version_is_doctor_verified(None, None));
    // One side missing: still unverified.
    assert!(!version_is_doctor_verified(Some("0.1.0"), None));
    assert!(!version_is_doctor_verified(None, Some("0.1.0")));
    // Genuine match, including across the `codex-cli X` prefix form.
    assert!(version_is_doctor_verified(
        Some("0.158.0"),
        Some("codex-cli 0.158.0")
    ));
    // Genuine mismatch.
    assert!(!version_is_doctor_verified(
        Some("0.158.0"),
        Some("0.157.0")
    ));
}

#[test]
fn etag_matches_accepts_quoted_weak_and_star_forms() {
    // The server emits `ETag: "hex"`; codex echoes that quoted value.
    assert!(etag_matches("\"4dcf1a\"", "4dcf1a"));
    // Weak form is a valid If-None-Match representation.
    assert!(etag_matches("W/\"4dcf1a\"", "4dcf1a"));
    // '*' means "any current representation".
    assert!(etag_matches("*", "4dcf1a"));
    // Comma-separated candidates are allowed by RFC 7232.
    assert!(etag_matches("\"4dcf1a\", \"zzz\"", "4dcf1a"));
    // A different tag must not match.
    assert!(!etag_matches("\"other\"", "4dcf1a"));
}

#[test]
fn first_content_event_recognizes_text_reasoning_and_tool_deltas() {
    assert!(is_first_content_event("response.output_text.delta"));
    assert!(is_first_content_event(
        "response.reasoning_summary_text.delta"
    ));
    assert!(is_first_content_event(
        "response.function_call_arguments.delta"
    ));
    assert!(!is_first_content_event("response.created"));
    assert!(!is_first_content_event("response.output_item.added"));
    assert!(!is_first_content_event("response.completed"));
}

#[test]
fn first_content_bytes_detects_content_marker_including_split_markers() {
    let marker = "response.output_text.delta";
    let split = marker.len() / 2;
    let mut carry = Vec::new();
    // Metadata-only chunk is not content.
    assert!(!first_content_event_bytes(
        b"event: response.created\n",
        &mut carry
    ));
    // A marker split across two chunks must be detected on the second chunk.
    assert!(!first_content_event_bytes(
        &marker.as_bytes()[..split],
        &mut carry
    ));
    assert!(first_content_event_bytes(
        &marker.as_bytes()[split..],
        &mut carry
    ));
    // Keepalive-only bytes are not content.
    let mut carry2 = Vec::new();
    assert!(!first_content_event_bytes(b": keepalive\n\n", &mut carry2));
    // Tool-call argument deltas are content too.
    let mut carry3 = Vec::new();
    assert!(first_content_event_bytes(
        b"event: response.function_call_arguments.delta\n",
        &mut carry3
    ));
}

#[test]
fn stream_metrics_error_class_skips_client_disconnects() {
    assert_eq!(
        stream_metrics_error_class(false, true, false),
        None,
        "client hang-up must not be counted as an upstream outcome"
    );
    assert_eq!(
        stream_metrics_error_class(true, false, false),
        Some(crate::metrics::ErrorClass::Empty)
    );
    assert_eq!(
        stream_metrics_error_class(false, false, false),
        Some(crate::metrics::ErrorClass::StreamTruncated)
    );
    // Proxy-initiated idle timeout: classified as a timeout unless the
    // stream had already delivered a completed response.
    assert_eq!(
        stream_metrics_error_class(false, false, true),
        Some(crate::metrics::ErrorClass::Timeout)
    );
    assert_eq!(
        stream_metrics_error_class(true, false, true),
        Some(crate::metrics::ErrorClass::Empty),
        "a trailing stall after a completed response is not a failure"
    );
    assert_eq!(
        stream_metrics_error_class(false, true, true),
        None,
        "client hang-up wins over the idle timer"
    );
}
