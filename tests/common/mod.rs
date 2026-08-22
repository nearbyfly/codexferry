//! End-to-end integration test harness (module-split refactor).
//!
//! Shared harness extracted from `integration.rs`: MockState, all mock handlers,
//! spawn/start helpers, fixture constants, and utility functions.
//!
//!
//! Why these tests exist: unit tests prove each conversion stage in
//! isolation, but only an end-to-end test can verify the whole pipeline the
//! way Codex CLI experiences it — routing by `provider/alias`, Responses →
//! Chat request conversion, the upstream HTTP call, SSE re-encoding, and
//! session merge on `previous_response_id`. Spec §14 ("integration testing") prescribes
//! exactly this shape: a local mock upstream serving a fixed Chat SSE
//! stream, with assertions on the Responses SSE event sequence the client
//! sees.
//!
//! Architecture:
//!   1. An in-test axum server (`MockState`) answers `POST /v1/chat/completions`
//!      with a fixed Chat SSE stream (streaming) or a fixed JSON body
//!      (non-streaming), capturing every forwarded request body and the
//!      `Authorization` header so tests can assert model substitution and
//!      API-key injection.
//!   2. A temp `config.toml` points the router at the mock (`providers.mock`,
//!      format = "chat", route `mock/chat`).
//!   3. The router binary is spawned via `CARGO_BIN_EXE_codex-router`, polled
//!      on `/healthz`, and killed on drop.
//!
//! `setup()` bundles all of the above into a `TestEnv` (router URL for
//! client requests, `MockState` for upstream assertions, and a ready reqwest
//! client); every test then issues one or two client requests and inspects
//! both the response and what the mock recorded.

pub use axum::{
    extract::State,
    response::{sse::Event, sse::Sse, IntoResponse, Response},
    routing::post,
    Json, Router,
};
pub use futures_util::StreamExt;
pub use reqwest::StatusCode;
pub use serde_json::{json, Value};
pub use std::convert::Infallible;
pub use std::path::PathBuf;
pub use std::process::Stdio;
pub use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
pub use std::sync::Arc;
pub use std::time::Duration;
pub use tokio::sync::Mutex;

// ---------------------------------------------------------------------------
// Mock upstream
// ---------------------------------------------------------------------------

/// Shared state for the mock: every request body (and Authorization header)
/// the router forwards is recorded.
#[derive(Clone, Default)]
///
/// This struct is the *upstream* side of the harness. Whatever the
/// router forwards on behalf of a client lands here, so tests can later
/// assert what the router actually sent: the substituted model name,
/// the `stream` flag, the merged message history, the stripped
/// `previous_response_id`, and the injected API key. `Arc<Mutex<..>>`
/// lets the axum handler (running on a tokio task) record while tests
/// read afterwards; the handle is `Clone` so the handler and the test
/// can each hold one.
pub struct MockState {
    pub received_requests: Arc<Mutex<Vec<Value>>>,
    pub received_auth: Arc<Mutex<Vec<String>>>,
    /// When true, the no-`[DONE]` handler serves `TRUNCATED_CHUNKS`
    /// (content deltas only, no finish_reason) instead of `STREAM_CHUNKS`,
    /// simulating a stream cut off mid-generation.
    pub truncated: Arc<AtomicBool>,
    /// When non-zero, the chat handler returns this HTTP status instead of
    /// the normal response — used to test error-path metrics (spec §7).
    pub error_status: Arc<AtomicU16>,
}

/// Fixed Chat Completions SSE payloads returned for streaming requests (spec §14).
///
/// Four chunks mirror a typical upstream stream: an opening delta that
/// also carries the `role`, a pure text delta, a finish chunk whose
/// `delta` is empty but carries `finish_reason`, and — as delivered when
/// the router requests `stream_options.include_usage` — a trailing
/// usage-only chunk with an EMPTY choices array after the finish chunk.
/// The router's `StreamConverter` must turn these into the Responses
/// event sequence asserted by
/// `streaming_chat_conversion_returns_responses_sse` — including
/// accumulating "Hello" + " world" into one "Hello world" output part
/// and mapping the trailing chunk's `prompt_tokens`/`completion_tokens`
/// to Responses usage naming in `response.completed`.
pub const STREAM_CHUNKS: [&str; 4] = [
    r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"},"finish_reason":null}]}"#,
    r#"{"choices":[{"index":0,"delta":{"content":" world"},"finish_reason":null}]}"#,
    r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    r#"{"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#,
];

/// Variant of STREAM_CHUNKS that never sends finish_reason: a stream
/// truncated mid-generation.
pub const TRUNCATED_CHUNKS: [&str; 2] = [
    r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"Hel"},"finish_reason":null}]}"#,
    r#"{"choices":[{"index":0,"delta":{"content":"lo"},"finish_reason":null}]}"#,
];
/// Content deltas dripped by `mock_drip_handler`, one every 150ms: eight
/// chunks ≈ 1.2s of total stream time.
pub const DRIP_CHUNK_TEXTS: [&str; 8] = ["a", "b", "c", "d", "e", "f", "g", "h"];

