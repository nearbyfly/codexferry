//! Scripted mock upstream for the E2E scripts (spec 2026-08-21-e2e-testing).
//!
//! One process serves ONE scenario (deterministic per-turn responses) and
//! appends every received request to a JSONL record the scripts assert
//! against. Same shape as the mocks in `tests/common/mod.rs`, but standalone
//! and stateful across turns.

use axum::{
    extract::State,
    response::{
        sse::{Event, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Router,
};
use clap::Parser;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Parser)]
#[command(about = "Scripted mock upstream for codexferry E2E scripts")]
struct Args {
    /// Port to listen on.
    #[arg(long)]
    port: u16,
    /// Scenario to serve: basic | basic-responses | tools | multiturn.
    #[arg(long)]
    scenario: String,
    /// JSONL file every received request is appended to.
    #[arg(long)]
    record: PathBuf,
}

#[derive(Clone)]
struct MockState {
    scenario: Arc<str>,
    turn: Arc<AtomicUsize>,
    record_path: PathBuf,
}

/// Append one request to the JSONL record (best-effort: unparseable bodies
/// are recorded as null rather than dropping the entry; open/write failures
/// are printed to stderr so script failures are debuggable, but the mock
/// keeps serving).
fn record(state: &MockState, path: &str, auth: &str, body: &[u8]) {
    let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let entry = json!({"path": path, "auth": auth, "body": parsed});
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&state.record_path)
    {
        Ok(mut f) => {
            if let Err(err) = writeln!(f, "{entry}") {
                eprintln!(
                    "e2e-mock: failed to append record '{}': {err}",
                    state.record_path.display()
                );
            }
        }
        Err(err) => eprintln!(
            "e2e-mock: failed to open record '{}': {err}",
            state.record_path.display()
        ),
    }
}

fn usage_chunk() -> &'static str {
    r#"{"choices":[],"usage":{"prompt_tokens":9,"completion_tokens":5,"total_tokens":14}}"#
}

/// One plain text turn: content delta → finish stop → usage → [DONE].
fn basic_chat_chunks(text: &str) -> Vec<String> {
    vec![
        json!({"choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}]}).to_string(),
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#.to_string(),
        usage_chunk().to_string(),
        "[DONE]".to_string(),
    ]
}

fn multiturn_marker(turn: usize) -> String {
    format!("E2E_TURN_{turn}_OK")
}

/// Granular events for the responses-format scenario: created →
/// output_item.added → output_text.delta → output_item.done → completed.
/// Codex CLI 0.147 renders text only from granular deltas (a bare
/// created+completed pair prints nothing — the reason for this drift fix,
/// spec 2026-08-21). Event names follow the real OpenAI Responses wire format
/// (AGENTS.md §8a) but INTENTIONALLY omit the part-level events
/// (`response.content_part.added/done`, `response.output_text.done`): the CLI
/// renders text from this shorter sequence, and expanding to the full
/// canonical event set would change the behavior this e2e asserts on.
fn responses_events(text: &str) -> Vec<(String, String)> {
    let item_id = "msg_e2e_1";
    let message_item = |status: &str, content: Vec<Value>| {
        json!({
            "type": "message",
            "id": item_id,
            "role": "assistant",
            "status": status,
            "content": content
        })
    };
    let created = json!({
        "type": "response.created",
        "response": {"id": "resp_e2e_1", "status": "in_progress", "model": "e2e-model"}
    })
    .to_string();
    let added = json!({
        "type": "response.output_item.added",
        "output_index": 0,
        "item": message_item("in_progress", Vec::new())
    })
    .to_string();
    let delta = json!({
        "type": "response.output_text.delta",
        "item_id": item_id,
        "output_index": 0,
        "delta": text
    })
    .to_string();
    let done = json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": message_item("completed", vec![json!({"type": "output_text", "text": text})])
    })
    .to_string();
    let completed = json!({
        "type": "response.completed",
        "response": {
            "id": "resp_e2e_1",
            "status": "completed",
            "model": "e2e-model",
            "output": [message_item("completed", vec![json!({"type": "output_text", "text": text})])],
            "usage": {"input_tokens": 4, "output_tokens": 3, "total_tokens": 7}
        }
    })
    .to_string();
    vec![
        ("response.created", created),
        ("response.output_item.added", added),
        ("response.output_text.delta", delta),
        ("response.output_item.done", done),
        ("response.completed", completed),
    ]
    .into_iter()
    .map(|(name, data)| (name.to_string(), data))
    .collect()
}

