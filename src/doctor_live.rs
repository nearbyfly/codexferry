//! `doctor --live`: in-process wire-shape probe + offline tool round-trip.
//!
//! Topology (all local, no real provider, no tokens):
//!
//! ```text
//! codex exec ──> temporary router (in-process `proxy::serve`) ──> mock upstream
//! ```
//!
//! The mock upstream speaks both wire formats (`/v1/responses` passthrough
//! shape, `/v1/chat/completions` converted shape). On the first turn it
//! streams a tool call whose name/arguments are synthesized from the tool
//! list the ROUTER forwarded — which is also the first assertion: if Codex
//! switched to a dialect delivery (`additional_tools` input items, empty
//! top-level `tools`) and the router failed to normalize it, `pick_tool`
//! finds nothing and the probe fails loudly instead of leaking markup.
//! The tool call touches a sentinel file, proving the full round-trip
//! (model → router → codex → local execution → tool output back upstream).
//!
//! The probe wiring mirrors the user's ACTUAL codex mode (spec
//! 2026-08-23 §Mode detection): `run()` reads `~/.codex/config.toml` and
//! dispatches on [`crate::mode::Mode`] — dynamic uses an `auth.command`
//! (codex fetches `/v1/models` live from the in-process router), pinned
//! pins the generated catalog, fallback (env_key only, no pin) is the
//! degraded metadata path. The live-fetch assertion is the codex-side
//! equivalent of the spec's router-log line: `models_cache.json` appears
//! in codex's CODEX_HOME only after a successful `/v1/models` fetch.

use crate::doctor::{print_report, report_has_fail, Check};
use crate::mode::{self, Mode};
use axum::routing::post;
use axum::Router;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// The input-item-type contract lives in `normalize.rs` as the single
// source of truth (shared with the request-side visibility warns).
use crate::normalize::KNOWN_INPUT_ITEM_TYPES;

/// Live-probe entry point: runs both routes, prints the report, exits 1/2.
///
/// Detects the user's wiring mode from `~/.codex/config.toml` (spec
/// 2026-08-23 §Mode detection) and threads it into the probe so the probe
/// tests what the user actually runs. Dynamic mode uses an auth command and
/// no pin (codex fetches `/v1/models` live); pinned mode pins the generated
/// temp catalog; fallback uses env_key only and is the degraded path. The
/// `config_path` argument — codexferry's own config — stays unused by the
/// probe itself (it generates its own temp router config), but the probe
/// wiring now depends on the real `~/.codex/config.toml` via the detected
/// mode.
pub fn run(_config_path: &Path) -> anyhow::Result<()> {
    // TODO(spec §3.2 item 6): on failure print last 20 lines of router log.
    // The in-process router installs no tracing subscriber → no router logs.
    //

    // Environment gate: codex must be installed AND runnable (exit 2 =
    // environment). A spawn failure or a codex binary that errors on
    // `--version` both mean the live probe cannot work — treat either as an
    // environment failure rather than letting it surface later as a probe
    // FAIL.
    let codex_ok = std::process::Command::new("codex")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    if !codex_ok {
        eprintln!("environment: `codex` not found or not runnable on PATH (doctor --live needs the Codex CLI)");
        std::process::exit(2);
    }
    // Resolve the user's actual mode (spec §Mode detection) from the raw
    // config text; a missing or unparseable `~/.codex/config.toml` degrades
    // to `Mode::Fallback`, so the live probe still runs, only with the
    // degraded wiring (see [`detect_active_mode`]).
    let codex_toml = std::fs::read_to_string(default_codex_config_path()).ok();
    let mode = detect_active_mode(codex_toml.as_deref());
    // `main` is `#[tokio::main]`, so the calling thread is already inside a
    // tokio runtime; a nested `Runtime::new()` would panic with "Cannot
    // start a runtime from within a runtime". Run the probe on a fresh OS
    // thread with its own runtime instead — the probe is fully
    // self-contained (own mock server, router, client).
    let probe = std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(probe_both_routes(mode))
    });
    let checks = match probe.join() {
        Ok(Ok(checks)) => checks,
        Ok(Err(e)) => return Err(e),
        Err(_) => anyhow::bail!("doctor --live probe thread panicked"),
    };
    print_report(&checks);
    if report_has_fail(&checks) {
        std::process::exit(1);
    }
    Ok(())
}

/// Resolve the active provider and wiring mode from raw codex config text
/// (injected, so `run()`'s mode resolution stays unit-testable without
/// touching `~/.codex` or spawning codex). The active provider is the
/// top-level `model_provider`, default
/// [`crate::mode::DEFAULT_ACTIVE_PROVIDER`]; missing text and TOML parse
/// failures degrade to `Mode::Fallback` via
/// [`crate::mode::detect_mode`].
fn detect_active_mode(codex_toml: Option<&str>) -> Mode {
    let active_provider = codex_toml
        .and_then(|text| text.parse::<toml::Value>().ok())
        .and_then(|val| {
            val.get("model_provider")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| mode::DEFAULT_ACTIVE_PROVIDER.to_string());
    mode::detect_mode(codex_toml, &active_provider)
}

/// Default Codex config location: `$HOME/.codex/config.toml`. Same home
/// resolution rule as `doctor.rs`: `HOME`, falling back to `USERPROFILE`
/// on Windows.
fn default_codex_config_path() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    PathBuf::from(home).join(".codex").join("config.toml")
}