/// Streaming tool-call corpus for the namespace round-trip test: the model
/// calls the tool under its ENCODED `{namespace}-{name}` chat name
/// (`functions-exec_command`) - the name the router bound upstream from the
/// client's `namespace` tool entry. The router must decode this back to a
/// Responses `function_call` with an independent `namespace` field (spec §7).
pub const NS_TOOL_CHUNKS: [&str; 3] = [
    r#"{"choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_ns_1","type":"function","function":{"name":"functions-exec_command","arguments":"{\"cmd\":\"ls\"}"}}]},"finish_reason":null}]}"#,
    r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
    r#"{"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#,
];

/// Single-bar DSML envelope (from codex-relay's captured corpus), split so
/// the closing tag straddles two chunks - pins marker withholding.
pub const DSML_LEAK_CHUNKS: [&str; 4] = [
    "我来逐步完成这个任务。\n<｜DSML｜tool_calls>\n<｜DSML｜invoke name=\"shell\">\n<｜DSML｜parameter name=\"command\" string=\"true\">ls</｜DSML｜parameter>\n</｜DSML｜invoke>\n</｜DSML｜",
    "tool_calls>",
    r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    r#"{"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#,
];

/// <think> leak split across chunks.
pub const THINK_LEAK_CHUNKS: [&str; 4] = [
    "<think>musing</th",
    "ink>Hello",
    r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    r#"{"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#,
];

