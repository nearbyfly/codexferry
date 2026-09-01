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
use crate::upstream::{chat_url, is_done, parse_sse_stream, resolve_api_key_async, responses_url};
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
///
/// Allocation-free on the steady-state relay path: a marker lying entirely
/// within `carry` was necessarily found by the previous call (the carry is
/// the tail of that call's scanned region), so only two regions need
/// scanning — `chunk` itself, and the cross-boundary stitches of a carry
/// suffix plus a chunk prefix — and the carry update reuses the Vec's
/// capacity (`clear`/`drain` + `extend`) instead of reallocating.
fn first_content_event_bytes(chunk: &[u8], carry: &mut Vec<u8>) -> bool {
    let hit = FIRST_CONTENT_MARKERS.iter().any(|marker| {
        let m = marker.as_bytes();
        if chunk.windows(m.len()).any(|w| w == m) {
            return true;
        }
        // Cross-boundary: marker starts at carry[start..], finishes in the
        // chunk prefix. Starts whose suffix outgrows the marker would be
        // fully-inside-carry matches — skipped by the invariant above (and
        // guarded here against underflow, since carry can outgrow the
        // shortest marker).
        (0..carry.len()).any(|start| {
            let from_carry = carry.len() - start;
            from_carry <= m.len()
                && carry[start..] == m[..from_carry]
                && m.len() - from_carry <= chunk.len()
                && chunk[..m.len() - from_carry] == m[from_carry..]
        })
    });
    // Keep the last (longest marker − 1) bytes of (carry + chunk) as the
    // next carry. In-place Vec operations only: capacity is retained, so
    // steady-state relaying allocates nothing.
    let max_marker_len = FIRST_CONTENT_MARKERS
        .iter()
        .map(|m| m.len())
        .max()
        .unwrap_or(0);
    let keep = (max_marker_len.saturating_sub(1)).min(carry.len() + chunk.len());
    if chunk.len() >= keep {
        // The new carry comes entirely out of chunk's tail.
        carry.clear();
        carry.extend_from_slice(&chunk[chunk.len() - keep..]);
    } else {
        let from_carry = keep - chunk.len();
        carry.drain(..carry.len() - from_carry);
        carry.extend_from_slice(chunk);
    }
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
/// Wrapped in `Arc` by [`serve`], so cloning is cheap and all handlers share
/// the same config, session store, HTTP client, model-catalog cache, metrics
/// registry, and codex client-version tripwire.
pub struct AppState {
    pub config: SharedConfig,
    pub sessions: SessionStore,
    pub client: reqwest::Client,
    pub models: Arc<crate::models_cache::CatalogCache>,
    pub metrics: crate::metrics::Metrics,
    /// Per-process codex client-version tripwire (spec §3): remembers which
    /// versions this daemon has already reported so the "rerun doctor"
    /// reminder fires once per version, not once per request.
    pub version_tracker: Arc<crate::version::CodexVersionTracker>,
    /// Path to the doctor state file (under `XDG_STATE_HOME`/`~/.local/state`).
    /// Injected rather than resolved here so tests never touch the developer's
    /// real state file (`DoctorState::read()` would consult `HOME`).
    pub doctor_state_path: std::path::PathBuf,
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
    // hot-reloads it in place via a channel + async applier (updates are
    // queued while the lock is busy, never dropped - AGENTS.md #7).
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
        models: Arc::new(crate::models_cache::CatalogCache::new()),
        metrics: crate::metrics::Metrics::new(),
        version_tracker: Arc::new(crate::version::CodexVersionTracker::new()),
        doctor_state_path: crate::version::state_path(),
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
///
/// A present `client_version` also feeds the version tripwire — including on
/// the 304 path, since a revalidation is still a codex turn (spec §3).
async fn handle_models(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ModelsQuery>,
    headers: HeaderMap,
) -> Response {
    match q.client_version {
        Some(v) => {
            // Runs before the If-None-Match short-circuit below so a 304
            // revalidation still observes the version.
            observe_client_version(&state, &v);

            // Codex ModelsResponse catalog shape (live model catalog). No
            // config read guard is held here: get() takes its own short
            // guards internally, and an outer guard kept alive across the
            // await can circular-wait with the hot-reload applier's write
            // lock (tokio RwLock is write-preferring) - PR #6 review
            // issue 1.
            let (etag, body) = state.models.get(&state.config).await;

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
            // This branch reads the config directly, so it takes (and
            // releases) its own guard.
            let config = state.config.read().await;
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

/// Sentinel tracker key for a `client_version` with no version-ish token.
///
/// `client_version` is caller-supplied, and the tracker's `seen` set keeps
/// one PERMANENT entry per distinct string — it never evicts. Collapsing
/// every unparseable value onto this single sentinel folds all digit-less
/// junk into ONE series instead of one permanent entry per junk string
/// (spec §3). Deliberately not version-shaped so it can never be mistaken
/// for a real version.
const UNPARSEABLE_CLIENT_VERSION: &str = "unparseable";

/// Longest `client_version` accepted as a metric label, in bytes.
///
/// `normalize_version` is a token PICKER, not a validator — it happily
/// returns a 4 KiB token as long as it holds a digit. Real codex versions are
/// well under this; anything longer is caller noise.
const MAX_CLIENT_VERSION_LEN: usize = 32;

/// Is every byte of `token` safe to render inside a Prometheus label value?
///
/// `prometheus-client` 0.25 does NO escaping of label values (its
/// `EncodeLabelValue for &str` is a bare `write_str`), and `client_version` is
/// the only caller-derived label in the whole registry — every other one comes
/// from validated config. An unescaped `"` or `}` would close the label set
/// early and corrupt the entire `/metrics` scrape (e.g. `client_version=1"}`
/// renders `codex_client_info{version="1"}"} 1`). The allowed set covers every
/// character real version strings use, including semver pre-release/build
/// separators.
fn is_safe_version_token(token: &str) -> bool {
    token
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'_' | b'-'))
}

/// Turn a normalized `client_version` into a bounded, injection-safe label.
///
/// Accepts the token only when it is non-empty, within
/// [`MAX_CLIENT_VERSION_LEN`] bytes, and passes [`is_safe_version_token`];
/// every other value — including a failed normalization — collapses onto
/// [`UNPARSEABLE_CLIENT_VERSION`].
///
/// The emptiness check is not redundant: an empty token satisfies both other
/// guards (length 0 is under the cap, and the charset check is vacuously true
/// over zero bytes), so without it `Some("")` would emit a bare
/// `version=""` label. `normalize_version` never returns one today, but this
/// helper is shared and must not rely on a caller's invariant.
fn client_version_label_from(normalized: Option<&str>) -> String {
    match normalized {
        Some(v)
            if !v.is_empty() && v.len() <= MAX_CLIENT_VERSION_LEN && is_safe_version_token(v) =>
        {
            v.to_string()
        }
        _ => UNPARSEABLE_CLIENT_VERSION.to_string(),
    }
}

/// Has `doctor` verified this exact codex version green?
///
/// `observed` is the already-normalized observed version (`None` when the
/// caller's value had no version-ish token); `last_green` is the raw
/// `last_green` field from doctor's state file, normalized here.
///
/// A `None` on EITHER side means "unverified", NEVER "equal" (spec §3.2).
/// Comparing the two `Option`s directly would make two failed normalizations
/// compare equal and report a false green.
fn version_is_doctor_verified(observed: Option<&str>, last_green: Option<&str>) -> bool {
    match (
        observed,
        last_green.and_then(crate::version::normalize_version),
    ) {
        (Some(seen), Some(verified)) => seen == verified,
        _ => false,
    }
}

/// Codex client-version tripwire (spec §3).
///
/// On the FIRST sighting of a version this process, emits one `info!` line
/// and warns while that version has not been verified green by `doctor`.
/// Repeat sightings are entirely silent — these are once-per-version EVENT
/// logs, not a second per-request line (the AGENTS.md #11 exception).
///
/// The raw value is normalized before it is observed, so the tracker's
/// `seen` set and log lines are keyed by normalized version; digit-less
/// input collapses onto [`UNPARSEABLE_CLIENT_VERSION`] instead of one
/// permanent entry per junk string.
///
/// Doctor's state is read from the production state file. Reading it is
/// best-effort and infallible: a missing or malformed file simply means
/// "never green"; the daemon never writes it — doctor is the only writer.
fn observe_client_version(state: &AppState, raw: &str) {
    // Two values from ONE normalization: `normalized` is `None` for an
    // unparseable input and drives the doctor-verified comparison (where
    // `None` means "unverified"), while `label` collapses that case — and
    // any unsafe/over-long token — onto the sentinel so the tracker and the
    // gauge stay bounded (see [`client_version_label_from`]).
    let normalized = crate::version::normalize_version(raw);
    let label = client_version_label_from(normalized.as_deref());
    let Some(transition) = state.version_tracker.observe(&label) else {
        return; // Already reported this version; stay silent.
    };
    let from = transition.from.as_deref().unwrap_or("(none)");
    let to = &transition.to;
    tracing::info!("codex client {from} → {to} detected — rerun `codexferry doctor`");
    state.metrics.record_codex_client(to);

    let doctor_state = crate::version::DoctorState::read_from(&state.doctor_state_path);
    if !version_is_doctor_verified(normalized.as_deref(), doctor_state.last_green.as_deref()) {
        let last = doctor_state.last_green.as_deref().unwrap_or("none");
        tracing::warn!("codex {to} not verified by doctor (last green: {last})");
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
            log_request(
                "?",
                "?",
                "?",
                status.as_u16(),
                0,
                0,
                started.elapsed(),
                Some(&format!("failed to parse request: {e}")),
            );
            return resp;
        }
    };

    let model_key = req.model.clone();

    // Look up the route by model key under a short config read guard. The
    // guard is dropped BEFORE the session lookup below: `sessions.get` takes
    // the session store's own write lock and deep-clones the full history,
    // and holding the config read lock across that await would stretch the
    // hold while a queued hot-reload write blocks new readers (tokio RwLock
    // is write-preferring).
    let route = {
        let config = state.config.read().await;
        match config.routes.get(&model_key) {
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
                    Some(&format!("no route for model: {model_key}")),
                );
                return resp;
            }
        }
    };

    // Merge stored history from previous_response_id (spec §8.3): a hit
    // prepends the prior conversation, a miss (expired/evicted/restart)
    // degrades gracefully to an empty history. Runs outside the config
    // guard — see the comment above.
    let history = if let Some(prev_id) = &req.previous_response_id {
        state.sessions.get(prev_id).await.unwrap_or_default()
    } else {
        Vec::new()
    };

    // Resolve the provider's API key (api_key → env → file); a failure here
    // is a server-side configuration problem, so return 500. The file source
    // is read on the blocking pool (see `resolve_api_key_async`).
    let api_key = match resolve_api_key_async(&route.provider).await {
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
                Some(&e),
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
        let (resp, error_detail) = extract_error_detail(resp).await;
        log_request(
            &model_key,
            &upstream_model,
            &req.model,
            status,
            input_tokens,
            output_tokens,
            started.elapsed(),
            error_detail.as_deref(),
        );
        return resp;
    }
    resp
}

