//! Integration tests: healing (module-split refactor).

mod common;

pub use common::*;

#[tokio::test]
/// [DONE] missing but `finish_reason` present, quirk on (default): the
/// `missing_done` quirk treats the unterminated stream as complete, so
/// Codex sees `response.completed` and never `response.failed`.
///
/// Regression pin: this is today's always-on behavior — finish_reason
/// alone authorizes completion even without the sentinel. The quirk gate
/// (tested by `disabled_missing_done_quirk_fails_unterminated_stream`)
/// is what makes it conditional.
async fn stream_without_done_but_with_finish_reason_completes() {
    let (mock_url, _state) = spawn_mock_upstream().await;
    let (_router, router_url) = spawn_router_two_providers(&mock_url, "").await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{router_url}/v1/responses"))
        .json(&json!({"model": "mocknd/chat", "input": "hello", "stream": true}))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(body.contains("response.completed"));
    assert!(!body.contains("response.failed"));
}

#[tokio::test]
/// Neither [DONE] nor `finish_reason` (content deltas only): the stream
/// was truncated mid-generation. The existing `!saw_completed` fallback
/// surfaces `response.failed` and never persists a partial session.
///
/// Regression pin for the always-on `on_error` path; the quirk only
/// rescues streams that at least carried a finish_reason.
async fn truncated_stream_without_done_or_finish_reason_fails() {
    let (mock_url, state) = spawn_mock_upstream().await;
    state.truncated.store(true, Ordering::SeqCst);
    let (_router, router_url) = spawn_router_two_providers(&mock_url, "").await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{router_url}/v1/responses"))
        .json(&json!({"model": "mocknd/chat", "input": "hello", "stream": true}))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(body.contains("response.failed"));
    assert!(!body.contains("response.completed"));
}

#[tokio::test]
/// Same mocknd stream as `stream_without_done_but_with_finish_reason_completes`
/// ([DONE] absent, finish_reason present) but the config DISABLES the
/// `missing_done` quirk: strict behavior — only [DONE] authorizes
/// completion, so the unterminated stream must surface `response.failed`
/// and never `response.completed`.
async fn disabled_missing_done_quirk_fails_unterminated_stream() {
    let (mock_url, _state) = spawn_mock_upstream().await;
    let (_router, router_url) = spawn_router_two_providers(
        &mock_url,
        r#"
[quirks]
disabled = ["missing_done"]
"#,
    )
    .await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{router_url}/v1/responses"))
        .json(&json!({"model": "mocknd/chat", "input": "hello", "stream": true}))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(body.contains("response.failed"));
    assert!(!body.contains("response.completed"));
}