/// Mock Chat Completions handler (spec §14).
///
/// First records the forwarded request body and `Authorization` header
/// into `MockState` (so tests can assert what the router sent upstream),
/// then answers according to the request's `stream` flag:
/// - `stream: true`  → the fixed `STREAM_CHUNKS` SSE stream, terminated
///   by the `data: [DONE]` sentinel — the exact Chat Completions
///   streaming protocol the router's hand-written SSE parser
///   (`parse_sse_stream`) consumes.
/// - otherwise       → a fixed JSON completion, exercising the
///   non-streaming `chat_response_to_items` path used by
///   `non_streaming_chat_conversion_returns_json`.
///
/// The `[DONE]` sentinel is what lets the router's parser know the
/// upstream stream ended, so it can emit the final
/// `response.completed` event.
pub async fn mock_chat_handler(
    State(state): State<MockState>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    state.received_requests.lock().await.push(parsed.clone());
    state.received_auth.lock().await.push(
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string(),
    );

    // Return non-2xx when the error_status override is set (spec §7).
    let err_status = state.error_status.load(Ordering::SeqCst);
    if err_status != 0 {
        let status = StatusCode::from_u16(err_status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        return (status, Json(json!({"error": "mock error"}))).into_response();
    }

    let is_stream = parsed
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if is_stream {
        let stream = futures_util::stream::iter(
            STREAM_CHUNKS
                .iter()
                .map(|chunk| Ok::<Event, Infallible>(Event::default().data(chunk)))
                .chain(std::iter::once(Ok::<Event, Infallible>(
                    Event::default().data("[DONE]"),
                ))),
        );
        Sse::new(stream).into_response()
    } else {
        Json(json!({
            "choices": [{
                "message": { "role": "assistant", "content": "test response" }
            }],
            "usage": { "prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5 }
        }))
        .into_response()
    }
}

/// Mock Chat Completions handler for the namespace tool round-trip test:
/// streams `NS_TOOL_CHUNKS` (a single encoded tool_call, its finish chunk,
/// and the trailing usage chunk) terminated by the `[DONE]` sentinel.
pub async fn mock_namespace_tool_handler(
    State(state): State<MockState>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    state.received_requests.lock().await.push(parsed.clone());
    state.received_auth.lock().await.push(
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string(),
    );
    let stream = futures_util::stream::iter(
        NS_TOOL_CHUNKS
            .iter()
            .map(|chunk| Ok::<Event, Infallible>(Event::default().data(chunk)))
            .chain(std::iter::once(Ok::<Event, Infallible>(
                Event::default().data("[DONE]"),
            ))),
    );
    Sse::new(stream).into_response()
}

/// Mock Chat Completions handler for the `missing_done` quirk tests: streams
/// `STREAM_CHUNKS` (or `TRUNCATED_CHUNKS` when `state.truncated` is set)
/// WITHOUT the trailing `data: [DONE]` sentinel.
///
/// Some providers drop the sentinel but still send a `finish_reason` chunk;
/// others die mid-stream leaving neither. Both shapes arrive here, letting
/// the router's `missing_done` quirk gate decide what an unterminated stream
/// means.
pub async fn mock_no_done_handler(
    State(state): State<MockState>,
    body: axum::body::Bytes,
) -> Response {
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    state.received_requests.lock().await.push(parsed.clone());
    let truncated = state.truncated.load(Ordering::SeqCst);
    let chunks: &[&str] = if truncated {
        &TRUNCATED_CHUNKS
    } else {
        &STREAM_CHUNKS
    };
    let stream = futures_util::stream::iter(
        chunks
            .iter()
            .map(|c| Ok::<Event, Infallible>(Event::default().data(c))),
    );
    Sse::new(stream).into_response()
}

/// Leaking upstream: serves whichever leak corpus the request encodes, so the
/// healing quirks (`dsml_heal` / `think_tags`) can be exercised end-to-end.
/// Selection is purely request-driven: the first user message content
/// decides which leak to serve ("dsml-probe" → DSML_LEAK_CHUNKS, anything
/// else → THINK_LEAK_CHUNKS). Non-streaming requests get the same leak as
/// one blocking JSON message.
pub async fn mock_leak_handler(
    State(state): State<MockState>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    state.received_requests.lock().await.push(parsed.clone());
    state.received_auth.lock().await.push(
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string(),
    );
    let is_dsml = parsed["messages"][0]["content"]
        .as_str()
        .is_some_and(|c| c.contains("dsml-probe"));
    let is_stream = parsed["stream"].as_bool().unwrap_or(false);
    let chunks: &[&str] = if is_dsml {
        &DSML_LEAK_CHUNKS
    } else {
        &THINK_LEAK_CHUNKS
    };
    if is_stream {
        // The first two entries are RAW leak text (halves of a DSML envelope
        // / think block), not Chat JSON — wrap each in a delta chunk so the
        // router's hand-written SSE parser can decode them. The trailing
        // finish/usage entries are already full Chat JSON payloads and pass
        // through verbatim.
        let stream = futures_util::stream::iter(
            chunks[..2]
                .iter()
                .map(|c| {
                    Ok::<Event, Infallible>(
                        Event::default().data(
                            json!({
                                "choices": [{ "index": 0, "delta": { "content": c } }]
                            })
                            .to_string(),
                        ),
                    )
                })
                .chain(
                    chunks[2..]
                        .iter()
                        .map(|c| Ok::<Event, Infallible>(Event::default().data(c))),
                )
                .chain(std::iter::once(Ok::<Event, Infallible>(
                    Event::default().data("[DONE]"),
                ))),
        );
        Sse::new(stream).into_response()
    } else {
        let content: String = chunks[0..2].concat();
        Json(json!({
            "choices": [{ "message": { "role": "assistant", "content": content } }],
            "usage": { "prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7 }
        }))
        .into_response()
    }
}

/// Mock Chat Completions handler whose first chunk lags by 800ms: models a
/// reasoning-heavy upstream (high/max effort) where time-to-first-chunk is
/// long. The router must open the client stream immediately with
/// `response.created` instead of staying silent until the first chunk
/// arrives - see `slow_first_chunk_still_opens_stream_immediately`.
pub async fn mock_slow_first_chunk_handler(
    State(state): State<MockState>,
    body: axum::body::Bytes,
) -> Response {
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    state.received_requests.lock().await.push(parsed.clone());

    // Split off the first chunk; the async block sleeps before yielding it,
    // then the rest of the stream (including [DONE]) follows immediately.
    let mut events = STREAM_CHUNKS
        .iter()
        .map(|c| Ok::<Event, Infallible>(Event::default().data(c)))
        .chain(std::iter::once(Ok::<Event, Infallible>(
            Event::default().data("[DONE]"),
        )));
    let first = events.next().expect("non-empty STREAM_CHUNKS");
    let stream = futures_util::stream::once(async move {
        tokio::time::sleep(Duration::from_millis(800)).await;
        first
    })
    .chain(futures_util::stream::iter(events));
    Sse::new(stream).into_response()
}

/// Mock Chat upstream that streams the first three `STREAM_CHUNKS` (the
/// third carries `finish_reason`), then stalls for 3s before the trailing
/// usage chunk and `[DONE]`. Against a router `timeout_ms` well below the
/// stall, the stream loop must hit its idle timeout mid-stream — while
/// `finish_reason` was already seen, which is exactly the shape the
/// `missing_done` quirk must NOT rescue (issue #14 / review #2).
pub async fn mock_stall_handler(
    State(state): State<MockState>,
    body: axum::body::Bytes,
) -> Response {
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    state.received_requests.lock().await.push(parsed);
    let stream = futures_util::stream::iter(
        STREAM_CHUNKS[..3]
            .iter()
            .map(|c| Ok::<Event, Infallible>(Event::default().data(*c))),
    )
    .chain(futures_util::stream::once(async move {
        tokio::time::sleep(Duration::from_millis(3000)).await;
        Ok::<Event, Infallible>(Event::default().data(STREAM_CHUNKS[3]))
    }))
    .chain(futures_util::stream::once(async move {
        Ok::<Event, Infallible>(Event::default().data("[DONE]"))
    }));
    Sse::new(stream).into_response()
}

/// Mock Chat upstream that drips one content chunk every 150ms (8 chunks ≈
/// 1.2s total) before the finish/usage/`[DONE]` tail: a healthy stream whose
/// TOTAL duration exceeds `timeout_ms` while every inter-chunk gap stays far
/// under it. The router must complete it — streaming is governed by the idle
/// timeout, not a total-duration cap (issue #14 core).
pub async fn mock_drip_handler(
    State(state): State<MockState>,
    body: axum::body::Bytes,
) -> Response {
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    state.received_requests.lock().await.push(parsed);
    let stream = futures_util::stream::iter(
        DRIP_CHUNK_TEXTS
            .iter()
            .map(|c| {
                Ok::<Event, Infallible>(
                    Event::default()
                        .data(json!({"choices":[{"index":0,"delta":{"content":c}}]}).to_string()),
                )
            })
            .chain(std::iter::once(Ok::<Event, Infallible>(
                Event::default()
                    .data(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#),
            )))
            .chain(std::iter::once(Ok::<Event, Infallible>(
                Event::default().data(STREAM_CHUNKS[3]),
            ))),
    )
    .then(|event| async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        event
    })
    .chain(futures_util::stream::iter(std::iter::once(Ok::<
        Event,
        Infallible,
    >(
        Event::default().data("[DONE]"),
    ))));
    Sse::new(stream).into_response()
}

/// Mock native-Responses upstream that sends `response.created`, stalls 3s,
/// then sends `response.completed`: with a sub-second router timeout the
/// passthrough relay must hit the idle timeout and surface a terminal
/// failure to the client instead of silently ending the stream
/// (review #3).
pub async fn mock_responses_stall_handler(
    State(state): State<MockState>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    state.received_requests.lock().await.push(parsed.clone());
    state.received_auth.lock().await.push(
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string(),
    );
    let stream = futures_util::stream::once(async move {
        Ok::<Event, Infallible>(
            Event::default()
                .event("response.created")
                .data(RESPONSE_CREATED_EVENT),
        )
    })
    .chain(futures_util::stream::once(async move {
        tokio::time::sleep(Duration::from_millis(3000)).await;
        Ok::<Event, Infallible>(
            Event::default()
                .event("response.completed")
                .data(RESPONSE_COMPLETED_EVENT),
        )
    }));
    Sse::new(stream).into_response()
}

/// Mock native-Responses upstream whose `response.completed` carries NO
/// `usage` object (id and output intact). The router must classify the
/// request as success in /metrics — completion is judged by the captured
/// upstream id, not by the presence of token counts (issue #15 item 1).
pub async fn mock_responses_no_usage_handler(
    State(state): State<MockState>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    state.received_requests.lock().await.push(parsed.clone());
    state.received_auth.lock().await.push(
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string(),
    );
    let stream = futures_util::stream::iter(vec![
        Ok::<Event, Infallible>(
            Event::default()
                .event("response.created")
                .data(RESPONSE_CREATED_EVENT),
        ),
        Ok::<Event, Infallible>(
            Event::default()
                .event("response.completed")
                .data(RESPONSE_COMPLETED_NO_USAGE_EVENT),
        ),
    ]);
    Sse::new(stream).into_response()
}

/// Fixed Responses SSE payloads served by the mock native-Responses
/// upstream (`mock_responses_handler`).
///
/// The completed event mirrors the native OpenAI `response`-nested shape:
/// the payload the router's `last_completed_payload` scans for at stream
/// end, from which it derives the session key (the upstream id
/// `resp_mock_123`), the output items (an assistant message saying
/// "Mock reply"), and the token counts. This fixture contains no leaked
/// markup or marker-prefix tails, so the healed relay forwards it
/// byte-for-byte and the client sees the events exactly as served here -
/// including the upstream-generated id that turn 2 must echo back as
/// `previous_response_id`.
pub const RESPONSE_CREATED_EVENT: &str = r#"{"type":"response.created","response":{"id":"resp_mock_123","object":"response","status":"in_progress","model":"upstream-resp-model"}}"#;
pub const RESPONSE_COMPLETED_EVENT: &str = r#"{"type":"response.completed","response":{"id":"resp_mock_123","object":"response","status":"completed","model":"upstream-resp-model","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Mock reply"}]}],"usage":{"input_tokens":4,"output_tokens":3,"total_tokens":7}}}"#;

/// A `response.completed` payload WITHOUT a `usage` object (id and output
/// intact): a completed-with-id-but-no-token-counts stream. Exercises the
/// metrics success predicate (issue #15 item 1) — completion must be judged
/// by the captured id, not by the presence of token counts.
pub const RESPONSE_COMPLETED_NO_USAGE_EVENT: &str = r#"{"type":"response.completed","response":{"id":"resp_mock_nou_1","object":"response","status":"completed","model":"upstream-resp-model","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"No usage reply"}]}]}}"#;

/// Shared implementation for the mock native-Responses upstream handlers.
///
/// Records the forwarded request body and `Authorization` header (same as
/// the Chat mock, into the shared `MockState`), then answers according to
/// the `stream` flag:
/// - `stream: true` -> a `response.created` event, plus a terminal
///   `response.completed` event when `emit_completed` is set. The events
///   are served with their real `event:` names so the passthrough path's
///   whole-buffer `rfind("event: response.completed")` capture works.
/// - otherwise -> a fixed flat (non-nested) completed Responses JSON
///   body, exercising the top-level branch of `completed_capture`.
pub async fn mock_responses_impl(
    state: MockState,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
    emit_completed: bool,
) -> Response {
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    state.received_requests.lock().await.push(parsed.clone());
    state.received_auth.lock().await.push(
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string(),
    );
    let is_stream = parsed
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if is_stream {
        let mut events: Vec<Result<Event, Infallible>> = vec![Ok(Event::default()
            .event("response.created")
            .data(RESPONSE_CREATED_EVENT))];
        if emit_completed {
            events.push(Ok(Event::default()
                .event("response.completed")
                .data(RESPONSE_COMPLETED_EVENT)));
        }
        Sse::new(futures_util::stream::iter(events)).into_response()
    } else {
        Json(json!({
            "id": "resp_mock_123",
            "object": "response",
            "status": "completed",
            "model": "upstream-resp-model",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "Mock reply" }]
            }],
            "usage": { "input_tokens": 4, "output_tokens": 3, "total_tokens": 7 }
        }))
        .into_response()
    }
}