/// Run both probe routes (responses passthrough + chat conversion)
/// against the mode-appropriate wiring, collecting the shape/round-trip
/// checks plus the codex-side live-catalog-fetch proof for each route.
async fn probe_both_routes(mode: Mode) -> anyhow::Result<Vec<Check>> {
    println!("[1/6] preparing temp workspace");
    let dir = tempfile::tempdir()?;
    let sentinel_dir = dir.path().to_path_buf();

    println!("[2/6] starting mock upstream");
    let mock = MockState {
        responses_requests: Arc::new(Mutex::new(Vec::new())),
        chat_requests: Arc::new(Mutex::new(Vec::new())),
        sentinel_dir: sentinel_dir.clone(),
    };
    let mock_state = mock.clone();
    let mock_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let mock_port = mock_listener.local_addr()?.port();
    let mock_app = Router::new()
        .route("/v1/responses", post(responses_handler))
        .route("/v1/chat/completions", post(chat_handler))
        .with_state(mock_state);
    let mock_task = tokio::spawn(async move {
        axum::serve(mock_listener, mock_app).await.unwrap();
    });

    println!("[3/6] generating temp catalog + config, starting in-process router");
    // Route keys must be `provider/alias` where the prefix names a
    // `[providers.X]` entry (config.rs rule 2); the two routes therefore
    // need two providers — `doctor` (responses) and `doctorchat` (chat).
    // The base_url must include the `/v1` path: the router appends
    // `/responses` / `/chat/completions` to it (upstream.rs).
    // `build_catalog_entry` is the from-scratch path, so each entry already
    // carries the pinned neutral `base_instructions` placeholder (catalog.rs
    // FALLBACK_BASE_INSTRUCTIONS) — codex >= 0.147 rejects models lacking
    // both base_instructions and model_messages.instructions_template. The
    // probe asserts the WIRE SHAPE (tools, item types, model rewrite,
    // round-trip), not the instruction content.
    let catalog = json!({ "models": [
        crate::catalog::build_catalog_entry("doctor/resp", 131072, Some("low")),
    // TODO(issue #3 item 5): free_port() has a TOCTOU race; wrap startup in a
    // retry loop (see tests/integration.rs setup() for the pattern).
    // TODO(issue #3 item 6): pick_tool should prefer exec_command; the first
    // function tool with a synthesizable required property might be something
    // else (e.g. apply_patch), causing the probe to synthesize an unusable call.

        crate::catalog::build_catalog_entry("doctorchat/chat", 131072, Some("low")),
    ]});
    let catalog_path = dir.path().join("catalog.json");
    std::fs::write(&catalog_path, serde_json::to_string_pretty(&catalog)?)?;
    let router_port = free_port();
    let router_config = format!(
        r#"
[server]
host = "127.0.0.1"
port = {router_port}

[providers.doctor]
base_url = "http://127.0.0.1:{mock_port}/v1"
api_key = "test-key"
format = "responses"
timeout_ms = 60000

[providers.doctorchat]
base_url = "http://127.0.0.1:{mock_port}/v1"
api_key = "test-key"
format = "chat"
timeout_ms = 60000

[routes]
"doctor/resp" = {{ model = "doctor-upstream" }}
"doctorchat/chat" = {{ model = "doctor-upstream" }}
"#
    );
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, router_config)?;
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let serve_path = config_path.clone();
    let router_task = tokio::spawn(async move {
        let _ = crate::proxy::serve(&serve_path, async {
            let _ = shutdown_rx.await;
        })
        .await;
    });
    wait_for_healthz(router_port).await?;

    let mut checks = Vec::new();
    for (idx, (route, endpoint)) in [("doctor/resp", "responses"), ("doctorchat/chat", "chat")]
        .into_iter()
        .enumerate()
    {
        println!("[{}/6] probing {route} ({endpoint} upstream)", idx + 4);
        let sentinel = sentinel_dir.join(format!("sentinel-{endpoint}.touched"));
        let _ = std::fs::remove_file(&sentinel);
        let outcome =
            run_codex_probe_with_deadline(mode, route, &catalog_path, router_port, CODEX_PROMPT);
        checks.extend(shape_checks(&mock, endpoint, "doctor-upstream"));
        checks.push(sentinel_check(&sentinel));
        checks.push(exit_check(&outcome));
        checks.push(live_catalog_check(mode, &outcome));
    }

    println!("[6/6] shutting down");
    let _ = shutdown_tx.send(());
    router_task.abort();
    mock_task.abort();
    Ok(checks)
}

/// Prompt that makes codex honor the tool call the mock upstream streams.
const CODEX_PROMPT: &str =
    "Run the tool call exactly as the model output requests, then report the result in one sentence.";

// ---- tool picking / argument synthesis (unit-tested pure functions) ----

/// Pick the first function tool whose schema can carry the sentinel command
/// and synthesize arguments for it. Handles both wire shapes: Responses
/// (`{type, name, parameters}`) and Chat (`{type, function: {name,
/// parameters}}`). Returns `(name, arguments-json)`.
pub fn pick_tool(tools: &[Value], sentinel_cmd: &str) -> Option<(String, String)> {
    for tool in tools {
        let (name, params) = match tool.get("type").and_then(Value::as_str) {
            Some("function") => {
                // Chat shape nests under `function`; Responses is flat.
                let func = tool
                    .get("function")
                    .cloned()
                    .unwrap_or_else(|| tool.clone());
                // Skip-and-continue, not `?`: a malformed function entry
                // (missing name) must not poison the whole search — a later
                // usable tool should still be picked.
                let Some(name) = func.get("name").and_then(Value::as_str) else {
                    continue;
                };
                (
                    name.to_string(),
                    func.get("parameters").cloned().unwrap_or(json!({})),
                )
            }
            _ => continue, // namespace wrappers, web_search, …: skip
        };
        if let Some(args) = synth_args(&params, sentinel_cmd) {
            return Some((name, Value::Object(args).to_string()));
        }
    }
    None
}

