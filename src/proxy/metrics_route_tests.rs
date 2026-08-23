//! Unit tests for metrics_route_tests, extracted from `proxy.rs` (module-split spec Phase 2).
//! Bodies are verbatim moves.
use super::*;
use axum::body::Body;
use axum::http::StatusCode;
use tower::ServiceExt;

#[tokio::test]
async fn metrics_endpoint_returns_200() {
    let state = Arc::new(AppState {
        config: Arc::new(tokio::sync::RwLock::new(
            crate::config::ValidatedConfig::default(),
        )),
        sessions: crate::session::SessionStore::new(168, 100, 16),
        client: reqwest::Client::new(),
        models: crate::models_cache::CatalogCache::default(),
        metrics: crate::metrics::Metrics::new(),
        version_tracker: Arc::new(crate::version::CodexVersionTracker::new()),
    });
    // Record a sample metric so the registry has data to encode.
    state.metrics.record_request(
        "test",
        "test/route",
        "test-model",
        crate::metrics::ErrorClass::Empty,
    );
    let app = build_router(state.clone());
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 64)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("upstream_requests_total"), "body: {text}");
}