/// Mock native-Responses upstream: answers with the full
/// created + completed event pair, so the router's best-effort session
/// capture succeeds and the turn's full context is stored under the
/// upstream id `resp_mock_123`.
pub async fn mock_responses_handler(
    State(state): State<MockState>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    mock_responses_impl(state, headers, body, true).await
}

/// Mock native-Responses upstream that never sends `response.completed`
/// (a stream cut off before completion): the router's capture finds no
/// completed payload, so no session is persisted and the next turn's
/// `previous_response_id` degrades to a store miss.
pub async fn mock_responses_no_completed_handler(
    State(state): State<MockState>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    mock_responses_impl(state, headers, body, false).await
}

/// Mock responses upstream whose stream leaks DSML markup into
/// output_text (the 2026-08-17 DeepSeek incident shape). Records requests
/// into the shared mock state like the other handlers.
pub async fn mock_leak_responses_handler(
    axum::extract::State(state): axum::extract::State<MockState>,
    body: axum::body::Bytes,
) -> axum::response::Response {
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) {
        state.received_requests.lock().await.push(v);
    }
    let sse = "event: response.created\n\
        data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_leak_1\",\"status\":\"in_progress\"}}\n\n\
        event: response.output_item.added\n\
        data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_leak_1\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}\n\n\
        event: response.output_text.delta\n\
        data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_leak_1\",\"output_index\":0,\"delta\":\"Checking.\\n\"}\n\n\
        event: response.output_text.delta\n\
        data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_leak_1\",\"output_index\":0,\"delta\":\"<｜｜DSML｜｜tool_calls><｜｜DSML｜｜invoke name=\\\"exec_command\\\"><｜｜DSML｜｜parameter name=\\\"cmd\\\" string=\\\"true\\\">touch sentinel-integration</｜｜DSML｜｜parameter></｜｜DSML｜｜invoke></｜｜DSML｜｜tool_calls>\"}\n\n\
        event: response.output_item.done\n\
        data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_leak_1\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"Checking.\\n<｜｜DSML｜｜tool_calls>…</｜｜DSML｜｜tool_calls>\"}]}}\n\n\
        event: response.completed\n\
        data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_leak_1\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"msg_leak_1\",\"content\":[{\"type\":\"output_text\",\"text\":\"Checking.\\n<｜｜DSML｜｜tool_calls>…</｜｜DSML｜｜tool_calls>\"}]}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n";
    axum::response::Response::builder()
        .header("content-type", "text/event-stream")
        .body(axum::body::Body::from(sse))
        .unwrap()
}

