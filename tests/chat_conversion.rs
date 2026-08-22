//! Integration tests: chat_conversion (module-split refactor).

mod common;

pub use common::*;

#[tokio::test]
/// The core end-to-end streaming path (spec §14): a client
/// `POST /v1/responses` with `stream: true` must yield a Responses SSE
/// stream built from the mock's three Chat Completions chunks.
///
/// This is the highest-value test in the file — it pins down the exact
/// event contract Codex CLI relies on. It verifies:
/// - Content-Type is `text/event-stream`.
/// - The exact 9-event sequence: `response.created` →
///   `response.in_progress` → `output_item.added` /
///   `content_part.added` → two `output_text.delta` (one per upstream
///   content chunk) → `content_part.done` / `output_item.done` →
///   `response.completed`. Any reordering or added/removed event fails
///   here.
/// - `response.created` carries a router-generated `resp_*` id that
///   `response.completed` echoes (and that the client can use as
///   `previous_response_id` on the next turn).
/// - Text deltas arrive in upstream order and accumulate into one
///   `output_text` part: "Hello" + " world" = "Hello world".
/// - Usage tokens are mapped from Chat naming (`prompt_tokens` /
///   `completion_tokens`) to Responses naming (`input_tokens` /
///   `output_tokens`).
/// - The upstream received exactly one request, with the route alias
///   substituted by the configured upstream model (`mock/chat` →
///   `upstream-chat-model`), `stream: true`, a single user message,
///   and the configured API key injected as
///   `Authorization: Bearer test-key`.
async fn streaming_chat_conversion_returns_responses_sse() {
    let env = setup().await;

    let resp = env
        .client
        .post(format!("{}/v1/responses", env.router_url))
        .json(&json!({"model": "mock/chat", "input": "hello", "stream": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        content_type.contains("text/event-stream"),
        "expected SSE content-type, got: {content_type}"
    );
    let body = resp.text().await.unwrap();
    let events = parse_sse(&body);

    // Exact event sequence: created, item added (lazy), two text deltas,
    // item done, completed. No in_progress, content_part.added/done.
    let names: Vec<&str> = events.iter().map(|e| e.event.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "response.created",
            "response.output_item.added",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.output_item.done",
            "response.completed",
        ],
        "unexpected event sequence; body: {body}"
    );

    // response.created has type + response wrapper.
    let created: Value = serde_json::from_str(&events[0].data).unwrap();
    assert_eq!(created["type"], "response.created");
    let resp_id = created["response"]["id"]
        .as_str()
        .expect("response.created id");
    assert!(resp_id.starts_with("resp_"), "id: {resp_id}");
    assert_eq!(created["response"]["model"], "mock/chat");
    assert_eq!(created["response"]["status"], "in_progress");

    // output_item.added has output_index + item with id + status.
    let item_added: Value = serde_json::from_str(&events[1].data).unwrap();
    assert_eq!(item_added["type"], "response.output_item.added");
    assert_eq!(item_added["output_index"], 0);
    assert_eq!(item_added["item"]["type"], "message");
    assert_eq!(item_added["item"]["role"], "assistant");

    // Text deltas carry item_id + output_index.
    let delta1: Value = serde_json::from_str(&events[2].data).unwrap();
    assert_eq!(delta1["delta"], "Hello");
    assert!(delta1["item_id"].as_str().unwrap().starts_with("msg_"));
    let delta2: Value = serde_json::from_str(&events[3].data).unwrap();
    assert_eq!(delta2["delta"], " world");

    // output_item.done has full accumulated text + completed status.
    let done: Value = serde_json::from_str(&events[4].data).unwrap();
    assert_eq!(done["type"], "response.output_item.done");
    assert_eq!(done["item"]["content"][0]["text"], "Hello world");
    assert_eq!(done["item"]["status"], "completed");

    // response.completed has type + response wrapper + output + usage.
    let completed: Value = serde_json::from_str(&events[5].data).unwrap();
    assert_eq!(completed["type"], "response.completed");
    assert_eq!(completed["response"]["id"], resp_id);
    assert_eq!(completed["response"]["status"], "completed");
    let output = completed["response"]["output"]
        .as_array()
        .expect("output array");
    assert_eq!(output.len(), 1);
    assert_eq!(output[0]["type"], "message");
    assert_eq!(output[0]["content"][0]["text"], "Hello world");
    assert_eq!(completed["response"]["usage"]["input_tokens"], 5);
    assert_eq!(completed["response"]["usage"]["output_tokens"], 2);

    // The router forwarded one request with model substitution + stream flag
    // and injected the configured API key as a Bearer token.
    let received = env.mock_state.received_requests.lock().await;
    assert_eq!(received.len(), 1);
    let upstream_req = &received[0];
    assert_eq!(upstream_req["model"], "upstream-chat-model");
    assert_eq!(upstream_req["stream"], true);
    let messages = upstream_req["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "hello");
    let auth = env.mock_state.received_auth.lock().await;
    assert_eq!(auth.as_slice(), &["Bearer test-key"]);

    // Scrape /metrics and assert counters incremented correctly (spec §7).
    let metrics_resp = env
        .client
        .get(format!("{}/metrics", env.router_url))
        .send()
        .await
        .unwrap();
    assert_eq!(metrics_resp.status(), StatusCode::OK);
    let body = metrics_resp.text().await.unwrap();

    // Assert request counter with empty error_class (success).
    assert!(
        body.contains(r#"upstream_requests_total{provider="mock",route="mock/chat",model="upstream-chat-model",error_class=""} 1"#),
        "expected success counter in /metrics, got:
{body}"
    );
    // Assert token counters.
    assert!(
        body.contains(
            r#"input_tokens_total{provider="mock",route="mock/chat",model="upstream-chat-model"} 5"#
        ),
        "expected input_tokens in /metrics, got:
{body}"
    );
    assert!(
        body.contains(r#"output_tokens_total{provider="mock",route="mock/chat",model="upstream-chat-model"} 2"#),
        "expected output_tokens in /metrics, got:
{body}"
    );
    // Assert histogram counts.
    assert!(
        body.contains(r#"upstream_ttft_seconds_count{provider="mock",route="mock/chat",model="upstream-chat-model"} 1"#),
        "expected ttft histogram count in /metrics, got:
{body}"
    );
    assert!(
        body.contains(r#"upstream_duration_seconds_count{provider="mock",route="mock/chat",model="upstream-chat-model"} 1"#),
        "expected duration histogram count in /metrics, got:
{body}"
    );
}

#[tokio::test]
async fn slow_first_chunk_still_opens_stream_immediately() {
    let (mock_url, _state) = spawn_mock_upstream().await;
    // Provider base_url points at the slow path; `start_router` builds the
    // same single `mock/chat`-route config around whatever URL it gets.
    let mut env = None;
    for _ in 0..2 {
        let (router, router_url) = start_router(&format!("{mock_url}/slow"));
        if wait_for_healthz(&router_url, &router).await.is_ok() {
            env = Some((router, router_url));
            break;
        }
    }
    let (_router, router_url) = env.expect("router failed to start against the slow upstream");

    let client = reqwest::Client::new();
    let started = std::time::Instant::now();
    let mut resp = client
        .post(format!("{router_url}/v1/responses"))
        .json(&json!({"model": "mock/chat", "input": "hello", "stream": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Read incrementally until the first complete SSE event ("\n\n"
    // terminator) lands; timing is the point of this test.
    let mut buf = String::new();
    let first_raw = loop {
        let chunk = resp
            .chunk()
            .await
            .expect("stream error before first event")
            .expect("stream ended before first event");
        buf.push_str(&String::from_utf8_lossy(&chunk));
        if let Some(pos) = buf.find("\n\n") {
            break buf[..pos].to_string();
        }
    };
    let elapsed = started.elapsed().as_millis();
    assert!(
        first_raw.contains("event: response.created"),
        "first event must be response.created, got: {first_raw}"
    );
    assert!(
        elapsed < 700,
        "response.created took {elapsed}ms; not immediate (upstream delay is 800ms)"
    );
}

#[tokio::test]
/// The non-streaming path (`stream: false`): the mock answers with a
/// plain JSON completion and the router must return a single Responses
/// JSON object (not SSE), converted by `chat_response_to_items`.
///
/// Guards the JSON branch that Codex uses for lightweight/quick
/// prompts. Verifies the `resp_*` id, the assistant `message` output
/// item carrying the mock's `"test response"` text, the usage
/// mapping, and that the request is forwarded with `stream: false` so
/// upstreams know not to stream.
async fn non_streaming_chat_conversion_returns_json() {
    let env = setup().await;

    let resp = env
        .client
        .post(format!("{}/v1/responses", env.router_url))
        .json(&json!({"model": "mock/chat", "input": "hello", "stream": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["object"], "response");
    assert!(body["id"].as_str().unwrap().starts_with("resp_"));
    let output = body["output"].as_array().expect("output array");
    assert_eq!(output.len(), 1);
    assert_eq!(output[0]["type"], "message");
    assert_eq!(output[0]["content"][0]["text"], "test response");
    assert_eq!(body["usage"]["input_tokens"], 3);
    assert_eq!(body["usage"]["output_tokens"], 2);
    assert_eq!(body["usage"]["total_tokens"], 5);

    // Non-streaming request forwarded with stream=false.
    let received = env.mock_state.received_requests.lock().await;
    assert_eq!(received.len(), 1);
    let upstream_req = &received[0];
    assert_eq!(upstream_req["model"], "upstream-chat-model");
    assert_eq!(upstream_req["stream"], false);
}

#[tokio::test]
/// Issue #14 / review #2: when the streaming idle timeout fires mid-stream,
/// the outcome must be an explicit failure — `response.failed` — even though
/// `finish_reason` was already seen and the `missing_done` quirk is enabled
/// (the quirk must not rescue a proxy-initiated timeout into a completion).
/// Metrics must classify it as `timeout`, and no session may be persisted
/// (turn 2's `previous_response_id` degrades to a store miss).
async fn streaming_idle_timeout_fails_stream_and_skips_session() {
    let env = setup_with_config(|mock_base_url, port| {
        format!(
            r#"
[server]
host = "127.0.0.1"
port = {port}

[providers.stall]
base_url = "{mock_base_url}/stall"
api_key = "test-key"
format = "chat"
timeout_ms = 500

[routes]
"stall/chat" = {{ model = "upstream-chat-model" }}

[quirks]
missing_done = true
"#
        )
    })
    .await;

    // Turn 1: the mock streams three chunks (incl. finish_reason), then
    // stalls 3s — the router must give up after the 500ms idle timeout.
    let resp = env
        .client
        .post(format!("{}/v1/responses", env.router_url))
        .json(&json!({"model": "stall/chat", "input": "hello", "stream": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.unwrap();
    assert!(
        !body.contains("response.completed"),
        "timed-out stream must not be rescued into a completion:\n{body}"
    );
    assert!(
        body.contains("response.failed"),
        "timed-out stream must surface response.failed:\n{body}"
    );

    // Metrics: classified as a timeout, never as success.
    let metrics = env
        .client
        .get(format!("{}/metrics", env.router_url))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains(r#"upstream_requests_total{provider="stall",route="stall/chat",model="upstream-chat-model",error_class="timeout"} 1"#),
        "expected timeout error class in /metrics:\n{metrics}"
    );
    assert!(
        !metrics.contains(r#"error_class="""#),
        "timed-out stream must not be counted as success:\n{metrics}"
    );

    // Turn 2 with turn 1's response id: the failed turn must not have been
    // persisted, so the upstream receives only the new input.
    let events = parse_sse(&body);
    let created: Value = serde_json::from_str(&events[0].data).unwrap();
    let response_id = created["response"]["id"].as_str().expect("response id");
    let resp = env
        .client
        .post(format!("{}/v1/responses", env.router_url))
        .json(&json!({
            "model": "stall/chat",
            "input": "second message",
            "stream": true,
            "previous_response_id": response_id,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let received = env.mock_state.received_requests.lock().await;
    assert_eq!(received.len(), 2);
    let messages = received[1]["messages"].as_array().expect("messages array");
    assert_eq!(
        messages.len(),
        1,
        "timed-out turn must not be persisted; upstream must see only the new input"
    );
    assert_eq!(messages[0]["content"], "second message");
}

#[tokio::test]
/// Issue #14 core: `timeout_ms` must not cap a healthy stream's TOTAL
/// duration — a stream whose chunks arrive steadily (150ms gaps, well under
/// the 400ms idle timeout) but whose total exceeds timeout_ms must complete
/// normally. (Before the fix, reqwest's per-request total deadline killed
/// such streams mid-flight.)
async fn healthy_stream_longer_than_timeout_ms_completes() {
    let env = setup_with_config(|mock_base_url, port| {
        format!(
            r#"
[server]
host = "127.0.0.1"
port = {port}

[providers.drip]
base_url = "{mock_base_url}/drip"
api_key = "test-key"
format = "chat"
timeout_ms = 400

[routes]
"drip/chat" = {{ model = "upstream-chat-model" }}
"#
        )
    })
    .await;

    let resp = env
        .client
        .post(format!("{}/v1/responses", env.router_url))
        .json(&json!({"model": "drip/chat", "input": "hello", "stream": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("response.completed"),
        "healthy stream longer than timeout_ms must complete:\n{body}"
    );
    assert!(
        !body.contains("response.failed"),
        "healthy stream must not fail:\n{body}"
    );

    let metrics = env
        .client
        .get(format!("{}/metrics", env.router_url))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        !metrics.contains(r#"error_class="timeout""#),
        "healthy stream must not be classified as timeout:\n{metrics}"
    );
}

#[tokio::test]
/// The same hoisting on the Chat path: a `to_chat_request` conversion whose
/// only tool source is an `additional_tools` input item (Codex 0.147
/// responses-lite shape) must still deliver function tools to the Chat
/// upstream, replayed history included (name-deduplicated).
async fn chat_conversion_hoists_additional_tools_from_input_and_history() {
    let env = setup_with_config(dual_format_config).await;

    let resp = env
        .client
        .post(format!("{}/v1/responses", env.router_url))
        .json(&json!({
            "model": "mock/chat",
            "stream": false,
            "input": [
                {
                    "type": "additional_tools",
                    "role": "developer",
                    "tools": [
                        { "type": "namespace", "name": "functions", "tools": [
                            { "type": "function", "name": "exec_command",
                              "description": "Run a shell command",
                              "parameters": { "type": "object" } }
                        ]}
                    ]
                },
                { "type": "message", "role": "user",
                  "content": [{ "type": "input_text", "text": "hi" }] }
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let received = env.mock_state.received_requests.lock().await;
    let upstream = received.last().unwrap();
    let tools = upstream["tools"].as_array().expect("chat tools array");
    assert_eq!(tools.len(), 1);
    // Namespace-inner tools hoisted on the chat path keep the encoded
    // `{namespace}-{name}` name (spec §7), so the upstream binds the same
    // tool name the model's DSML calls use.
    assert_eq!(tools[0]["function"]["name"], "functions-exec_command");
    assert_eq!(tools[0]["function"]["description"], "Run a shell command");
}

#[tokio::test]
/// End-to-end namespace tool round-trip on the chat path (spec §7): a
/// client request carrying a `namespace` tool entry is flattened and bound
/// upstream under the encoded `{namespace}-{name}` name; the model's
/// tool_call using that name must decode back to a Responses `function_call`
/// with an independent `namespace` field in every SSE event the client sees.
///
/// A unit test cannot prove this: the request-side `NamespaceToolMap`
/// construction and the response-side decoder live in different modules and
/// are only connected by the wiring in `proxy.rs` — the exact wiring whose
/// absence produced "unsupported call: spawn_agent" on the real chat path.
async fn chat_path_namespace_tool_round_trip() {
    let env = setup_with_config(namespace_tool_config).await;

    let resp = env
        .client
        .post(format!("{}/v1/responses", env.router_url))
        .json(&json!({
            "model": "mock/chat",
            "stream": true,
            "tools": [
                { "type": "namespace", "name": "functions", "tools": [
                    { "type": "function", "name": "exec_command",
                      "description": "Run a shell command",
                      "parameters": { "type": "object" } }
                ]}
            ],
            "input": [{ "type": "message", "role": "user",
                        "content": [{ "type": "input_text", "text": "run ls" }] }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.unwrap();
    let events = parse_sse(&body);

    // Request side: the upstream bound the ENCODED name.
    let received = env.mock_state.received_requests.lock().await;
    let upstream = received.last().unwrap();
    let tools = upstream["tools"].as_array().expect("chat tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["function"]["name"], "functions-exec_command");

    // Response side: the tool_call decodes back to namespace + name in the
    // output_item.added event ...
    let added: Value = serde_json::from_str(
        &events
            .iter()
            .find(|e| e.event == "response.output_item.added")
            .expect("output_item.added event")
            .data,
    )
    .unwrap();
    assert_eq!(added["item"]["type"], "function_call");
    assert_eq!(added["item"]["namespace"], "functions");
    assert_eq!(added["item"]["name"], "exec_command");

    // ... the output_item.done event ...
    let done: Value = serde_json::from_str(
        &events
            .iter()
            .find(|e| e.event == "response.output_item.done")
            .expect("output_item.done event")
            .data,
    )
    .unwrap();
    assert_eq!(done["item"]["namespace"], "functions");
    assert_eq!(done["item"]["name"], "exec_command");

    // ... and the response.completed output array.
    let completed: Value = serde_json::from_str(
        &events
            .iter()
            .find(|e| e.event == "response.completed")
            .expect("response.completed event")
            .data,
    )
    .unwrap();
    let out = &completed["response"]["output"][0];
    assert_eq!(out["type"], "function_call");
    assert_eq!(out["namespace"], "functions");
    assert_eq!(out["name"], "exec_command");
}

#[tokio::test]
/// Provider escape hatches (spec §5): `extra_params` must be merged into
/// the forwarded Chat body (winning on collision), and `drop_params`
/// must strip named fields — here `reasoning_effort` from a
/// `reasoning.effort` request — before the body reaches the upstream.
///
/// Uses a custom config (via `start_router_with_config`) instead of the
/// standard `setup()` config, and asserts on what the mock actually
/// received: `top_k` and `max_tokens` present (from `extra_params`; the
/// latter wins over the converted `max_output_tokens`), `reasoning_effort`
/// absent (dropped), and the route's `test-model` substitution intact.
async fn extra_and_drop_params_reach_upstream_body() {
    let (mock_url, state) = spawn_mock_upstream().await;
    // The free_port() probe has a small TOCTOU window; mirror setup()'s
    // retry loop with a fresh port (and thus fresh config) per attempt.
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
extra_params = {{ top_k = 50, max_tokens = 999 }}
drop_params = ["reasoning_effort"]
[routes]
"mock/chat" = {{ model = "test-model" }}
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
    let (_router, router_url) = router.expect("retry loop always returns or panics");

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{router_url}/v1/responses"))
        .json(&serde_json::json!({
            "model": "mock/chat",
            "stream": false,
            "input": "hi",
            "reasoning": { "effort": "high" },
            "max_output_tokens": 100
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let sent = state.received_requests.lock().await;
    let body = sent.last().unwrap();
    assert_eq!(body["model"], "test-model");
    assert_eq!(body["top_k"], serde_json::json!(50));
    // Extra wins on collision: the request's max_output_tokens: 100 converts
    // to max_tokens: 100, but extra_params overrides it to 999.
    assert_eq!(body["max_tokens"], serde_json::json!(999));
    assert!(body.get("reasoning_effort").is_none());
}

#[tokio::test]
/// Quirk `glm_thinking` reaches the actual upstream body (spec §2): with a
/// GLM-like *upstream* model, the forwarded chat body must carry
/// `thinking: {"type":"enabled"}` — even though the route alias
/// (`mock/chat`) is not GLM-like, proving the gate keys on the upstream
/// model only. Unit tests prove the serialization; this closes the wiring
/// gap by asserting on what `handle_chat_format` actually sends upstream.
async fn glm_thinking_reaches_upstream_body() {
    let (mock_url, state) = spawn_mock_upstream().await;
    // The free_port() probe has a small TOCTOU window; mirror setup()'s
    // retry loop with a fresh port (and thus fresh config) per attempt.
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
[routes]
"mock/chat" = {{ model = "glm-5" }}
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
    let (_router, router_url) = router.expect("retry loop always returns or panics");

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{router_url}/v1/responses"))
        .json(&serde_json::json!({
            "model": "mock/chat",
            "stream": false,
            "input": "hi",
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let sent = state.received_requests.lock().await;
    let body = sent.last().unwrap();
    assert_eq!(body["model"], "glm-5");
    assert_eq!(body["thinking"], serde_json::json!({ "type": "enabled" }));
}