/// Read a 4xx/5xx response body and extract `error.message` for the
/// per-request log line. Returns the reconstructed response alongside the
/// extracted detail so the client still receives the original body.
/// Non-error statuses pass through untouched without reading the body.
///
/// The read has no size cap: error bodies are already fully buffered by
/// `upstream_non_2xx` before `error_response` serializes them, so this adds
/// no new memory exposure. A cap here would silently strip oversized error
/// bodies from the client response (to_bytes -> Err -> empty body).
async fn extract_error_detail(resp: Response) -> (Response, Option<String>) {
    if !resp.status().is_client_error() && !resp.status().is_server_error() {
        return (resp, None);
    }
    let (parts, body) = resp.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();
    let detail = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(String::from));
    (
        Response::from_parts(parts, axum::body::Body::from(bytes)),
        detail,
    )
}

/// Emit the single per-request log line (spec §11): route, upstream, model,
/// status code, input/output tokens, duration, and (for 4xx/5xx) the error
/// message returned to the client - the detail that makes the log line
/// self-sufficient for debugging without re-sending the request.
///
/// Uses explicit `%`-style Display field assignments (AGENTS.md #3) because the
/// route/upstream/model values are borrowed `&str`s rather than locals that
/// could be auto-captured with `{field}` placeholders.
#[allow(clippy::too_many_arguments)]
fn log_request(
    route: &str,
    upstream: &str,
    model: &str,
    status: u16,
    input_tokens: u32,
    output_tokens: u32,
    elapsed: Duration,
    error: Option<&str>,
) {
    tracing::info!(
        route = %route,
        upstream = %upstream,
        model = %model,
        status = status,
        input_tokens = input_tokens,
        output_tokens = output_tokens,
        elapsed_ms = elapsed.as_millis() as u64,
        error = error.unwrap_or(""),
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
