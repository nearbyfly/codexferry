//! Upstream HTTP transport for the proxy paths.
//! Extracted from `proxy/mod.rs` (module-split spec Phase 2).
//! NOTE: do not set `.timeout()` on streaming requests — reqwest's per-request
//! timeout is a TOTAL deadline enforced on every body read (issue #14).

use super::error_response;
use axum::http::StatusCode;
use axum::response::Response;
use std::time::Duration;

/// Send a POST to an upstream endpoint with the resolved Bearer token, JSON
/// content type, per-provider timeout, and any configured `extra_headers`.
///
/// Transport-level failures (DNS, connect, timeout) are mapped to a 502
/// Responses-shaped error; HTTP status handling is left to the caller.
///
/// Timeout semantics (issue #14): reqwest's per-request `.timeout()` is a
/// TOTAL deadline that keeps being enforced on every streaming-body read, so
/// it is only set for non-streaming requests. Streaming requests bound just
/// the header phase here (`tokio::time::timeout` around `send()`); their
/// body is governed by the idle timeout in the stream loops, so a healthy
/// stream may legitimately run longer than `timeout_ms`.
pub(super) async fn send_upstream(
    client: &reqwest::Client,
    url: &str,
    body: Vec<u8>,
    api_key: &str,
    route: &crate::config::ValidatedRoute,
    timeout: Duration,
    streaming: bool,
) -> Result<reqwest::Response, (Response, crate::metrics::ErrorClass)> {
    let mut builder = client
        .post(url)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .body(body);
    if !streaming {
        builder = builder.timeout(timeout);
    }
    if let Some(extra) = &route.provider.extra_headers {
        for (k, v) in extra {
            builder = builder.header(k, v);
        }
    }
    let sent = if streaming {
        tokio::time::timeout(timeout, builder.send()).await
    } else {
        Ok(builder.send().await)
    };
    match sent {
        Ok(result) => result.map_err(|e| {
            let error_class = if e.is_timeout() {
                crate::metrics::ErrorClass::Timeout
            } else {
                crate::metrics::ErrorClass::Network
            };
            (
                error_response(
                    StatusCode::BAD_GATEWAY,
                    "server_error",
                    &format!("upstream request failed: {e}"),
                ),
                error_class,
            )
        }),
        // Header-phase timeout (streaming only; non-streaming requests are
        // bounded end-to-end by reqwest's total deadline instead).
        Err(_elapsed) => Err((
            error_response(
                StatusCode::BAD_GATEWAY,
                "server_error",
                "upstream request timed out waiting for response headers",
            ),
            crate::metrics::ErrorClass::Timeout,
        )),
    }
}

/// Record a transport-level upstream failure with its error class (shared
/// by the chat and passthrough handlers).
pub(super) fn record_upstream_failure(
    metrics: &crate::metrics::Metrics,
    provider: &str,
    route: &str,
    model: &str,
    error_class: crate::metrics::ErrorClass,
) {
    metrics.record_request(provider, route, model, error_class);
}

/// Handle a non-2xx upstream response: read (and truncate-log) its body,
/// observe TTFT/duration, record the upstream request, and return the
/// Responses-shaped error. `path_label` distinguishes the two handlers in
/// the warn log ("chat path" / "passthrough path").
/// Signature fixed by the module-split plan (spec §2) — grouping params into
/// a struct would be a larger refactor than the dedup it replaces.
#[allow(clippy::too_many_arguments)]
pub(super) async fn upstream_non_2xx(
    metrics: &crate::metrics::Metrics,
    provider: &str,
    route: &str,
    model: &str,
    upstream_started: std::time::Instant,
    timeout: Duration,
    upstream_resp: reqwest::Response,
    path_label: &str,
) -> Response {
    let status = upstream_resp.status();
    let body = match tokio::time::timeout(timeout, upstream_resp.text()).await {
        Ok(Ok(text)) => text,
        _ => String::new(),
    };
    tracing::warn!(
        status = status.as_u16(),
        body = %super::truncate_for_log(&body, 1024),
        "upstream returned non-2xx ({path_label})"
    );
    let elapsed = upstream_started.elapsed().as_secs_f64();
    metrics.observe_ttft(provider, route, model, elapsed);
    metrics.observe_duration(provider, route, model, elapsed);
    metrics.record_request(
        provider,
        route,
        model,
        crate::metrics::ErrorClass::from_status(status.as_u16()),
    );
    super::error_response(status, "upstream_error", &body)
}

/// Classify and record a blocking body-read failure, returning the
/// Responses-shaped 502 error (shared by both handlers).
pub(super) fn body_read_failure(
    e: &reqwest::Error,
    metrics: &crate::metrics::Metrics,
    provider: &str,
    route: &str,
    model: &str,
) -> Response {
    let error_class = if e.is_timeout() {
        crate::metrics::ErrorClass::Timeout
    } else {
        crate::metrics::ErrorClass::Network
    };
    metrics.record_request(provider, route, model, error_class);
    super::error_response(
        StatusCode::BAD_GATEWAY,
        "server_error",
        &format!("failed to read upstream response: {e}"),
    )
}