/// Spawn the mock upstream on an ephemeral port; returns its base URL and state.
///
/// Binding to port 0 lets the OS pick a free port, so the mock can
/// never collide with the router or other tests. The axum server runs
/// in a detached tokio task for the lifetime of the test process; the
/// returned `MockState` handle is what tests query afterwards. The
/// returned base URL is of the form `http://127.0.0.1:<port>/v1` so
/// the temp config can point a provider at it directly.
pub async fn spawn_mock_upstream() -> (String, MockState) {
    let state = MockState::default();
    let app = Router::new()
        .route("/v1/chat/completions", post(mock_chat_handler))
        .route(
            "/v1/ns-tool/chat/completions",
            post(mock_namespace_tool_handler),
        )
        .route("/v1/no-done/chat/completions", post(mock_no_done_handler))
        .route(
            "/v1/slow/chat/completions",
            post(mock_slow_first_chunk_handler),
        )
        .route("/v1/stall/chat/completions", post(mock_stall_handler))
        .route("/v1/drip/chat/completions", post(mock_drip_handler))
        .route("/v1/stall/responses", post(mock_responses_stall_handler))
        .route(
            "/v1/no-usage/responses",
            post(mock_responses_no_usage_handler),
        )
        .route("/v1/leak/chat/completions", post(mock_leak_handler))
        .route("/v1/responses", post(mock_responses_handler))
        .route(
            "/v1/noid/responses",
            post(mock_responses_no_completed_handler),
        )
        .route("/v1/leak/responses", post(mock_leak_responses_handler))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock listener");
    let addr = listener.local_addr().expect("mock local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("mock server");
    });
    (format!("http://127.0.0.1:{}/v1", addr.port()), state)
}