/// Synthesize arguments for the schema's REQUIRED properties only:
/// string → sentinel command, number/integer → 1, boolean → false,
/// enum → first value. `None` when nothing is required (nothing could carry
/// the sentinel).
fn synth_args(schema: &Value, sentinel_cmd: &str) -> Option<serde_json::Map<String, Value>> {
    let required = schema.get("required").and_then(Value::as_array)?;
    let props = schema.get("properties").and_then(Value::as_object)?;
    if required.is_empty() {
        return None;
    }
    let mut out = serde_json::Map::new();
    for key in required {
        let key = key.as_str()?;
        let spec = props.get(key)?;
        let value = match spec.get("type").and_then(Value::as_str) {
            Some("number") | Some("integer") => json!(1),
            Some("boolean") => json!(false),
            _ => spec
                .get("enum")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .cloned()
                .unwrap_or_else(|| json!(sentinel_cmd)),
        };
        out.insert(key.to_string(), value);
    }
    Some(out)
}

// ---- mock upstream ----

#[derive(Clone)]
struct MockState {
    responses_requests: Arc<Mutex<Vec<Value>>>,
    chat_requests: Arc<Mutex<Vec<Value>>>,
    /// Per-probe sentinel dir; each endpoint derives its own sentinel path
    /// (`sentinel-<endpoint>.touched`) and builds the `touch` command at
    /// request time, so the tool argument the mock streams always matches
    /// the file the assertions look for.
    sentinel_dir: PathBuf,
}

fn record(store: &Arc<Mutex<Vec<Value>>>, body: &[u8]) -> Option<Value> {
    let parsed: Value = serde_json::from_slice(body).ok()?;
    store.lock().unwrap().push(parsed.clone());
    Some(parsed)
}

async fn responses_handler(
    axum::extract::State(state): axum::extract::State<MockState>,
    body: axum::body::Bytes,
) -> axum::response::Response {
    let Some(req) = record(&state.responses_requests, &body) else {
        return sse_response("event: response.failed\ndata: {}\n\n");
    };
    let round = state.responses_requests.lock().unwrap().len();
    let sse = if round == 1 {
        let sentinel = state.sentinel_dir.join("sentinel-responses.touched");
        let sentinel_cmd = format!("touch {}", sentinel.display());
        let tools = req
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // NOTE: if the router failed to normalize a dialect delivery, there
        // are no top-level tools here and the probe fails at the shape
        // checks; stream a failed response so codex exits fast.
        match pick_tool(&tools, &sentinel_cmd) {
            Some((name, args)) => responses_tool_call_sse(&name, &args),
            None => "event: response.failed\ndata: {}\n\n".to_string(),
        }
    } else {
        responses_text_sse("doctor round-trip complete")
    };
    sse_response(&sse)
}

async fn chat_handler(
    axum::extract::State(state): axum::extract::State<MockState>,
    body: axum::body::Bytes,
) -> axum::response::Response {
    let Some(req) = record(&state.chat_requests, &body) else {
        return sse_response("data: [DONE]\n\n");
    };
    let round = state.chat_requests.lock().unwrap().len();
    let sse = if round == 1 {
        let sentinel = state.sentinel_dir.join("sentinel-chat.touched");
        let sentinel_cmd = format!("touch {}", sentinel.display());
        let tools = req
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        match pick_tool(&tools, &sentinel_cmd) {
            Some((name, args)) => chat_tool_call_sse(&name, &args),
            None => chat_text_sse("no tools visible"),
        }
    } else {
        chat_text_sse("doctor round-trip complete")
    };
    sse_response(&sse)
}

fn sse_response(body: &str) -> axum::response::Response {
    axum::response::Response::builder()
        .header("content-type", "text/event-stream")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap()
}

/// Responses SSE for one complete function_call round-trip.
fn responses_tool_call_sse(name: &str, args: &str) -> String {
    let item = json!({
        "type": "function_call",
        "id": "fc_doctor_1",
        "call_id": "call_doctor_1",
        "name": name,
        "arguments": args,
        "status": "completed"
    });
    format!(
        "event: response.created\ndata: {created}\n\n\
         event: response.output_item.added\ndata: {added}\n\n\
         event: response.function_call_arguments.delta\ndata: {delta}\n\n\
         event: response.output_item.done\ndata: {done}\n\n\
         event: response.completed\ndata: {completed}\n\n",
        created = json!({"type": "response.created", "response": {
            "id": "resp_doctor_1", "status": "in_progress", "model": "doctor-upstream"}}),
        added = json!({"type": "response.output_item.added", "output_index": 0,
            "item": {"type": "function_call", "id": "fc_doctor_1", "call_id": "call_doctor_1",
                     "name": name, "arguments": "", "status": "in_progress"}}),
        delta = json!({"type": "response.function_call_arguments.delta",
            "item_id": "fc_doctor_1", "output_index": 0, "delta": args}),
        done = json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
        completed = json!({"type": "response.completed", "response": {
            "id": "resp_doctor_1", "status": "completed", "model": "doctor-upstream",
            "output": [item],
            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}}),
    )
}

