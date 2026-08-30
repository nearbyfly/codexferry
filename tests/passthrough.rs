//! Integration tests: passthrough (module-split refactor).

mod common;

pub use common::*;

#[tokio::test]
/// Review #3: when the passthrough relay hits the idle timeout before a
/// `response.completed` event, the client must receive a terminal
/// `response.failed` event — distinguishable from a silently dropped
/// upstream connection — and metrics must classify it as `timeout`.
async fn passthrough_idle_timeout_emits_terminal_failed_event() {
    let env = setup_with_config(|mock_base_url, port| {
        format!(
            r#"
[server]
host = "127.0.0.1"
port = {port}

[providers.respstall]
base_url = "{mock_base_url}/stall"
api_key = "test-key"
format = "responses"
timeout_ms = 500

[routes]
"respstall/resp" = {{ model = "upstream-resp-model" }}
"#
        )
    })
    .await;

    let resp = env
        .client
        .post(format!("{}/v1/responses", env.router_url))
        .json(&json!({
            "model": "respstall/resp",
            "input": "hello",
            "stream": true,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("response.created"),
        "relayed events must reach the client:\n{body}"
    );
    assert!(
        body.contains("response.failed"),
        "idle-timeout passthrough must end with a terminal failure event:\n{body}"
    );
    assert!(
        !body.contains("response.completed"),
        "no completed event was sent by the mock:\n{body}"
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
        metrics.contains(r#"upstream_requests_total{provider="respstall",route="respstall/resp",model="upstream-resp-model",error_class="timeout"} 1"#),
        "expected timeout error class in /metrics:\n{metrics}"
    );
}

#[tokio::test]
/// Codex >= 0.147 under `use_responses_lite` delivers its tools as
/// `additional_tools` INPUT items (namespace-wrapped) with an empty
/// top-level `tools` array. Third-party Responses upstreams cannot bind
/// tools from that item, so the router's passthrough must hoist them into
/// the top-level `tools` and strip the non-standard items before
/// forwarding - otherwise the model's tool-call markup leaks into visible
/// text (the deepseek DSML leak of 2026-08-17).
async fn responses_passthrough_hoists_additional_tools_into_top_level_tools() {
    let env = setup_with_config(dual_format_config).await;

    let resp = env
        .client
        .post(format!("{}/v1/responses", env.router_url))
        .json(&json!({
            "model": "respmock/resp",
            "stream": false,
            "tools": [],
            "input": [
                {
                    "type": "additional_tools",
                    "role": "developer",
                    "tools": [
                        { "type": "namespace", "name": "functions", "tools": [
                            { "type": "function", "name": "exec_command",
                              "parameters": { "type": "object" } },
                            { "type": "function", "name": "write_stdin" }
                        ]},
                        { "type": "function", "name": "update_plan" }
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
    let tools = upstream["tools"].as_array().expect("top-level tools array");
    let names: Vec<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(names, ["exec_command", "write_stdin", "update_plan"]);
    // The non-standard input item is stripped; the real message remains.
    let input = upstream["input"].as_array().expect("input array");
    assert_eq!(input.len(), 1);
    assert_eq!(input[0]["type"], "message");
}

#[tokio::test]
/// Responses-passthrough session replay, same format (spec §8): turn 1
/// hits a native Responses upstream (passthrough, no conversion) and
/// turn 2 references the UPSTREAM id via
/// `previous_response_id`. Proves the passthrough path's best-effort
/// capture stored the full context under the upstream id and that turn
/// 2 replays it: the forwarded `input` must carry the stored turn-1
/// items (user input + assistant output, verbatim Responses shapes)
/// ahead of the new input, and `previous_response_id` must be consumed,
/// never forwarded.
async fn responses_passthrough_merges_history_same_format() {
    let env = setup_with_config(dual_format_config).await;

    // Turn 1: passthrough SSE; the client sees the upstream's events and id.
    let body = streaming_turn(&env, "respmock/resp", "first message", None).await;
    let response_id = passthrough_response_id(&body);
    assert_eq!(response_id, "resp_mock_123");

    // Turn 2: reference turn 1's upstream id.
    let body2 = streaming_turn(&env, "respmock/resp", "second message", Some(&response_id)).await;
    assert!(!body2.is_empty());

    // The mock received two requests; the second must carry merged history:
    // stored user item + stored assistant output + the new input item.
    let received = env.mock_state.received_requests.lock().await;
    assert_eq!(received.len(), 2);
    let second = &received[1];
    assert_eq!(second["model"], "upstream-resp-model");
    // previous_response_id must not be forwarded to the upstream.
    assert!(second.get("previous_response_id").is_none());
    let input = second["input"].as_array().expect("input array");
    assert_eq!(input.len(), 3, "expected merged history + new input");
    assert_eq!(input[0]["type"], "message");
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[0]["content"][0]["type"], "input_text");
    assert_eq!(input[0]["content"][0]["text"], "first message");
    assert_eq!(input[1]["type"], "message");
    assert_eq!(input[1]["role"], "assistant");
    assert_eq!(input[1]["content"][0]["type"], "output_text");
    assert_eq!(input[1]["content"][0]["text"], "Mock reply");
    assert_eq!(input[2]["type"], "message");
    assert_eq!(input[2]["role"], "user");
    assert_eq!(input[2]["content"][0]["text"], "second message");
}

#[tokio::test]
/// Cross-format provider switch, responses -> chat (spec §8.3): turn 1
/// seeds the session through the native-Responses passthrough route,
/// then turn 2 switches to the Chat route with the same
/// `previous_response_id`. The stored Responses-format history must be
/// converted into Chat messages so the stateless Chat upstream sees the
/// full conversation: user input, assistant reply, new input - in order.
async fn responses_passthrough_history_survives_switch_to_chat() {
    let env = setup_with_config(dual_format_config).await;

    // Turn 1 on the Responses route; session captured under the upstream id.
    let body = streaming_turn(&env, "respmock/resp", "first message", None).await;
    let response_id = passthrough_response_id(&body);

    // Turn 2 switches to the Chat route mid-conversation.
    let body2 = streaming_turn(&env, "mock/chat", "second message", Some(&response_id)).await;
    assert!(!body2.is_empty());

    // Turn 1 landed on the Responses mock (`input` field), turn 2 on the
    // Chat mock (`messages` field) - both share one MockState, so index
    // by arrival order.
    let received = env.mock_state.received_requests.lock().await;
    assert_eq!(received.len(), 2);
    assert!(
        received[0].get("input").is_some(),
        "turn 1 must be Responses-shaped"
    );
    let second = &received[1];
    assert!(
        second.get("messages").is_some(),
        "turn 2 must be Chat-shaped"
    );
    assert!(second.get("previous_response_id").is_none());
    let messages = second["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 3, "expected converted history + new input");
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "first message");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"], "Mock reply");
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[2]["content"], "second message");
}

#[tokio::test]
/// Capture-failure degradation on the passthrough path: an upstream
/// stream that ends WITHOUT `response.completed` leaves no session
/// entry, so turn 2's `previous_response_id` is a store miss and the
/// forwarded request degrades to new-input-only. This pins the
/// documented best-effort gap: passthrough history survives provider
/// switches only when the terminal completed event (with an id) arrives.
async fn responses_passthrough_without_completed_event_loses_history() {
    let env = setup_with_config(|mock_base_url, port| {
        format!(
            r#"
[server]
host = "127.0.0.1"
port = {port}

[providers.noresp]
base_url = "{mock_base_url}/noid"
api_key = "test-key"
format = "responses"
timeout_ms = 10000

[providers.mock]
base_url = "{mock_base_url}"
api_key = "test-key"
format = "chat"
timeout_ms = 10000

[routes]
"noresp/resp" = {{ model = "upstream-resp-model" }}
"mock/chat" = {{ model = "upstream-chat-model" }}
"#
        )
    })
    .await;

    // Turn 1: the stream contains only response.created - no terminal
    // completed event, so the router cannot capture a session.
    let body = streaming_turn(&env, "noresp/resp", "first message", None).await;
    let events = parse_sse(&body);
    assert_eq!(events[0].event, "response.created");
    assert!(
        !events.iter().any(|e| e.event == "response.completed"),
        "no-completed mock must not emit a completed event"
    );
    // The client still learned an id from the created event and echoes it.
    let created: Value = serde_json::from_str(&events[0].data).unwrap();
    let response_id = created["response"]["id"].as_str().unwrap().to_string();

    // Turn 2 switches to the Chat route; the store misses, so the upstream
    // sees only the new input - history is lost (documented degradation).
    streaming_turn(&env, "mock/chat", "second message", Some(&response_id)).await;

    let received = env.mock_state.received_requests.lock().await;
    assert_eq!(received.len(), 2);
    let messages = received[1]["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 1, "history must be lost on a capture miss");
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "second message");
}

#[tokio::test]
/// Phase B: a responses-format upstream that leaks DSML markup into
/// output_text must be healed on the passthrough path — the client receives
/// a function_call item instead of markup, and the session replay (turn 2)
/// carries the healed items, not the leaked text.
async fn responses_passthrough_dsml_leak_heals_into_function_call() {
    let env = setup_with_config(|mock_base, port| {
        format!(
            r#"
[server]
host = "127.0.0.1"
port = {port}

[providers.leak]
base_url = "{mock_base}/leak"
api_key = "test-key"
format = "responses"
timeout_ms = 10000

[routes]
"leak/resp" = {{ model = "leak-upstream" }}
"#
        )
    })
    .await;

    // Turn 1: streamed leak healed.
    let body = streaming_turn(&env, "leak/resp", "run the tool", None).await;
    assert!(
        body.contains("response.function_call_arguments.delta"),
        "healed function_call events missing:\n{body}"
    );
    assert!(
        body.contains("\"name\":\"exec_command\""),
        "healed call name missing:\n{body}"
    );
    assert!(!body.contains("DSML"), "markup leaked through:\n{body}");
    let response_id = passthrough_response_id(&body);

    // Turn 2: replay carries the healed items (function_call, clean text).
    let body2 = streaming_turn(&env, "leak/resp", "thanks", Some(&response_id)).await;
    assert!(!body2.is_empty());
    let received = env.mock_state.received_requests.lock().await;
    let second = received.last().unwrap();
    let input = second["input"].as_array().expect("input array");
    assert!(
        input
            .iter()
            .any(|it| it.get("type").and_then(|v| v.as_str()) == Some("function_call")),
        "replayed history must carry the healed function_call item"
    );
    assert!(
        !serde_json::to_string(second).unwrap().contains("DSML"),
        "leaked text must not be replayed upstream"
    );
}

#[tokio::test]
/// Phase B fast-path pin: with the healing quirks disabled on a
/// responses-format route, the passthrough is a verbatim byte relay — the
/// leaked DSML markup survives untouched (today's pre-healing behavior) and
/// no function_call items are synthesized.
async fn responses_passthrough_healing_disabled_relays_leak_verbatim() {
    let env = setup_with_config(|mock_base, port| {
        format!(
            r#"
[server]
host = "127.0.0.1"
port = {port}

[providers.leak]
base_url = "{mock_base}/leak"
api_key = "test-key"
format = "responses"
timeout_ms = 10000

[quirks]
disabled = ["dsml_heal", "think_tags"]

[routes]
"leak/resp" = {{ model = "leak-upstream" }}
"#
        )
    })
    .await;

    // No healing: the leaked DSML markup must be relayed through verbatim
    // (the command argument survives), and no function_call events appear.
    let body = streaming_turn(&env, "leak/resp", "run the tool", None).await;
    assert!(
        body.contains("touch sentinel-integration"),
        "leak must pass through when healing is disabled:\n{body}"
    );
    assert!(
        !body.contains("response.function_call_arguments.delta"),
        "no healed function_call events when gates are off:\n{body}"
    );
}

#[tokio::test]
/// Issue #15 item 1: a `response.completed` event WITHOUT a `usage` object
/// (id and output intact) is still a completed response — metrics must
/// classify it as success (`error_class=""`), never `stream_truncated`.
/// The streaming path's success predicate is the captured upstream id
/// (same as `stream_status` and the non-streaming path), not the presence
/// of token counts.
async fn completed_without_usage_counts_as_success_in_metrics() {
    let env = setup_with_config(|mock_base_url, port| {
        format!(
            r#"
[server]
host = "127.0.0.1"
port = {port}

[providers.respnou]
base_url = "{mock_base_url}/no-usage"
api_key = "test-key"
format = "responses"
timeout_ms = 10000

[routes]
"respnou/resp" = {{ model = "upstream-resp-model" }}
"#
        )
    })
    .await;

    let resp = env
        .client
        .post(format!("{}/v1/responses", env.router_url))
        .json(&json!({
            "model": "respnou/resp",
            "input": "hello",
            "stream": true,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("response.completed"),
        "the completed event must be relayed:\n{body}"
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
        metrics.contains(r#"upstream_requests_total{provider="respnou",route="respnou/resp",model="upstream-resp-model",error_class=""} 1"#),
        "completed-without-usage must be counted as success:\n{metrics}"
    );
    assert!(
        !metrics.contains(r#"error_class="stream_truncated""#),
        "completed-without-usage must not be classified as truncated:\n{metrics}"
    );
}

#[tokio::test]
/// End-to-end pin for the `merge_fragmented` heal pass (spec
/// `docs/superpowers/specs/2026-08-30-fragmented-items-merger-design.md`).
/// The mock upstream emits 5 message fragments — each `output_text.delta`
/// arrives as its own `output_item.added` event, the MiniMax M3 shape
/// observed in NOTES-2026-08-28 §2. With the merger wired into the
/// passthrough relay, the client must see exactly ONE
/// `response.output_item.added` event (the first fragment passes
/// through; the other four are suppressed) and one merged assistant
/// message whose text concatenates all five `"chunk{i} "` deltas.
///
/// Without the merger, the client would see 5 `output_item.added`
/// events and 5 disjoint message items in the Codex TUI (one bullet per
/// chunk). The assertion is the binary test of merger-correctness
/// through the real binary against a real upstream HTTP boundary.
async fn passthrough_merges_fragmented_message_run() {
    let env = setup_with_config(|mock_base_url, port| {
        format!(
            r#"
[server]
host = "127.0.0.1"
port = {port}

[providers.merger]
base_url = "{mock_base_url}/fragmented"
api_key = "test-key"
format = "responses"
timeout_ms = 5000

[routes]
"merger/M3" = {{ model = "upstream-M3" }}
"#
        )
    })
    .await;

    let resp = env
        .client
        .post(format!("{}/v1/responses", env.router_url))
        .json(&json!({
            "model": "merger/M3",
            "input": "hello",
            "stream": true,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.unwrap();

    // The merger collapses 5 upstream fragments into 1 client-visible
    // item; the trailing 4 `output_item.added` events are suppressed.
    // Match on the SSE `event:` line (not the JSON `data:` payload,
    // which also contains the literal `"type":"response.output_item.added"`
    // string) so each event counts once.
    let added_count = body.matches("event: response.output_item.added").count();
    assert_eq!(
        added_count, 1,
        "expected exactly 1 output_item.added (merged from 5 fragments), got {added_count}:\n{body}"
    );

    // The merged text must be the concatenation of all 5 deltas. The
    // deltas carry a trailing space (`"chunk{i} "`), so the merged
    // body text is exactly `"chunk0 chunk1 chunk2 chunk3 chunk4 "`
    // — confirmed by checking each chunk individually (the
    // implementation re-emits 5 separate deltas, all rewritten to the
    // first fragment's item_id).
    let events = parse_sse(&body);
    let mut text_chunks: Vec<String> = Vec::new();
    for evt in &events {
        if evt.event == "response.output_text.delta" {
            let v: Value = serde_json::from_str(&evt.data).unwrap();
            if let Some(d) = v["delta"].as_str() {
                text_chunks.push(d.to_string());
            }
        }
    }
    assert_eq!(
        text_chunks.join(""),
        "chunk0 chunk1 chunk2 chunk3 chunk4 ",
        "merged deltas must concatenate the 5 fragments"
    );

    // Each rewritten delta must point at the FIRST fragment's
    // item_id (`msg_0`) and output_index (0), not its own — that's
    // the delta-rewrite rule from spec §Event rewriting rules.
    for evt in events
        .iter()
        .filter(|e| e.event == "response.output_text.delta")
    {
        let v: Value = serde_json::from_str(&evt.data).unwrap();
        assert_eq!(
            v["item_id"].as_str(),
            Some("msg_0"),
            "item_id rewritten: {evt:?}"
        );
        assert_eq!(
            v["output_index"].as_u64(),
            Some(0),
            "output_index rewritten: {evt:?}"
        );
    }

    // The relay must still emit a `response.completed` event so the
    // client's session replay and metrics classification see a
    // successful turn (success is judged by the captured upstream id,
    // AGENTS.md §11).
    assert!(
        events.iter().any(|e| e.event == "response.completed"),
        "response.completed missing from relay:\n{body}"
    );
}

#[tokio::test]
/// End-to-end pin for the merger's type-switch boundary (spec §State
/// machine). The mock upstream emits a mixed stream: 3 message
/// fragments → 1 reasoning item (standalone) → 2 message fragments,
/// all under the same response. With the merger wired in, the client
/// must see 3 `output_item.added` events: the merged first message run
/// (length 3 → 1), the reasoning item (length 1 → 1, untouched), and the
/// merged second message run (length 2 → 1). The reasoning summary
/// stays inside the reasoning item; the message runs concatenate text
/// independently of each other.
async fn passthrough_merges_interleaved_runs() {
    let env = setup_with_config(|mock_base_url, port| {
        format!(
            r#"
[server]
host = "127.0.0.1"
port = {port}

[providers.merger]
base_url = "{mock_base_url}/fragmented"
api_key = "test-key"
format = "responses"
timeout_ms = 5000

[routes]
"merger/M3" = {{ model = "upstream-M3" }}
"#
        )
    })
    .await;

    let resp = env
        .client
        .post(format!("{}/v1/responses", env.router_url))
        .json(&json!({
            "model": "merger/M3",
            "input": "interleaved hello",
            "stream": true,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.unwrap();

    let events = parse_sse(&body);

    // Exactly 3 client-visible items (3 merged/suppressed upstream
    // runs → 3 distinct items: msg-run-1, reasoning, msg-run-2).
    let added_count = body.matches("event: response.output_item.added").count();
    assert_eq!(
        added_count, 3,
        "expected 3 output_item.added (msg+reasoning+msg), got {added_count}:\n{body}"
    );

    // Collect the per-item types so we can verify the reasoning item
    // is its own item (not merged with the surrounding messages).
    let mut added_items: Vec<Value> = Vec::new();
    for evt in events
        .iter()
        .filter(|e| e.event == "response.output_item.added")
    {
        let v: Value = serde_json::from_str(&evt.data).unwrap();
        if let Some(t) = v["item"]["type"].as_str() {
            added_items.push(json!({ "type": t, "id": v["item"]["id"] }));
        }
    }
    assert_eq!(added_items.len(), 3);
    let types: Vec<&str> = added_items
        .iter()
        .map(|i| i["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        vec!["message", "reasoning", "message"],
        "expected msg → reasoning → msg ordering"
    );

    // The first message run concatenates its 3 deltas (text = "m0 m1 m2 ").
    // The second message run concatenates its 2 deltas (text = "n4 n5 ").
    // The reasoning item carries its own summary.
    let mut msg_run1_text = String::new();
    let mut msg_run2_text = String::new();
    for evt in events
        .iter()
        .filter(|e| e.event == "response.output_text.delta")
    {
        let v: Value = serde_json::from_str(&evt.data).unwrap();
        if let Some(d) = v["delta"].as_str() {
            match v["item_id"].as_str() {
                // After the merger rewrite, the first message run's
                // deltas all carry msg_0; the second message run's
                // deltas all carry msg_4 (its first fragment's id).
                Some("msg_0") => msg_run1_text.push_str(d),
                Some("msg_4") => msg_run2_text.push_str(d),
                _ => {}
            }
        }
    }
    assert_eq!(
        msg_run1_text, "m0 m1 m2 ",
        "first message run must concatenate its 3 deltas"
    );
    assert_eq!(
        msg_run2_text, "n4 n5 ",
        "second message run must concatenate its 2 deltas"
    );

    // The reasoning summary delta must reach the client unchanged
    // (run length 1, no rewriting).
    assert!(
        events
            .iter()
            .any(|e| e.event == "response.reasoning_summary_text.delta"),
        "reasoning summary delta must reach the client:\n{body}"
    );
}