// ---------------------------------------------------------------------------
// Router subprocess
// ---------------------------------------------------------------------------

/// Owns the router child process and its temp config; kills the child on drop.
///
/// RAII cleanup: when the guard is dropped — at normal end of scope or
/// while a panic unwinds through a test — the child is killed and
/// reaped, so no orphaned router process or leaked temp dir survives a
/// failed test. The `_dir` field keeps the temp config/stderr files
/// alive as long as the guard lives (dropping a `TempDir` deletes
/// them).
pub struct RouterGuard {
    pub child: std::process::Child,
    pub stderr_path: PathBuf,
    pub _dir: tempfile::TempDir,
}

impl Drop for RouterGuard {
    // kill() + wait() = SIGKILL then reap the zombie; both are
    // best-effort (`let _ =`) because the child may already have
    // exited on its own.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Reserve a free loopback port (small race with the router bind; acceptable).
///
/// The port is discovered by binding `:0` and immediately closing the
/// socket, so between this probe and the router's own `bind()` another
/// process could theoretically claim it. `setup()` tolerates that
/// TOCTOU race by retrying once with a fresh port (see the retry loop
/// there).
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind for free port")
        .local_addr()
        .expect("local addr")
        .port()
}

/// Write `config_text` to a temp config file and spawn the router binary
/// against it. Returns the guard (kills the router on drop) and the router
/// base URL.
///
/// The `port` must appear both inside `config_text` (as `[server] port`)
/// and in the returned URL, so the caller passes the same `free_port()`
/// value for both. The router's stderr is redirected into a file inside
/// the temp dir so `wait_for_healthz` can surface the startup log on
/// failure; `CODEX_ROUTER_CONFIG` points the binary at the temp config.
pub fn start_router_with_config(config_text: &str, port: u16) -> (RouterGuard, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, config_text).expect("write router config");

    let stderr_path = dir.path().join("router.stderr.log");
    let stderr = std::fs::File::create(&stderr_path).expect("create stderr log");

    let bin = env!("CARGO_BIN_EXE_codex-router");
    let child = std::process::Command::new(bin)
        .env("CODEX_ROUTER_CONFIG", &config_path)
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .expect("spawn codex-router");

    (
        RouterGuard {
            child,
            stderr_path,
            _dir: dir,
        },
        format!("http://127.0.0.1:{port}"),
    )
}

/// Spawn the router against a standard single-`mock/chat`-route config.
/// Returns the guard (kills the router on drop) and the router base URL.
///
/// The generated config declares a single `mock/chat` route backed by
/// the `mock` provider (format = "chat", API key `test-key`). Having
/// exactly one route keeps assertions like "the mock received exactly
/// one request" deterministic.
pub fn start_router(mock_base_url: &str) -> (RouterGuard, String) {
    let port = free_port();
    let config_text = format!(
        r#"
[server]
host = "127.0.0.1"
port = {port}

[providers.mock]
base_url = "{mock_base_url}"
api_key = "test-key"
format = "chat"
timeout_ms = 10000

[routes]
"mock/chat" = {{ model = "upstream-chat-model" }}
"#
    );
    start_router_with_config(&config_text, port)
}

