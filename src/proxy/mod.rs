//! The axum HTTP server: routing, request dispatch, and streaming orchestration.
//!
//! ## Architecture
//!
//! [`AppState`] (wrapped in `Arc` by [`serve`]) bundles the hot-reloadable
//! config ([`SharedConfig`]), the in-memory [`SessionStore`], and the pooled
//! `reqwest` client. The server exposes:
//!
//! - `POST /v1/responses` (and the `/responses` alias) — the main entry,
//!   dispatched by [`handle_responses`] to a chat-format or responses-format
//!   handler based on the route's provider `format`.
//! - `GET /v1/models` (and `/models`) — the aggregated model list.
//! - `GET /healthz` — liveness probe.
//! - fallback — 404 in Responses error shape.
//!
//! ## Chat-format handler
//!
//! [`handle_chat_format`] converts the Responses request to Chat Completions
//! and forwards it upstream. Streaming requests are answered by a spawned task
//! that walks the upstream SSE stream (via [`parse_sse_stream`]), feeds each
//! chunk through a [`StreamConverter`], and pushes the resulting Responses SSE
//! events over an `mpsc` channel wrapped in `ReceiverStream`. Non-streaming
//! requests read the full upstream body and convert it in one shot. The full
//! conversation context is saved to the [`SessionStore`] on success.
//!
//! ## Responses-format handler
//!
//! [`handle_responses_format`] relays the upstream SSE stream through (raw
//! bytes via `Body::from_stream`; event-granular healing when the `dsml_heal`
//! / `think_tags` quirks are on), and keys the session by the upstream's
//! `response.id` extracted from the final `response.completed` event
//! (AGENTS.md #4).
//!
//! ## Logging & errors
//!
//! Each request emits exactly one `tracing` line (spec §11); streaming
//! requests log from their spawned task once the stream finishes. Errors are
//! serialized by [`error_response`] into the Responses error shape (spec §10).
//!
use crate::config::{spawn_watcher, Config, SharedConfig};
use crate::convert::request::to_chat_request_with_ns_map;
use crate::convert::response::{build_completed_response, chat_response_to_items, StreamConverter};
use crate::logging;
use crate::session::SessionStore;
use crate::upstream::{chat_url, is_done, parse_sse_stream, resolve_api_key, responses_url};
use crate::wire::chat::{ChatResponse, ChatStreamChunk};
use crate::wire::responses::ResponsesRequest;
mod capture;
mod chat;
mod passthrough;
use passthrough::handle_responses_format;
mod upstream;
use chat::handle_chat_format;

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{sse::Event, sse::Sse, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use bytes::Bytes;
use capture::{input_items, save_session, store_enabled};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio_stream::wrappers::ReceiverStream;

/// Whether request/response body tracing is enabled (spec §11).
///
/// Controlled by `CODEXFERRY_TRACE_BODY=1`; meant for local debugging only
/// (request/response bodies may contain sensitive content).
fn trace_body_enabled() -> bool {
    std::env::var("CODEXFERRY_TRACE_BODY")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// Log a body at debug level when CODEXFERRY_TRACE_BODY=1.
///
/// The raw bytes are rendered lossy to UTF-8; bodies are only ever logged for
/// debugging, never in normal operation.
fn trace_body(label: &str, body: &[u8]) {
    if trace_body_enabled() {
        tracing::debug!("{label}: {}", String::from_utf8_lossy(body));
    }
}

/// Truncate a string for inclusion in a log line at `max_chars` characters,
/// char-boundary safe, with a marker noting how much was cut.
fn truncate_for_log(s: &str, max_chars: usize) -> String {
    let total = s.chars().count();
    if total <= max_chars {
        return s.to_string();
    }
    let end = s
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    format!("{}…[truncated {} chars]", &s[..end], total - max_chars)
}

/// Query params accepted by `/models`. `client_version` is sent unconditionally
/// by Codex's ModelsClient and switches the response to the ModelsResponse
/// catalog shape (spec: live model catalog, issue #10).
#[derive(serde::Deserialize, Default)]
struct ModelsQuery {
    #[serde(default)]
    client_version: Option<String>,
}

/// RFC 7232 match for an `If-None-Match` header value against the emitted ETag.
///
/// Handles the quoted opaque-tag the server sends (`"hex"`), the weak form
/// (`W/"hex"`), the `*` wildcard, and comma-separated candidate lists.
fn etag_matches(if_none: &str, etag: &str) -> bool {
    if if_none.trim() == "*" {
        return true;
    }
    fn normalize(v: &str) -> &str {
        let v = v.trim();
        let v = v.strip_prefix("W/").unwrap_or(v);
        v.trim_matches('"').trim()
    }
    let current = normalize(etag);
    if_none
        .split(',')
        .any(|candidate| normalize(candidate) == current)
}

/// Event types that carry the first content of an upstream turn (text,
/// reasoning, or tool-call arguments) - the TTFT trigger (spec §3-4).
fn is_first_content_event(event_type: &str) -> bool {
    matches!(
        event_type,
        "response.output_text.delta"
            | "response.reasoning_summary_text.delta"
            | "response.function_call_arguments.delta"
    )
}

/// Raw-byte markers for the same first-content events, used by the
/// passthrough fast path which relays bytes verbatim (spec §3-4).
const FIRST_CONTENT_MARKERS: [&str; 3] = [
    "response.output_text.delta",
    "response.reasoning_summary_text.delta",
    "response.function_call_arguments.delta",
];

/// Detect a first-content marker in raw relayed bytes.
///
/// `carry` keeps the trailing bytes of the previous chunk so a marker split
/// across chunk boundaries still matches (same boundary concern as the
/// hand-written SSE parser, AGENTS.md #2). The forwarded bytes are never
/// modified; this only reads them.
fn first_content_event_bytes(chunk: &[u8], carry: &mut Vec<u8>) -> bool {
    let mut haystack = Vec::with_capacity(carry.len() + chunk.len());
    haystack.extend_from_slice(carry);
    haystack.extend_from_slice(chunk);
    let hit = FIRST_CONTENT_MARKERS.iter().any(|marker| {
        haystack
            .windows(marker.len())
            .any(|w| w == marker.as_bytes())
    });
    let max_marker_len = FIRST_CONTENT_MARKERS
        .iter()
        .map(|m| m.len())
        .max()
        .unwrap_or(0);
    let keep = haystack
        .len()
        .saturating_sub(max_marker_len.saturating_sub(1));
    *carry = haystack[keep..].to_vec();
    hit
}

/// Upstream outcome for stream-end metrics.
///
/// Returns `None` when the end was client-initiated: a hang-up is not an
/// upstream outcome and must not be counted (spec §3-4 error classes). A
/// proxy-initiated idle timeout counts as `Timeout` — unless the stream had
/// already delivered a completed response, in which case a trailing stall
/// does not fail the response itself.
fn stream_metrics_error_class(
    saw_completed: bool,
    client_disconnected: bool,
    timed_out: bool,
) -> Option<crate::metrics::ErrorClass> {
    if client_disconnected {
        return None;
    }
    Some(if saw_completed {
        crate::metrics::ErrorClass::Empty
    } else if timed_out {
        crate::metrics::ErrorClass::Timeout
    } else {
        crate::metrics::ErrorClass::StreamTruncated
    })
}

/// Shared application state handed to every handler via axum's `State`.
///
/// Wrapped in `Arc` by [`serve`], so cloning is cheap and all handlers share the
/// same config, session store, and HTTP client.
pub struct AppState {
    pub config: SharedConfig,
    pub sessions: SessionStore,
    pub client: reqwest::Client,
    pub models: crate::models_cache::CatalogCache,
    pub metrics: crate::metrics::Metrics,
}

/// Decrements the in-flight gauge when dropped.
///
/// Created right before the upstream call so every return path (success,
/// error, client disconnect) is covered; streaming branches move the guard
/// into the spawned task, which drops it once the stream is fully drained.
struct InFlightGuard {
    metrics: crate::metrics::Metrics,
    provider: String,
    route: String,
}

impl InFlightGuard {
    fn new(metrics: crate::metrics::Metrics, provider: &str, route: &str) -> Self {
        metrics.inc_in_flight(provider, route);
        Self {
            metrics,
            provider: provider.to_string(),
            route: route.to_string(),
        }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.metrics.dec_in_flight(&self.provider, &self.route);
    }
}

/// Start the proxy daemon (logging + config path resolution + [`serve`]).
pub async fn run() -> anyhow::Result<()> {
    // Initialize tracing (log level controlled by RUST_LOG).
    logging::init();

    // Config path: CODEXFERRY_CONFIG, else ./cxf.toml, else the legacy
    // ./config.toml (with a deprecation warning) - see
    // `config::default_config_path`.
    let config_path = crate::config::default_config_path();

    serve(&config_path, signal_shutdown()).await
}

/// Resolve SIGINT/SIGTERM into a shutdown future that logs the reason
/// (spec §12; systemd Type=simple sends SIGTERM via KillSignal).
async fn signal_shutdown() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("ctrl_c handler");
        "SIGINT"
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler")
            .recv()
            .await;
        "SIGTERM"
    };
    #[cfg(unix)]
    let reason = tokio::select! { r = ctrl_c => r, r = terminate => r };
    #[cfg(not(unix))]
    let reason = ctrl_c.await;
    tracing::info!("received {reason}, shutting down");
}