#[tokio::test]
/// Streaming DSML leak (quirk `dsml_heal`): the upstream leaks the raw
/// `<｜DSML｜tool_calls>` envelope across two SSE chunks (the closing tag
/// straddles them). The router must release the visible prefix, withhold the
/// marker, and heal the leaked tool call at stream end — the client sees
/// text + a synthesized `function_call`, never the DSML markup.
///
/// Pins the exact 8-event sequence (message created on the first delta,
/// message done, then function_call added → args delta → done at the finish
/// flush) and the semantic contract: text delta == the visible prefix, args
/// delta == `{"command":"ls"}`, completed output kinds ==
/// [message, function_call].
async fn streaming_dsml_leak_heals_into_function_call() {
    let (mock_url, _state) = spawn_mock_upstream().await;
    let (_router, router_url) = spawn_router_leak(&mock_url, "").await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{router_url}/v1/responses"))
        .json(&json!({"model": "mock/chat", "input": "dsml-probe", "stream": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.unwrap();
    let events = parse_sse(&body);

    // Exact event sequence: created, message added, text delta, message done,
    // function_call added, args delta, function_call done, completed.
    let names: Vec<&str> = events.iter().map(|e| e.event.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "response.created",
            "response.output_item.added",
            "response.output_text.delta",
            "response.output_item.done",
            "response.output_item.added",
            "response.function_call_arguments.delta",
            "response.output_item.done",
            "response.completed",
        ],
        "unexpected event sequence; body: {body}"
    );

    // Text delta carries only the visible prefix — no DSML markup.
    let text_delta: Value = serde_json::from_str(&events[2].data).unwrap();
    assert_eq!(text_delta["delta"], "我来逐步完成这个任务。\n");

    // Function-call args delta carries the healed JSON arguments.
    let args_delta: Value = serde_json::from_str(&events[5].data).unwrap();
    assert_eq!(args_delta["delta"], r#"{"command":"ls"}"#);

    // Completed output: message + function_call, in that order.
    let completed: Value = serde_json::from_str(&events[7].data).unwrap();
    assert_eq!(completed["response"]["status"], "completed");
    let output = completed["response"]["output"]
        .as_array()
        .expect("output array");
    let kinds: Vec<&str> = output
        .iter()
        .map(|o| o["type"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(kinds, vec!["message", "function_call"]);
    assert_eq!(output[1]["name"], "shell");
    assert_eq!(output[1]["arguments"], r#"{"command":"ls"}"#);

    // No DSML markup reaches the client anywhere in the body.
    assert!(!body.contains("<｜DSML｜"), "DSML markup leaked to client");
}

#[tokio::test]
/// Streaming think leak (quirk `think_tags`): the upstream leaks
/// `<think>musing</think>Hello` with the closing tag split across two SSE
/// chunks. The router must split the reasoning onto the reasoning channel
/// and keep only "Hello" as visible text.
///
/// Pins the exact 8-event sequence and the semantic contract: reasoning
/// delta == "musing", text delta == "Hello", both done events emitted, and
/// the completed output kinds == [reasoning, message].
async fn streaming_think_leak_splits_onto_reasoning_channel() {
    let (mock_url, _state) = spawn_mock_upstream().await;
    let (_router, router_url) = spawn_router_leak(&mock_url, "").await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{router_url}/v1/responses"))
        .json(&json!({"model": "mock/chat", "input": "think-probe", "stream": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.unwrap();
    let events = parse_sse(&body);

    // Exact event sequence: created, reasoning added, reasoning delta,
    // message added, text delta, reasoning done, message done, completed.
    let names: Vec<&str> = events.iter().map(|e| e.event.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "response.created",
            "response.output_item.added",
            "response.reasoning_summary_text.delta",
            "response.output_item.added",
            "response.output_text.delta",
            "response.output_item.done",
            "response.output_item.done",
            "response.completed",
        ],
        "unexpected event sequence; body: {body}"
    );

    // Reasoning delta carries the hidden musing text.
    let reasoning_delta: Value = serde_json::from_str(&events[2].data).unwrap();
    assert_eq!(reasoning_delta["delta"], "musing");

    // Text delta carries only the visible "Hello".
    let text_delta: Value = serde_json::from_str(&events[4].data).unwrap();
    assert_eq!(text_delta["delta"], "Hello");

    // Both items complete: reasoning with its summary, message with its text.
    let done_reasoning: Value = serde_json::from_str(&events[5].data).unwrap();
    assert_eq!(done_reasoning["item"]["type"], "reasoning");
    assert_eq!(done_reasoning["item"]["summary"][0]["text"], "musing");
    let done_message: Value = serde_json::from_str(&events[6].data).unwrap();
    assert_eq!(done_message["item"]["type"], "message");
    assert_eq!(done_message["item"]["content"][0]["text"], "Hello");

    // Completed output: reasoning + message.
    let completed: Value = serde_json::from_str(&events[7].data).unwrap();
    assert_eq!(completed["response"]["status"], "completed");
    let output = completed["response"]["output"]
        .as_array()
        .expect("output array");
    let kinds: Vec<&str> = output
        .iter()
        .map(|o| o["type"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(kinds, vec!["reasoning", "message"]);
}

#[tokio::test]
/// Non-streaming leak healing (both quirks): the blocking Chat JSON response
/// carries the full leaked DSML envelope / think block in one message. The
/// proxy's in-place healers repair it before conversion — dsml-probe →
/// message + function_call, think-probe → reasoning + message.
async fn non_streaming_leaks_heal() {
    let (mock_url, _state) = spawn_mock_upstream().await;
    let (_router, router_url) = spawn_router_leak(&mock_url, "").await;
    let client = reqwest::Client::new();

    // dsml-probe → message (visible prefix) + function_call (shell, ls).
    let resp = client
        .post(format!("{router_url}/v1/responses"))
        .json(&json!({"model": "mock/chat", "input": "dsml-probe", "stream": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    let output = body["output"].as_array().expect("output array");
    assert_eq!(output.len(), 2);
    assert_eq!(output[0]["type"], "message");
    assert_eq!(output[0]["content"][0]["text"], "我来逐步完成这个任务。\n");
    assert_eq!(output[1]["type"], "function_call");
    assert_eq!(output[1]["name"], "shell");
    assert_eq!(output[1]["arguments"], r#"{"command":"ls"}"#);
    assert!(
        output[1]["call_id"]
            .as_str()
            .unwrap()
            .starts_with("call_dsml_"),
        "synthesized call id: {}",
        output[1]["call_id"]
    );

    // think-probe → reasoning (summary "musing") + message ("Hello").
    let resp = client
        .post(format!("{router_url}/v1/responses"))
        .json(&json!({"model": "mock/chat", "input": "think-probe", "stream": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    let output = body["output"].as_array().expect("output array");
    assert_eq!(output.len(), 2);
    assert_eq!(output[0]["type"], "reasoning");
    assert_eq!(output[0]["summary"][0]["text"], "musing");
    assert_eq!(output[1]["type"], "message");
    assert_eq!(output[1]["content"][0]["text"], "Hello");
}

#[tokio::test]
/// Kill switch (`[quirks] disabled = ["dsml_heal", "think_tags"]`): with both
/// healing quirks off, leaked markup must pass through as plain visible text —
/// no reasoning item, no reasoning_summary_text.delta, and the raw
/// `<think>…</think>` in the streamed text.
async fn disabled_healing_quirks_pass_markup_through() {
    let (mock_url, _state) = spawn_mock_upstream().await;
    let (_router, router_url) = spawn_router_leak(
        &mock_url,
        r#"
[quirks]
disabled = ["dsml_heal", "think_tags"]
"#,
    )
    .await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{router_url}/v1/responses"))
        .json(&json!({"model": "mock/chat", "input": "think-probe", "stream": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.unwrap();
    let events = parse_sse(&body);

    // Kill switch: no reasoning channel — the markup stays visible text.
    assert!(
        !body.contains("response.reasoning_summary_text.delta"),
        "reasoning delta emitted despite kill switch; body: {body}"
    );
    assert!(
        body.contains("<think>"),
        "markup was healed despite kill switch; body: {body}"
    );

    // The streamed text is the raw concatenation, split across two deltas
    // (one per content chunk).
    let text_deltas: Vec<&str> = events
        .iter()
        .filter(|e| e.event == "response.output_text.delta")
        .map(|e| e.data.as_str())
        .collect();
    assert_eq!(text_deltas.len(), 2, "expected 2 text deltas; body: {body}");
    let joined: String = text_deltas
        .iter()
        .map(|data| {
            serde_json::from_str::<Value>(data).expect("delta data")["delta"]
                .as_str()
                .expect("delta string")
                .to_string()
        })
        .collect();
    assert_eq!(joined, "<think>musing</think>Hello");

    // The completed output is a single message holding the raw markup.
    let completed: Value = events
        .iter()
        .find(|e| e.event == "response.completed")
        .expect("completed event")
        .data
        .parse()
        .expect("completed json");
    let output = completed["response"]["output"]
        .as_array()
        .expect("output array");
    assert_eq!(output.len(), 1);
    assert_eq!(output[0]["type"], "message");
    assert_eq!(
        output[0]["content"][0]["text"],
        "<think>musing</think>Hello"
    );
}