/// Poll `GET /healthz` until it returns "ok" or the timeout elapses.
/// Returns `Err(stderr_log)` on timeout so callers can retry with a fresh port.
///
/// The 10s deadline covers cold-start work (config parse, listener
/// bind, notify watcher setup). Polling every 50ms keeps the wait
/// snappy. The body must be exactly `"ok"` — anything else (or a
/// non-2xx, or a connection refused) counts as not-yet-healthy and the
/// loop keeps polling. On timeout the router's stderr log is returned
/// so `setup()` can diagnose *why* startup failed (bad config, port
/// already in use, …).
pub async fn wait_for_healthz(router_url: &str, guard: &RouterGuard) -> Result<(), String> {
    let client = reqwest::Client::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(resp) = client.get(format!("{router_url}/healthz")).send().await {
            if resp.status().is_success()
                && resp.text().await.map(|t| t.trim() == "ok").unwrap_or(false)
            {
                return Ok(());
            }
        }
        if std::time::Instant::now() >= deadline {
            let log = std::fs::read_to_string(&guard.stderr_path).unwrap_or_default();
            return Err(log);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Full end-to-end test environment: mock upstream + router subprocess.
///
/// Bundles everything a test needs: the router's base URL (to send
/// client requests to), the `MockState` handle (to inspect what the
/// router forwarded upstream), and a ready-to-use reqwest client. The
/// `_router` guard keeps the subprocess alive for the test's duration
/// and reaps it on drop.
pub struct TestEnv {
    pub _router: RouterGuard,
    pub router_url: String,
    pub mock_state: MockState,
    pub client: reqwest::Client,
}

/// Bring up the full environment: mock upstream + router subprocess.
///
/// Retries once on startup failure with a fresh port: `free_port()`
/// closes its probe socket before the router binds, leaving a small
/// TOCTOU window in which another process can claim the port. Retrying
/// with a brand-new `RouterGuard` (new temp config, new stderr log,
/// new port) makes the race self-healing instead of a flaky CI
/// failure. The stderr log from the failed attempt is included in the
/// panic message for diagnosis.
pub async fn setup() -> TestEnv {
    setup_with_config(|mock_base_url, port| {
        format!(
            r#"
[server]
host = "127.0.0.1"
port = {port}

[providers.mock]
base_url = "{mock_base_url}"
api_key = "test-key"
format = "chat"
timeout_ms = 10000

[routes]
"mock/chat" = {{ model = "upstream-chat-model" }}
"#
        )
    })
    .await
}

/// Bring up the full environment against a caller-supplied config builder.
///
/// The builder receives the live mock upstream's base URL (known only
/// after the mock binds its ephemeral port) and the router port, and
/// returns the complete `config.toml` text - so multi-provider /
/// multi-route setups (e.g. a Chat route next to a native-Responses
/// route) get the same healthz-polling and TOCTOU retry as `setup()`.
pub async fn setup_with_config<F>(build_config: F) -> TestEnv
where
    F: Fn(&str, u16) -> String,
{
    let (mock_url, mock_state) = spawn_mock_upstream().await;
    for attempt in 0..2 {
        let port = free_port();
        let (router, router_url) = start_router_with_config(&build_config(&mock_url, port), port);
        match wait_for_healthz(&router_url, &router).await {
            Ok(()) => {
                return TestEnv {
                    _router: router,
                    router_url,
                    mock_state,
                    client: reqwest::Client::new(),
                };
            }
            Err(log) => {
                // The free_port() probe has a small TOCTOU window; retry once
                // with a fresh port before surfacing the failure.
                drop(router); // kills the child
                if attempt == 1 {
                    panic!("router did not become healthy within 10s; stderr:\n{log}");
                }
            }
        }
    }
    unreachable!("setup loop always returns or panics")
}

// ---------------------------------------------------------------------------
// SSE helpers
// ---------------------------------------------------------------------------

#[derive(Debug)]
/// A single parsed SSE record: the `event:` name plus the `data:`
/// payload.
///
/// Mirrors what the router emits for Responses streams — one `event:`
/// line followed by a single `data:` JSON line per
/// blank-line-delimited record. `event` is empty for records that only
/// carry `data:`.
pub struct SseRecord {
    pub event: String,
    pub data: String,
}

/// Minimal SSE parser: extracts `event:` and `data:` fields per
/// blank-line-delimited record.
///
/// Deliberately tiny: the tests only need to split the response body
/// into records and read each record's event name + JSON payload, so
/// this parser implements just the subset of SSE the router emits. It
/// handles `event:` and `data:` lines (with the optional space after
/// the colon), skips records with no `data:` (e.g. keepalive
/// comments), and joins multi-line `data:` with `\n` — matching
/// `parse_sse_stream`'s semantics upstream.
pub fn parse_sse(text: &str) -> Vec<SseRecord> {
    let mut records = Vec::new();
    for block in text.split("\n\n") {
        if block.trim().is_empty() {
            continue;
        }
        let mut event = String::new();
        let mut data_lines: Vec<&str> = Vec::new();
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("event:") {
                event = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("data:") {
                data_lines.push(rest.trim_start());
            }
        }
        if !data_lines.is_empty() {
            records.push(SseRecord {
                event,
                data: data_lines.join("\n"),
            });
        }
    }
    records
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A slow-to-first-chunk upstream must not delay the client stream's opening.
///
/// At high/max reasoning effort a Chat upstream can take tens of seconds
/// before its first chunk. The router synthesizes `response.created` as
/// soon as the upstream accepts the stream, so the client (Codex TUI) sees
/// "thinking" immediately instead of a silent, hung-looking wait. The mock
/// delays its first chunk by 800ms; the first client event must be
/// `response.created` and arrive well before that delay elapses.
/// Spawn the router against a two-provider config: `mock` at the plain mock
/// URL and `mocknd` at the no-`[DONE]` path (`<mock>/no-done`), with an
/// optional `[quirks]` snippet. Mirrors `setup()`'s TOCTOU retry loop.
pub async fn spawn_router_two_providers(mock_url: &str, quirks: &str) -> (RouterGuard, String) {
    let mut router = None;
    for attempt in 0..2 {
        let port = free_port();
        let config_text = format!(
            r#"
[server]
host = "127.0.0.1"
port = {port}

[providers.mock]
base_url = "{mock_url}"
api_key = "test-key"
format = "chat"
timeout_ms = 10000

[providers.mocknd]
base_url = "{mock_url}/no-done"
api_key = "test-key"
format = "chat"
timeout_ms = 10000

[routes]
"mock/chat" = {{ model = "upstream-chat-model" }}
"mocknd/chat" = {{ model = "upstream-chat-model" }}
{quirks}
"#
        );
        let (guard, router_url) = start_router_with_config(&config_text, port);
        match wait_for_healthz(&router_url, &guard).await {
            Ok(()) => {
                router = Some((guard, router_url));
                break;
            }
            Err(log) => {
                // The free_port() probe has a small TOCTOU window; retry once
                // with a fresh port before surfacing the failure.
                drop(guard); // kills the child
                if attempt == 1 {
                    panic!("router did not become healthy within 10s; stderr:\n{log}");
                }
            }
        }
    }
    router.expect("retry loop always returns or panics")
}

/// Spawn the router against the leak upstream, with optional extra config
/// lines (e.g. a `[quirks]` section). Mirrors `spawn_router_two_providers`'
/// TOCTOU retry loop.
pub async fn spawn_router_leak(mock_url: &str, extra: &str) -> (RouterGuard, String) {
    let mut router = None;
    for _ in 0..2 {
        let port = free_port();
        let config_text = format!(
            r#"
[server]
host = "127.0.0.1"
port = {port}

[providers.mock]
base_url = "{mock_url}/leak"
api_key = "test-key"
format = "chat"
timeout_ms = 10000

[routes]
"mock/chat" = {{ model = "upstream-chat-model" }}
{extra}
"#
        );
        let (r, url) = start_router_with_config(&config_text, port);
        if wait_for_healthz(&url, &r).await.is_ok() {
            router = Some((r, url));
            break;
        }
    }
    router.expect("router failed to start against the leak upstream")
}

/// Config with a Chat-format route (`mock/chat`) and a native-Responses
/// route (`respmock/resp`) side by side, both pointing at the same mock
/// server. This is the cross-format-switch topology: a client can hold a
/// conversation that hops between the two routes mid-session.
pub fn dual_format_config(mock_base_url: &str, port: u16) -> String {
    format!(
        r#"
[server]
host = "127.0.0.1"
port = {port}

[providers.mock]
base_url = "{mock_base_url}"
api_key = "test-key"
format = "chat"
timeout_ms = 10000

[providers.respmock]
base_url = "{mock_base_url}"
api_key = "test-key"
format = "responses"
timeout_ms = 10000

[routes]
"mock/chat" = {{ model = "upstream-chat-model" }}
"respmock/resp" = {{ model = "upstream-resp-model" }}
"#
    )
}

/// Config with a single Chat-format route pointing at the namespace-tool
/// mock handler (`/v1/ns-tool/chat/completions`), whose model answers with
/// an encoded `{namespace}-{name}` tool_call.
pub fn namespace_tool_config(mock_base_url: &str, port: u16) -> String {
    format!(
        r#"
[server]
host = "127.0.0.1"
port = {port}

[providers.mock]
base_url = "{mock_base_url}/ns-tool"
api_key = "test-key"
format = "chat"
timeout_ms = 10000

[routes]
"mock/chat" = {{ model = "upstream-chat-model" }}
"#
    )
}

/// Send one streaming turn through the router; returns the response body
/// text. Shared by the passthrough session tests below.
pub async fn streaming_turn(
    env: &TestEnv,
    model: &str,
    input: &str,
    prev_id: Option<&str>,
) -> String {
    let mut body = json!({"model": model, "input": input, "stream": true});
    if let Some(id) = prev_id {
        body["previous_response_id"] = json!(id);
    }
    let resp = env
        .client
        .post(format!("{}/v1/responses", env.router_url))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    resp.text().await.unwrap()
}

/// Extract the response id from a passthrough SSE body: parse the last
/// `response.completed` event and read `response.id` (the UPSTREAM id -
/// passthrough never substitutes a router-generated one).
pub fn passthrough_response_id(body: &str) -> String {
    let events = parse_sse(body);
    let completed = events
        .iter()
        .find(|e| e.event == "response.completed")
        .expect("response.completed event in passthrough stream");
    let payload: Value = serde_json::from_str(&completed.data).unwrap();
    payload["response"]["id"]
        .as_str()
        .expect("upstream response id")
        .to_string()
}