/// Load config, start the watcher/session store, bind and serve until
/// `shutdown` completes.
///
/// Startup sequence: load and validate config → log the loaded routes → spawn
/// the config hot-reload watcher → build the pooled HTTP client → create the
/// `SessionStore` and its hourly cleanup task → assemble the router → bind the
/// listener → serve with graceful shutdown when `shutdown` resolves.
///
/// Extracted from [`run`] so `doctor --live` can host an in-process router
/// against a temporary config (main entry behavior is unchanged).
pub async fn serve(
    config_path: &std::path::Path,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    // Parse + validate: route keys must be provider/alias, providers must
    // reference real entries, and every provider needs a resolvable API key.
    let raw_config = Config::parse_file(config_path)?;
    let validated = raw_config.validate()?;

    tracing::info!(
        "loaded {} routes across {} providers",
        validated.routes.len(),
        validated.providers.len()
    );
    for (key, route) in &validated.routes {
        tracing::info!(
            "  route: {key} -> {} ({})",
            route.model,
            route.provider.format
        );
    }

    // Share the validated config behind an RwLock; the notify watcher
    // hot-reloads it in place (non-blocking try_write, AGENTS.md #7).
    let shared: SharedConfig = Arc::new(RwLock::new(validated.clone()));
    let _watcher = spawn_watcher(config_path, shared.clone())?;

    // Pooled HTTP client shared by all handlers. A 90s idle timeout avoids
    // errors from upstreams closing idle keep-alive connections.
    let client = reqwest::Client::builder()
        .pool_idle_timeout(Duration::from_secs(90))
        .build()?;

    // In-memory session store: TTL (hours), LRU count cap, and a memory budget.
    let sessions = SessionStore::new(
        validated.session.ttl_hours,
        validated.session.max_sessions,
        validated.session.max_memory_mb,
    );

    // Background session cleanup task (spec §8.5: hourly cleanup of expired sessions).
    {
        let cleanup_sessions = sessions.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(3600));
            interval.tick().await; // skip immediate first tick
            loop {
                interval.tick().await;
                cleanup_sessions.cleanup().await;
            }
        });
    }

    // Bundle shared state; handlers extract it via axum's State extractor.
    let state = Arc::new(AppState {
        config: shared,
        sessions,
        client,
        models: crate::models_cache::CatalogCache::new(),
        metrics: crate::metrics::Metrics::new(),
    });

    // Delegate to build_router (extracted so tests can reuse it).
    let app = build_router(state);

    // Bind the TCP listener; errors (e.g. port in use) fail startup.
    let host = validated.server.host.as_str();
    let port = validated.server.port;
    let listener = tokio::net::TcpListener::bind((host, port)).await?;
    tracing::info!("codexferry listening on {host}:{port}");

    // Serve until `shutdown` completes: stop accepting new connections, then
    // let in-flight streaming requests finish (spec §12).
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;

    Ok(())
}