fn sse_response(chunks: Vec<String>) -> Response {
    let stream = futures_util::stream::iter(
        chunks
            .into_iter()
            .map(|c| Ok::<Event, Infallible>(Event::default().data(c))),
    );
    Sse::new(stream).into_response()
}

/// Turn-1 tool-call chunks. The tool NAME is echoed from the request's own
/// tools array (chat-converted OpenAI shape: tools[0].function.name, with a
/// tools[0].name fallback) so the fixture survives Codex renaming its tools
/// across versions. The arguments are calibrated to the schema Codex CLI
/// 0.147 actually advertises (record's tools[0].function.parameters): the
/// shell tool is `exec_command` with a required string `cmd`. An earlier
/// `{"command": [...]}` array shape was rejected by the CLI at parse time
/// ("missing field `cmd`"), so the fixture now emits the string form and the
/// CLI runs `echo E2E_TOOL_OK` (the tools scenario runs with
/// `--dangerously-bypass-approvals-and-sandbox` unconditionally, via the
/// `E2E_CODEX_SANDBOX=bypass` toggle in `scripts/e2e-lib.sh` set by
/// `scripts/e2e.sh`: bubblewrap needs user namespaces, which externally
/// sandboxed containers cannot create, so a normal sandbox makes even
/// `echo` fail).
fn tools_turn1_chunks(tool: Value) -> Vec<String> {
    let name = tool["function"]["name"]
        .as_str()
        .or_else(|| tool["name"].as_str())
        .unwrap_or("exec_command");
    let args = json!({"cmd": "echo E2E_TOOL_OK"}).to_string();
    vec![
        json!({"choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}).to_string(),
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_e2e_1","type":"function","function":{"name":name,"arguments":args}}]},"finish_reason":null}]}).to_string(),
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#.to_string(),
        "[DONE]".to_string(),
    ]
}

/// Scripted tools scenario: turn 1 emits a tool-call stream whose name is
/// echoed from the request's own tools array; turn >=2 emits the convergence
/// text marker E2E_TOOL_DONE (after the CLI replays the function_call_output).
fn tools_chat_response(turn: usize, body: &[u8]) -> Response {
    if turn == 1 {
        let req: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        sse_response(tools_turn1_chunks(req["tools"][0].clone()))
    } else {
        sse_response(basic_chat_chunks("E2E_TOOL_DONE"))
    }
}

async fn chat_handler(
    State(state): State<MockState>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    record(
        &state,
        "/v1/chat/completions",
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        &body,
    );
    let turn = state.turn.fetch_add(1, Ordering::SeqCst) + 1;
    match state.scenario.as_ref() {
        "basic" => sse_response(basic_chat_chunks("E2E_BASIC_OK")),
        "multiturn" => sse_response(basic_chat_chunks(&multiturn_marker(turn))),
        "tools" => crate::tools_chat_response(turn, &body),
        other => (
            axum::http::StatusCode::BAD_REQUEST,
            format!("scenario '{other}' does not serve the chat path"),
        )
            .into_response(),
    }
}

/// Probe-only handler for `GET /v1/models`. Records the request so the
/// static-mode scenario can assert "codex did NOT fetch the live catalog"
/// (env_key providers never fetch, so under static wiring the JSONL record
/// must contain zero `/v1/models` entries). Returns 200 with an empty
/// `ModelsResponse`; live-mode tests do not depend on this endpoint.
async fn models_probe(
    State(state): State<MockState>,
    headers: axum::http::HeaderMap,
) -> Response {
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    record(&state, "/v1/models", &auth, b"");
    (
        axum::http::StatusCode::OK,
        [("content-type", "application/json")],
        json!({"models": []}).to_string(),
    )
        .into_response()
}

async fn responses_handler(
    State(state): State<MockState>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    record(
        &state,
        "/v1/responses",
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        &body,
    );
    match state.scenario.as_ref() {
        // "basic" serves both paths: Task 4's scenario_basic reuses ONE basic
        // mock instance for the chat route (mocka/chat) and the responses
        // route (mockr/resp). "basic-responses" is the responses-only scenario.
        "basic" | "basic-responses" => {
            let events = responses_events("E2E_RESP_OK")
                .into_iter()
                .map(|(event, data)| {
                    Ok::<Event, Infallible>(Event::default().event(event).data(data))
                });
            Sse::new(futures_util::stream::iter(events)).into_response()
        }
        other => (
            axum::http::StatusCode::BAD_REQUEST,
            format!("scenario '{other}' does not serve the responses path"),
        )
            .into_response(),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let state = MockState {
        scenario: args.scenario.into(),
        turn: Arc::new(AtomicUsize::new(0)),
        record_path: args.record,
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(chat_handler))
        .route("/v1/responses", post(responses_handler))
        // `/v1/models` is a probe endpoint only - the mock records the hit so
        // e2e can assert "codex did NOT fetch the live catalog" under static
        // wiring (env_key providers never fetch; the static e2e scenario
        // relies on the absence of any such record entry).
        .route("/v1/models", get(models_probe))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", args.port)).await?;
    eprintln!(
        "e2e-mock listening on 127.0.0.1:{} (scenario from args)",
        args.port
    );
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_chunks_carry_marker_usage_done() {
        let chunks = basic_chat_chunks("E2E_BASIC_OK");
        assert!(chunks[0].contains("E2E_BASIC_OK"));
        assert!(chunks
            .iter()
            .any(|c| c.contains("\"finish_reason\":\"stop\"")));
        assert!(chunks.iter().any(|c| c.contains("\"usage\"")));
        assert_eq!(*chunks.last().unwrap(), "[DONE]".to_string());
    }

    #[test]
    fn multiturn_marker_embeds_turn_number() {
        assert!(basic_chat_chunks(&multiturn_marker(2))[0].contains("E2E_TURN_2_OK"));
    }

    #[test]
    fn responses_events_have_real_event_names() {
        let events = responses_events("E2E_RESP_OK");
        let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "response.created",
                "response.output_item.added",
                "response.output_text.delta",
                "response.output_item.done",
                "response.completed"
            ]
        );
        assert!(events.iter().any(
            |(name, data)| name == "response.output_text.delta" && data.contains("E2E_RESP_OK")
        ));
        assert!(events.last().unwrap().1.contains("E2E_RESP_OK"));
        assert!(events.last().unwrap().1.contains("\"usage\""));
    }

    #[test]
    fn tools_turn1_echoes_request_tool_name_with_shell_args() {
        let req = json!({"tools":[{"type":"function","function":{"name":"shell","description":"d"}}],
                         "messages":[]});
        let chunks = tools_turn1_chunks(req["tools"][0].clone());
        assert!(chunks.iter().any(|c| c.contains("\"name\":\"shell\"")));
        // Calibrated args (spec 2026-08-21): the CLI requires `cmd` as a
        // string; the old {"command": [...]} array shape failed at parse.
        // Wire shape: function arguments ride as a JSON-encoded STRING, so
        // the chunk text contains backslash-escaped quotes around cmd.
        assert!(chunks
            .iter()
            .any(|c| c.contains("\\\"cmd\\\":\\\"echo E2E_TOOL_OK\\\"")));
        assert!(chunks.iter().any(|c| c.contains("E2E_TOOL_OK")));
        assert!(chunks
            .iter()
            .any(|c| c.contains("\"finish_reason\":\"tool_calls\"")));
    }

    #[test]
    fn tools_turn2_converges_with_done_marker() {
        let chunks = basic_chat_chunks("E2E_TOOL_DONE");
        assert!(chunks[0].contains("E2E_TOOL_DONE"));
    }
}