/// Responses SSE for a final assistant text message (round 2).
fn responses_text_sse(text: &str) -> String {
    let item = json!({
        "type": "message", "id": "msg_doctor_1", "role": "assistant", "status": "completed",
        "content": [{"type": "output_text", "text": text}]
    });
    format!(
        "event: response.created\ndata: {created}\n\n\
         event: response.output_item.added\ndata: {added}\n\n\
         event: response.output_text.delta\ndata: {delta}\n\n\
         event: response.output_item.done\ndata: {done}\n\n\
         event: response.completed\ndata: {completed}\n\n",
        created = json!({"type": "response.created", "response": {
            "id": "resp_doctor_1", "status": "in_progress", "model": "doctor-upstream"}}),
        added = json!({"type": "response.output_item.added", "output_index": 0,
            "item": {"type": "message", "id": "msg_doctor_1", "role": "assistant", "status": "in_progress", "content": []}}),
        delta = json!({"type": "response.output_text.delta",
            "item_id": "msg_doctor_1", "output_index": 0, "delta": text}),
        done = json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
        completed = json!({"type": "response.completed", "response": {
            "id": "resp_doctor_1", "status": "completed", "model": "doctor-upstream",
            "output": [item],
            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}}),
    )
}

/// Chat SSE carrying one complete tool call (converted by the router).
fn chat_tool_call_sse(name: &str, args: &str) -> String {
    format!(
        "data: {c1}\n\ndata: {c2}\n\ndata: [DONE]\n\n",
        c1 = json!({"choices": [{"index": 0, "delta": {"role": "assistant",
            "tool_calls": [{"index": 0, "id": "call_doctor_1", "type": "function",
                "function": {"name": name, "arguments": args}}]}, "finish_reason": null}]}),
        c2 = json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}}),
    )
}

/// Chat SSE with a final assistant text message (round 2).
fn chat_text_sse(text: &str) -> String {
    format!(
        "data: {c1}\n\ndata: {c2}\n\ndata: [DONE]\n\n",
        c1 = json!({"choices": [{"index": 0, "delta": {"role": "assistant", "content": text},
            "finish_reason": null}]}),
        c2 = json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}}),
    )
}

// ---- codex probe and assertions ----

struct ProbeOutcome {
    success: bool,
    stderr_tail: String,
    /// Whether codex wrote `models_cache.json` in its probe CODEX_HOME —
    /// the codex-side live-catalog fetch proof (false on timeout, where the
    /// child may still be running and the cache may appear later).
    models_cache_fetched: bool,
    /// Slugs parsed from the probe's `models_cache.json`
    /// ([`read_cached_slugs`]); empty when the cache is absent or
    /// unparseable. The dynamic live-catalog check requires the two probe
    /// route slugs to be present here.
    cached_slugs: Vec<String>,
}