/// Liveness probe (`GET /healthz`) used by systemd and integration tests.
async fn handle_healthz() -> &'static str {
    "ok"
}

/// List models (`GET /v1/models`, `/models`).
///
/// Two response shapes:
/// - Without `client_version`: OpenAI Chat Completions list shape
///   (`{"object": "list", "data": [...]}`), sorted by route key.
/// - With `client_version`: Codex ModelsResponse catalog shape
///   (`{"models": [...]}`), served from the [`CatalogCache`].
///
/// Both shapes return an `ETag` header and support `If-None-Match` → 304.
async fn handle_models(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ModelsQuery>,
    headers: HeaderMap,
) -> Response {
    let config = state.config.read().await;

    match q.client_version {
        Some(_) => {
            // Codex ModelsResponse catalog shape (live model catalog).
            let (etag, body) = state.models.get(&config);
            drop(config);

            if let Some(if_none) = headers.get("if-none-match").and_then(|v| v.to_str().ok()) {
                if etag_matches(if_none, &etag) {
                    return StatusCode::NOT_MODIFIED.into_response();
                }
            }

            let etag_header = format!("\"{etag}\"");
            (
                [
                    (axum::http::header::ETAG, etag_header.as_str()),
                    (axum::http::header::CONTENT_TYPE, "application/json"),
                ],
                body,
            )
                .into_response()
        }
        None => {
            // Chat-Completions list shape (same body as before, but now with ETag + 304).
            let mut keys: Vec<&String> = config.routes.keys().collect();
            keys.sort();
            let data: Vec<Value> = keys
                .iter()
                .map(|key| {
                    let owned_by = key.split_once('/').map(|(p, _)| p).unwrap_or("unknown");
                    json!({"id": key, "object": "model", "owned_by": owned_by})
                })
                .collect();
            let body = serde_json::to_vec(&json!({"object": "list", "data": data}))
                .map(Bytes::from)
                .unwrap_or_default();
            drop(config);

            let etag = crate::models_cache::weak_etag(&body);

            if let Some(if_none) = headers.get("if-none-match").and_then(|v| v.to_str().ok()) {
                if etag_matches(if_none, &etag) {
                    return StatusCode::NOT_MODIFIED.into_response();
                }
            }

            let etag_header = format!("\"{etag}\"");
            (
                [
                    (axum::http::header::ETAG, etag_header.as_str()),
                    (axum::http::header::CONTENT_TYPE, "application/json"),
                ],
                body,
            )
                .into_response()
        }
    }
}

/// Fallback for any unknown path: a 404 in the Responses error shape.
async fn handle_fallback() -> Response {
    error_response(StatusCode::NOT_FOUND, "not_found", "unknown endpoint")
}

/// Build the axum Router with all routes and shared state.
///
/// Extracted from [`serve`] so tests can build a router against synthetic
/// state without binding a listener.
pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/responses", post(handle_responses))
        .route("/responses", post(handle_responses))
        .route("/v1/models", get(handle_models))
        .route("/models", get(handle_models))
        .route("/healthz", get(handle_healthz))
        .route("/metrics", get(handle_metrics))
        .fallback(handle_fallback)
        .with_state(state)
}

/// Serve Prometheus-format upstream metrics (spec §6).
async fn handle_metrics(State(state): State<Arc<AppState>>) -> Response {
    let mut buf = String::new();
    match state.metrics.encode(&mut buf) {
        Ok(()) => (
            [(
                "content-type",
                "application/openmetrics-text; version=1.0.0; charset=utf-8",
            )],
            buf,
        )
            .into_response(),
        Err(e) => {
            tracing::warn!("metrics encode error: {e}");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "metrics encode failed",
            )
        }
    }
}

/// Main entry point for `POST /v1/responses` (and the `/responses` alias).
///
/// Dispatch flow (spec §6):
/// 1. Trace the raw request body when `CODEXFERRY_TRACE_BODY=1`.
/// 2. Parse the JSON body; a parse failure is a 400 Responses error.
/// 3. Look up the route by `model` (`provider/alias`); unknown models get a
///    400 with "no route for model".
/// 4. When `previous_response_id` is present, merge the stored conversation
///    history from the [`SessionStore`] (a cache miss degrades to new input
///    only, with a warning).
/// 5. Resolve the provider API key; an unresolvable key is a 500.
/// 6. Branch on the provider `format`: chat → [`handle_chat_format`],
///    responses → [`handle_responses_format`].
///
/// Logging: non-streaming requests and failures that occur before a stream
/// task is spawned are logged here; successful streaming requests log their
/// single line from the spawned task (see [`handle_chat_format`]).
async fn handle_responses(State(state): State<Arc<AppState>>, body: axum::body::Bytes) -> Response {
    let started = std::time::Instant::now();

    // Trace request body if enabled (spec §11).
    trace_body("request body", &body);

    // Parse the Responses request body; report malformed JSON as a 400.
    let req: ResponsesRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            let status = StatusCode::BAD_REQUEST;
            let resp = error_response(
                status,
                "invalid_request_error",
                &format!("failed to parse request: {e}"),
            );
            log_request("?", "?", "?", status.as_u16(), 0, 0, started.elapsed());
            return resp;
        }
    };

    let model_key = req.model.clone();

    // Look up the route by model key; both the route and any merged session
    // history are fetched under one config read lock.
    let (route, history) = {
        let config = state.config.read().await;
        let route = match config.routes.get(&model_key) {
            Some(r) => r.clone(),
            None => {
                let status = StatusCode::BAD_REQUEST;
                let resp = error_response(
                    status,
                    "invalid_request_error",
                    &format!("no route for model: {model_key}"),
                );
                log_request(
                    &model_key,
                    "?",
                    &model_key,
                    status.as_u16(),
                    0,
                    0,
                    started.elapsed(),
                );
                return resp;
            }
        };

        // Merge stored history from previous_response_id (spec §8.3): a hit
        // prepends the prior conversation, a miss (expired/evicted/restart)
        // degrades gracefully to an empty history.
        let history = if let Some(prev_id) = &req.previous_response_id {
            state.sessions.get(prev_id).await.unwrap_or_default()
        } else {
            Vec::new()
        };

        (route, history)
    };

    // Resolve the provider's API key (api_key → env → file); a failure here
    // is a server-side configuration problem, so return 500.
    let api_key = match resolve_api_key(&route.provider) {
        Ok(k) => k,
        Err(e) => {
            let status = StatusCode::INTERNAL_SERVER_ERROR;
            let resp = error_response(status, "server_error", &e);
            log_request(
                &model_key,
                &route.model,
                &model_key,
                status.as_u16(),
                0,
                0,
                started.elapsed(),
            );
            return resp;
        }
    };

    let upstream_model = route.model.clone();
    let is_chat = route.provider.format == "chat";

    // Branch by provider format: convert (chat) or pass through (responses).
    // Both return (response, input_tokens, output_tokens).
    let (resp, input_tokens, output_tokens) = if is_chat {
        handle_chat_format(&state, &req, &history, &route, &api_key, &upstream_model).await
    } else {
        handle_responses_format(&state, &req, &history, &route, &api_key, &upstream_model).await
    };
    let status = resp.status().as_u16();
    // Streaming requests emit their single per-request line from the spawned
    // task (real token counts are only known once the stream completes). But
    // when a streaming request fails *before* the stream task is spawned
    // (upstream 502, non-2xx, ...), no task exists to log -- so log errors
    // here too, keeping one line per request on every path.
    if !req.stream || resp.status().is_client_error() || resp.status().is_server_error() {
        log_request(
            &model_key,
            &upstream_model,
            &req.model,
            status,
            input_tokens,
            output_tokens,
            started.elapsed(),
        );
    }
    resp
}

/// Emit the single per-request log line (spec §11): route, upstream, model,
/// status code, input/output tokens, and duration.
///
/// Uses explicit `%`-style Display field assignments (AGENTS.md #3) because the
/// route/upstream/model values are borrowed `&str`s rather than locals that
/// could be auto-captured with `{field}` placeholders.
fn log_request(
    route: &str,
    upstream: &str,
    model: &str,
    status: u16,
    input_tokens: u32,
    output_tokens: u32,
    elapsed: Duration,
) {
    tracing::info!(
        route = %route,
        upstream = %upstream,
        model = %model,
        status = status,
        input_tokens = input_tokens,
        output_tokens = output_tokens,
        elapsed_ms = elapsed.as_millis() as u64,
        "request handled"
    );
}

/// Build a Responses-shaped error response (spec §10):
/// `{"error": {"type": ..., "message": ...}}` with the given HTTP status.
pub fn error_response(status: StatusCode, error_type: &str, message: &str) -> Response {
    let body = json!({"error": {"type": error_type, "message": message}});
    (status, Json(body)).into_response()
}
#[cfg(test)]
mod tests;

#[cfg(test)]
mod metrics_route_tests;