/// Run one `codex exec` probe on a blocking thread with a 90s deadline
/// (codex startup + two rounds); a timeout yields a failed outcome instead
/// of hanging the report. `mode` selects the probe's auth/catalog wiring
/// (spec 2026-08-23 §Mode detection).
///
/// DELIBERATE DEVIATION from spec §3.2 ("kill the process group on
/// timeout"): on a 90s timeout the spawned `codex exec` process is NOT
/// killed, so a straggler child may outlive `doctor --live`. The timeout
/// path also intentionally leaves that probe's CODEX_HOME in place — the
/// child may still be running and needs the directory to exist.
fn run_codex_probe_with_deadline(
    mode: Mode,
    route: &str,
    catalog_path: &Path,
    router_port: u16,
    prompt: &'static str,
) -> ProbeOutcome {
    let (tx, rx) = std::sync::mpsc::channel();
    let route = route.to_string();
    let catalog_path = catalog_path.to_path_buf();
    std::thread::spawn(move || {
        let _ = tx.send(run_codex_probe(
            mode,
            &route,
            &catalog_path,
            router_port,
            prompt,
        ));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(90)) {
        Ok(outcome) => outcome,
        Err(_) => ProbeOutcome {
            success: false,
            stderr_tail: "codex probe timed out after 90s".into(),
            models_cache_fetched: false,
            cached_slugs: Vec::new(),
        },
    }
}

/// Run one `codex exec` probe against the in-process router with
/// mode-appropriate wiring: dynamic uses an `auth.command` (codex fetches
/// `/v1/models` live, no pin), pinned uses env_key + the generated catalog
/// pin, fallback uses env_key only (no pin — degraded metadata path).
/// Everything else is identical across modes: base_url, wire_api=responses,
/// approval_policy=never, sandbox_mode=danger-full-access,
/// model_reasoning_effort=low, `-m route`, prompt, scratch CODEX_HOME and
/// the `DOCTOR_CODEX_KEY=dummy` env (the router does not authenticate
/// clients; the dummy key satisfies codex's env_key auth). The returned
/// outcome also carries the live-fetch proof (cache existence + slug set)
/// captured before CODEX_HOME is reclaimed.
fn run_codex_probe(
    mode: Mode,
    route: &str,
    catalog_path: &Path,
    router_port: u16,
    prompt: &str,
) -> ProbeOutcome {
    let home = std::env::temp_dir().join(format!("doctor-home-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&home).unwrap();
    // The live-fetch proof artifact is captured before CODEX_HOME is
    // reclaimed (see the capture below), so the returned outcome records
    // both the cache presence and the slug set.
    let mut cmd = std::process::Command::new("codex");
    cmd.arg("exec")
        .arg("--skip-git-repo-check")
        .arg("-c")
        .arg("model_provider=doctor")
        .arg("-c")
        .arg("model_providers.doctor.name=doctor")
        .arg("-c")
        .arg(format!(
            "model_providers.doctor.base_url=http://127.0.0.1:{router_port}/v1"
        ))
        .arg("-c")
        .arg("model_providers.doctor.wire_api=responses");
    // Mode-aware auth/catalog wiring (spec §Mode detection): dynamic uses
    // an auth command so codex's OpenAiModelsManager fetches the live
    // catalog (no pin); pinned couples env_key with the generated temp
    // catalog pin (StaticModelsManager); fallback is env_key-only (no pin —
    // degraded metadata path, never fetches).
    match mode {
        Mode::Dynamic => {
            cmd.arg("-c")
                .arg("model_providers.doctor.auth={command=\"echo\",args=[\"dummy\"]}");
        }
        Mode::Pinned => {
            cmd.arg("-c")
                .arg("model_providers.doctor.env_key=DOCTOR_CODEX_KEY");
            cmd.arg("-c")
                .arg(format!("model_catalog_json={}", catalog_path.display()));
        }
        Mode::Fallback => {
            cmd.arg("-c")
                .arg("model_providers.doctor.env_key=DOCTOR_CODEX_KEY");
        }
    }
    cmd.arg("-c")
        .arg("approval_policy=never")
        .arg("-c")
        .arg("sandbox_mode=danger-full-access")
        .arg("-c")
        .arg("model_reasoning_effort=low")
        .arg("-m")
        .arg(route)
        .arg(prompt)
        .env("CODEX_HOME", &home)
        .env("DOCTOR_CODEX_KEY", "dummy")
        .stdin(std::process::Stdio::null());
    let output = cmd.output();
    // This function only runs on the non-timeout path (a 90s deadline
    // timeout returns from `run_codex_probe_with_deadline` without
    // consuming this result), so codex has fully exited here and CODEX_HOME
    // is no longer in use — reclaim the per-probe home. The timeout path
    // intentionally leaves the dir behind: codex may still be running and
    // needs its CODEX_HOME to exist.
    // Record the live-catalog fetch proof BEFORE reclaiming the home:
    // codex writes `models_cache.json` only after a successful `/v1/models`
    // fetch, so its presence is the codex-side equivalent of the spec's
    // router-log `codex client X detected` assertion (the in-process router
    // installs no tracing subscriber, so no router log exists to assert).
    // The slug set is captured too, mirroring
    // `scripts/e2e-lib.sh::assert_live_catalog_fetched`: the dynamic check
    // only passes when the cached catalog actually contains the probe's
    // two route slugs.
    let cache_path = home.join("models_cache.json");
    let models_cache_fetched = cache_path.exists();
    let cached_slugs = read_cached_slugs(&cache_path);
    let _ = std::fs::remove_dir_all(&home);
    match output {
        Ok(out) => ProbeOutcome {
            success: out.status.success(),
            stderr_tail: tail(&String::from_utf8_lossy(&out.stderr), 1200),
            models_cache_fetched,
            cached_slugs,
        },
        Err(e) => ProbeOutcome {
            success: false,
            stderr_tail: format!("failed to spawn codex: {e}"),
            models_cache_fetched,
            cached_slugs,
        },
    }
}

/// The two route slugs every probe run expects from the in-process router's
/// `/v1/models`; the dynamic live-catalog check requires both in codex's
/// cached catalog (mirror of
/// `scripts/e2e-lib.sh::assert_live_catalog_fetched`).
const PROBE_CATALOG_SLUGS: [&str; 2] = ["doctor/resp", "doctorchat/chat"];

/// Codex-side live-catalog fetch proof for one probe (spec L3 assertion,
/// adapted to this in-process topology). Dynamic mode MUST fetch the live
/// catalog AND see both probe route slugs in it — a missing cache means the
/// `/v1/models` fetch never happened, a cache missing a slug means it
/// returned a truncated/incorrect catalog. Pinned/fallback modes must NOT
/// fetch — a present cache means codex fetched despite the env_key-only/
/// pinned wiring (an anomaly; inspect the user's config for a stray
/// `auth.command`).
fn live_catalog_check(mode: Mode, outcome: &ProbeOutcome) -> Check {
    match mode {
        Mode::Dynamic => {
            if !outcome.models_cache_fetched {
                Check::fail(
                    "live catalog fetched",
                    "the dynamic-mode /v1/models fetch is broken — codex never wrote \
                     its models_cache.json; check models_cache.rs::CatalogCache::get \
                     / the router",
                )
            } else {
                let missing: Vec<&str> = PROBE_CATALOG_SLUGS
                    .iter()
                    .copied()
                    .filter(|slug| !outcome.cached_slugs.iter().any(|s| s == slug))
                    .collect();
                if missing.is_empty() {
                    Check::pass(
                        "live catalog fetched",
                        format!(
                            "codex wrote its models_cache.json with both probe slugs ({})",
                            PROBE_CATALOG_SLUGS.join(", ")
                        ),
                    )
                } else {
                    Check::fail(
                        "live catalog fetched",
                        format!(
                            "dynamic-mode /v1/models fetch returned a catalog missing \
                             slug(s) {} — check models_cache.rs::CatalogCache::get \
                             / the router",
                            missing.join(", ")
                        ),
                    )
                }
            }
        }
        Mode::Pinned | Mode::Fallback => {
            if outcome.models_cache_fetched {
                Check::fail(
                    "no live catalog fetch (pinned/fallback wiring)",
                    "codex fetched /v1/models despite the wiring — inspect \
                     the config for a stray auth.command",
                )
            } else {
                Check::pass(
                    "no live catalog fetch (pinned/fallback wiring)",
                    "codex never fetched /v1/models (intended)",
                )
            }
        }
    }
}

/// Parse the slugs from a codex `models_cache.json` (empty when the file is
/// absent or unparseable). Only the `models[].slug` strings are needed —
/// the value-level mirror of `scripts/e2e-lib.sh::assert_live_catalog_fetched`,
/// which checks the same slugs at the e2e layer.
fn read_cached_slugs(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(val) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    val.get("models")
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter_map(|m| m.get("slug").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Last `n` characters of `s` (diagnostic tails keep the end, where the
/// error usually is).
fn tail(s: &str, n: usize) -> String {
    s.chars()
        .rev()
        .take(n)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

/// Compact one-line summary of a captured request for FAIL diagnostics:
/// tools count, item/role types, and the model. Kept short (a few fields,
/// not the whole body) — it is the highest-value signal for the
/// DSML-leak class: empty tools + leaked `additional_tools`/unknown item
/// types are exactly what this prints.
fn request_summary(req: &Value) -> String {
    let tools = req
        .get("tools")
        .and_then(Value::as_array)
        .map(|t| t.len())
        .unwrap_or(0);
    // Responses requests carry `input` items with a `type`; chat requests
    // carry `messages` with a `role` — cover both shapes.
    let kinds: Vec<String> = if let Some(input) = req.get("input").and_then(Value::as_array) {
        input
            .iter()
            .filter_map(|it| it.get("type").and_then(Value::as_str))
            .map(String::from)
            .collect()
    } else if let Some(messages) = req.get("messages").and_then(Value::as_array) {
        messages
            .iter()
            .filter_map(|m| m.get("role").and_then(Value::as_str))
            .map(String::from)
            .collect()
    } else {
        Vec::new()
    };
    let model = req.get("model").and_then(Value::as_str).unwrap_or("?");
    format!(
        "tools: {tools}, item types: [{}], model: {model}",
        kinds.join(", ")
    )
}

fn shape_checks(mock: &MockState, endpoint: &str, upstream_model: &str) -> Vec<Check> {
    let (store, label) = match endpoint {
        "responses" => (&mock.responses_requests, "top-level tools"),
        _ => (&mock.chat_requests, "chat function tools"),
    };
    let requests = store.lock().unwrap();
    let Some(req) = requests.first() else {
        return vec![Check::fail(
            &format!("{endpoint}: upstream received a request"),
            "mock upstream recorded nothing",
        )];
    };
    let mut checks = vec![Check::pass(
        &format!("{endpoint}: upstream received a request"),
        format!("{} round(s) recorded", requests.len()),
    )];

    // Tools visible to the upstream in that format's shape.
    let tools_ok = match endpoint {
        "responses" => req.get("tools").and_then(Value::as_array).is_some_and(|t| {
            t.iter()
                .any(|tool| tool.get("type").and_then(Value::as_str) == Some("function"))
        }),
        _ => req
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|t| t.iter().any(|tool| tool.get("function").is_some())),
    };
    checks.push(if tools_ok {
        Check::pass(
            &format!("{endpoint}: {label} non-empty"),
            "at least one function tool",
        )
    } else {
        Check::fail(
            &format!("{endpoint}: {label} non-empty"),
            format!(
                "no function tools reached the upstream — dialect delivery not normalized ({})",
                request_summary(req)
            ),
        )
    });

    if endpoint == "responses" {
        let input = req
            .get("input")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let has_additional = input
            .iter()
            .any(|it| it.get("type").and_then(Value::as_str) == Some("additional_tools"));
        checks.push(if has_additional {
            Check::fail(
                &format!("{endpoint}: no additional_tools input items"),
                "additional_tools item leaked through the router",
            )
        } else {
            Check::pass(&format!("{endpoint}: no additional_tools input items"), "")
        });
        let unknown: Vec<String> = input
            .iter()
            .filter_map(|it| it.get("type").and_then(Value::as_str))
            .filter(|t| !KNOWN_INPUT_ITEM_TYPES.contains(t))
            .map(String::from)
            .collect();
        checks.push(if unknown.is_empty() {
            Check::pass(&format!("{endpoint}: input item types known"), "")
        } else {
            Check::fail(
                &format!("{endpoint}: input item types known"),
                format!(
                    "unknown item type(s): {} — review before allowing",
                    unknown.join(", ")
                ),
            )
        });
    }

    checks.push(
        if req.get("model").and_then(Value::as_str) == Some(upstream_model) {
            Check::pass(&format!("{endpoint}: model rewritten"), "")
        } else {
            Check::fail(
                &format!("{endpoint}: model rewritten"),
                format!(
                    "expected {upstream_model}, got {}",
                    req.get("model").unwrap_or(&json!("null"))
                ),
            )
        },
    );
    checks
}

fn sentinel_check(sentinel: &Path) -> Check {
    if sentinel.exists() {
        Check::pass("tool round-trip executed", sentinel.display().to_string())
    } else {
        Check::fail(
            "tool round-trip executed",
            format!(
                "sentinel {} missing — codex did not execute the tool call",
                sentinel.display()
            ),
        )
    }
}

fn exit_check(outcome: &ProbeOutcome) -> Check {
    // No embedded step bracket in the NAME: print_report renumbers the whole
    // report ([1/N]..[N/N]), so a "[4/6]" prefix here would read as a
    // duplicate. The progress println's [4/6]/[5/6] is the only step
    // numbering.
    let name = "codex exited cleanly".to_string();
    if outcome.success {
        Check::pass(&name, "")
    } else {
        Check::fail(&name, outcome.stderr_tail.clone())
    }
}

async fn wait_for_healthz(port: u16) -> anyhow::Result<()> {
    for _ in 0..100 {
        if let Ok(resp) = reqwest::get(format!("http://127.0.0.1:{port}/healthz")).await {
            if resp.status().is_success() {
                return Ok(());
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    anyhow::bail!("router did not become healthy on port {port} within 10s")
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- mode resolution (pure wrapper around detec_mode) ----

    #[test]
    fn detect_active_mode_missing_or_unparseable_is_fallback() {
        // `run()` feeds the raw file text through this wrapper; a missing
        // or unparseable `~/.codex/config.toml` must degrade to Fallback
        // (consistent with `crate::mode::detect_mode`) so the live probe
        // still runs with the degraded wiring.
        assert_eq!(detect_active_mode(None), Mode::Fallback);
        assert_eq!(
            detect_active_mode(Some("not valid toml [[[")),
            Mode::Fallback
        );
    }

    #[test]
    fn detect_active_mode_follows_model_provider_selection() {
        let toml = r#"
            model = "x/y"
            model_provider = "alt"
            [model_providers.codexferry]
            base_url = "http://127.0.0.1:8787/v1"
            env_key = "DUMMY"
            [model_providers.alt]
            base_url = "http://127.0.0.1:8787/v1"
            [model_providers.alt.auth]
            command = "echo"
            args = ["dummy"]
        "#;
        // `alt` has auth.command; `codexferry` is env_key-only. The active
        // provider (top-level `model_provider`) must be honored, so this is
        // Dynamic — not the Fallback the default provider would give.
        assert_eq!(detect_active_mode(Some(toml)), Mode::Dynamic);
    }

    #[test]
    fn detect_active_mode_defaults_to_codexferry() {
        // No top-level `model_provider`: the DEFAULT_ACTIVE_PROVIDER table
        // decides, and env_key-only wiring is Fallback.
        let fallback = r#"
            model = "x/y"
            [model_providers.codexferry]
            base_url = "http://127.0.0.1:8787/v1"
            env_key = "DUMMY"
        "#;
        assert_eq!(detect_active_mode(Some(fallback)), Mode::Fallback);
        // The same default provider with a pin is Pinned.
        let pinned = r#"
            model_catalog_json = "/tmp/catalog.json"
            [model_providers.codexferry]
            base_url = "http://127.0.0.1:8787/v1"
        "#;
        assert_eq!(detect_active_mode(Some(pinned)), Mode::Pinned);
    }

    // ---- live-catalog check (mode-aware L3 assertion) ----

    /// Probe outcome with only the live-catalog parts set (`success` and
    /// `stderr_tail` are irrelevant to `live_catalog_check`).
    fn outcome_with_cache(cache: bool, slugs: &[&str]) -> ProbeOutcome {
        ProbeOutcome {
            success: true,
            stderr_tail: String::new(),
            models_cache_fetched: cache,
            cached_slugs: slugs.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn dynamic_with_fresh_cache_and_both_slugs_passes() {
        let check = live_catalog_check(
            Mode::Dynamic,
            &outcome_with_cache(true, &["doctor/resp", "doctorchat/chat"]),
        );
        assert_eq!(check.status, crate::doctor::CheckStatus::Pass);
        assert_eq!(check.name, "live catalog fetched");
        assert_eq!(
            check.detail,
            "codex wrote its models_cache.json with both probe slugs (doctor/resp, doctorchat/chat)"
        );
    }

    #[test]
    fn dynamic_without_cache_fails_naming_catalog_cache() {
        let check = live_catalog_check(Mode::Dynamic, &outcome_with_cache(false, &[]));
        assert_eq!(check.status, crate::doctor::CheckStatus::Fail);
        assert_eq!(check.name, "live catalog fetched");
        assert!(
            check.detail.contains("CatalogCache"),
            "detail should name the catalog-cache remediation: {}",
            check.detail
        );
        assert!(
            check.detail.contains("never wrote"),
            "detail should distinguish missing cache from missing slug: {}",
            check.detail
        );
    }

    #[test]
    fn dynamic_cache_missing_a_probe_slug_fails() {
        // A cache that exists but lacks one (or both) probe slugs must FAIL
        // with the missing-slug wording — the truncated-catalog case.
        for slugs in [
            &["doctor/resp"][..],
            &["doctorchat/chat"][..],
            &["unrelated/route"][..],
        ] {
            let check = live_catalog_check(Mode::Dynamic, &outcome_with_cache(true, slugs));
            assert_eq!(check.status, crate::doctor::CheckStatus::Fail);
            assert_eq!(check.name, "live catalog fetched");
            assert!(
                check.detail.contains("missing slug(s)"),
                "detail should name the missing slug: {}",
                check.detail
            );
        }
    }

    #[test]
    fn pinned_or_fallback_with_cache_fails_on_stray_auth_command() {
        for mode in [Mode::Pinned, Mode::Fallback] {
            let check = live_catalog_check(
                mode,
                &outcome_with_cache(true, &["doctor/resp", "doctorchat/chat"]),
            );
            assert_eq!(check.status, crate::doctor::CheckStatus::Fail);
            assert_eq!(check.name, "no live catalog fetch (pinned/fallback wiring)");
            assert!(
                check.detail.contains("stray auth.command"),
                "detail should point at the stray auth.command: {}",
                check.detail
            );
        }
    }

    #[test]
    fn pinned_or_fallback_without_cache_passes() {
        for mode in [Mode::Pinned, Mode::Fallback] {
            let check = live_catalog_check(mode, &outcome_with_cache(false, &[]));
            assert_eq!(check.status, crate::doctor::CheckStatus::Pass);
            assert_eq!(check.name, "no live catalog fetch (pinned/fallback wiring)");
        }
    }

    #[test]
    fn timeout_outcome_is_treated_as_no_cache() {
        // The 90s deadline path records no cache proof (the child may still
        // be running and write the cache later): dynamic must FAIL (fetch
        // unknown), pinned/fallback must PASS (no fetch observed).
        let timed_out = ProbeOutcome {
            success: false,
            stderr_tail: "codex probe timed out after 90s".into(),
            models_cache_fetched: false,
            cached_slugs: Vec::new(),
        };
        assert_eq!(
            live_catalog_check(Mode::Dynamic, &timed_out).status,
            crate::doctor::CheckStatus::Fail
        );
        assert_eq!(
            live_catalog_check(Mode::Pinned, &timed_out).status,
            crate::doctor::CheckStatus::Pass
        );
        assert_eq!(
            live_catalog_check(Mode::Fallback, &timed_out).status,
            crate::doctor::CheckStatus::Pass
        );
    }

    // ---- models_cache.json slug parsing ----

    #[test]
    fn read_cached_slugs_parses_models_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models_cache.json");
        std::fs::write(
            &path,
            json!({"models": [
                {"slug": "doctor/resp"},
                {"slug": "doctorchat/chat"},
                {"slug": "other/x"},
                {"display_name": "no slug"}
            ]})
            .to_string(),
        )
        .unwrap();
        assert_eq!(
            read_cached_slugs(&path),
            vec![
                "doctor/resp".to_string(),
                "doctorchat/chat".to_string(),
                "other/x".to_string()
            ]
        );
    }

    #[test]
    fn read_cached_slugs_is_empty_for_missing_or_bad_cache() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("models_cache.json");
        assert!(read_cached_slugs(&missing).is_empty());

        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "not json").unwrap();
        assert!(read_cached_slugs(&bad).is_empty());

        let wrong_shape = dir.path().join("shape.json");
        std::fs::write(&wrong_shape, json!({"models": "not-an-array"}).to_string()).unwrap();
        assert!(read_cached_slugs(&wrong_shape).is_empty());
    }

    #[test]
    fn picks_first_function_tool_with_required_string() {
        let tools = vec![
            json!({"type": "namespace", "name": "functions", "tools": []}),
            json!({
                "type": "function", "name": "exec_command",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "cmd": {"type": "string"},
                        "timeout_ms": {"type": "number"},
                        "tty": {"type": "boolean"},
                        "mode": {"type": "string", "enum": ["a", "b"]},
                    },
                    "required": ["cmd"]
                }
            }),
        ];
        let (name, args) = pick_tool(&tools, "touch /tmp/x").expect("usable tool");
        assert_eq!(name, "exec_command");
        let args: serde_json::Value = serde_json::from_str(&args).unwrap();
        // Only REQUIRED properties are synthesized; string → sentinel,
        // number → 1, boolean → false, enum → first value.
        assert_eq!(args, json!({"cmd": "touch /tmp/x"}));
    }

    #[test]
    fn picks_chat_shaped_function_tool() {
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "exec_command",
                "parameters": {
                    "type": "object",
                    "properties": {"cmd": {"type": "string"}},
                    "required": ["cmd"]
                }
            }
        })];
        let (name, _) = pick_tool(&tools, "touch /tmp/x").expect("usable tool");
        assert_eq!(name, "exec_command");
    }

    #[test]
    fn no_function_tools_yields_none() {
        let tools = vec![
            json!({"type": "namespace", "name": "functions", "tools": []}),
            json!({"type": "web_search"}),
        ];
        assert!(pick_tool(&tools, "touch /tmp/x").is_none());
    }

    #[test]
    fn no_required_properties_yields_none() {
        // A function tool whose schema has no required props cannot carry
        // the sentinel command; skip it.
        let tools = vec![json!({
            "type": "function", "name": "noop",
            "parameters": {"type": "object", "properties": {"x": {"type": "string"}}, "required": []}
        })];
        assert!(pick_tool(&tools, "touch /tmp/x").is_none());
    }

    #[test]
    fn synthesizes_number_boolean_and_enum_required_properties() {
        let tools = vec![json!({
            "type": "function", "name": "exec_command",
            "parameters": {
                "type": "object",
                "properties": {
                    "cmd": {"type": "string"},
                    "num": {"type": "number"},
                    "flag": {"type": "boolean"},
                    "mode": {"type": "string", "enum": ["a", "b"]},
                },
                "required": ["cmd", "num", "flag", "mode"]
            }
        })];
        let (_, args) = pick_tool(&tools, "touch /tmp/x").expect("usable tool");
        let args: serde_json::Value = serde_json::from_str(&args).unwrap();
        // string → sentinel, number/integer → 1, boolean → false,
        // enum → first value.
        assert_eq!(
            args,
            json!({
                "cmd": "touch /tmp/x",
                "num": 1,
                "flag": false,
                "mode": "a"
            })
        );
    }

    #[test]
    fn malformed_function_tool_is_skipped_not_poisoning() {
        // A `type: "function"` entry with a missing name must be skipped,
        // not abort the whole search (regression: `?` used to propagate
        // None out of pick_tool, hiding the valid tool that follows).
        let tools = vec![
            json!({"type": "function", "parameters": {
                "type": "object", "properties": {}, "required": []}}),
            json!({
                "type": "function", "name": "exec_command",
                "parameters": {
                    "type": "object",
                    "properties": {"cmd": {"type": "string"}},
                    "required": ["cmd"]
                }
            }),
        ];
        let (name, _) = pick_tool(&tools, "touch /tmp/x").expect("usable tool");
        assert_eq!(name, "exec_command");
    }
}
