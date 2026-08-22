//! Unit tests for tests module, extracted from `request.rs`
//! (module-split spec Phase 1; bodies are verbatim moves).
use super::*;

fn make_request(input: &str) -> ResponsesRequest {
    serde_json::from_str(input).unwrap()
}

/// A minimal request the effort/thinking tests mutate and reuse.
fn base_req() -> ResponsesRequest {
    make_request(r#"{"model":"x","input":"hello"}"#)
}

#[test]
fn reasoning_effort_forwarded_verbatim() {
    let mut req = base_req();
    req.reasoning = Some(json!({ "effort": "xhigh" }));
    let chat = to_chat_request(&req, &[], "deepseek-v4-pro", false);
    assert_eq!(chat.reasoning_effort.as_deref(), Some("xhigh"));
    // Serialized shape matters: upstreams read a top-level string field.
    let body = serde_json::to_value(&chat).unwrap();
    assert_eq!(body["reasoning_effort"], json!("xhigh"));
}

#[test]
fn absent_reasoning_omits_the_field() {
    let chat = to_chat_request(&base_req(), &[], "deepseek-v4-pro", false);
    assert!(chat.reasoning_effort.is_none());
    let body = serde_json::to_value(&chat).unwrap();
    assert!(body.get("reasoning_effort").is_none());
}

#[test]
fn glm_models_get_thinking_switch_when_quirk_on() {
    let mut req = base_req();
    req.reasoning = None;
    let chat = to_chat_request(&req, &[], "zai-org/GLM-5", true);
    let body = serde_json::to_value(&chat).unwrap();
    assert_eq!(body["thinking"], json!({ "type": "enabled" }));
    // Quirk off: no field, preserving the request shape for GLM too.
    let chat = to_chat_request(&req, &[], "zai-org/GLM-5", false);
    let body = serde_json::to_value(&chat).unwrap();
    assert!(body.get("thinking").is_none());
}

#[test]
fn non_glm_models_never_get_thinking_switch() {
    let chat = to_chat_request(&base_req(), &[], "deepseek-v4-flash", true);
    let body = serde_json::to_value(&chat).unwrap();
    assert!(body.get("thinking").is_none());
}

#[test]
fn reasoning_effort_and_thinking_compose() {
    let mut req = base_req();
    req.reasoning = Some(json!({ "effort": "high" }));
    let chat = to_chat_request(&req, &[], "glm-4.6", true);
    let body = serde_json::to_value(&chat).unwrap();
    // Both fields live on the same serialized body: the effort
    // passthrough and the GLM thinking switch must coexist.
    assert_eq!(body["reasoning_effort"], json!("high"));
    assert_eq!(body["thinking"], json!({ "type": "enabled" }));
}

#[test]
fn converts_string_input() {
    let req = make_request(r#"{"model":"x","input":"hello"}"#);
    let chat = to_chat_request(&req, &[], "upstream-model", false);
    assert_eq!(chat.model, "upstream-model");
    assert_eq!(chat.messages.len(), 1);
    assert_eq!(chat.messages[0].role, "user");
    assert_eq!(chat.messages[0].text_content(), "hello");
}

#[test]
fn converts_instructions_to_system() {
    let req = make_request(r#"{"model":"x","input":"hi","instructions":"be helpful"}"#);
    let chat = to_chat_request(&req, &[], "m", false);
    assert_eq!(chat.messages[0].role, "system");
    assert_eq!(chat.messages[0].text_content(), "be helpful");
    assert_eq!(chat.messages[1].role, "user");
}

#[test]
fn converts_function_call_output() {
    let req = make_request(
        r#"{"model":"x","input":[{"type":"function_call_output","call_id":"c1","output":"result"}]}"#,
    );
    let chat = to_chat_request(&req, &[], "m", false);
    assert_eq!(chat.messages[0].role, "tool");
    assert_eq!(chat.messages[0].tool_call_id.as_deref(), Some("c1"));
    assert_eq!(chat.messages[0].text_content(), "result");
}

#[test]
fn converts_function_tool() {
    let req = make_request(
        r#"{"model":"x","input":"hi","tools":[{"type":"function","name":"exec","description":"run","parameters":{"type":"object","properties":{}}}]}"#,
    );
    let chat = to_chat_request(&req, &[], "m", false);
    assert_eq!(chat.tools.len(), 1);
    assert_eq!(chat.tools[0]["function"]["name"], "exec");
}

#[test]
fn converts_freeform_tool_to_function() {
    let req = make_request(
        r#"{"model":"x","input":"hi","tools":[{"type":"freeform","name":"apply_patch","description":"patch","input_schema":{}}]}"#,
    );
    let chat = to_chat_request(&req, &[], "m", false);
    assert_eq!(chat.tools.len(), 1);
    let params = &chat.tools[0]["function"]["parameters"];
    assert_eq!(params["properties"]["input"]["type"], "string");
    assert_eq!(params["required"][0], "input");
}

#[test]
fn maps_max_output_tokens_to_max_tokens() {
    let req = make_request(r#"{"model":"x","input":"hi","max_output_tokens":4096}"#);
    let chat = to_chat_request(&req, &[], "m", false);
    assert_eq!(chat.max_tokens, Some(4096));
}

#[test]
fn drops_unknown_fields() {
    let req = make_request(
        r#"{"model":"x","input":"hi","store":true,"metadata":{"k":"v"},"previous_response_id":"r1"}"#,
    );
    let chat = to_chat_request(&req, &[], "m", false);
    let json = serde_json::to_string(&chat).unwrap();
    assert!(!json.contains("store"));
    assert!(!json.contains("metadata"));
    assert!(!json.contains("previous_response_id"));
}

#[test]
fn merges_history_items() {
    let history = vec![
        json!({"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}),
        json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi there"}]}),
    ];
    let req = make_request(r#"{"model":"x","input":"next question"}"#);
    let chat = to_chat_request(&req, &history, "m", false);
    assert_eq!(chat.messages.len(), 3);
    assert_eq!(chat.messages[0].role, "user");
    assert_eq!(chat.messages[0].text_content(), "hello");
    assert_eq!(chat.messages[1].role, "assistant");
    assert_eq!(chat.messages[2].role, "user");
    assert_eq!(chat.messages[2].text_content(), "next question");
}

#[test]
fn merges_text_and_tool_call_from_history() {
    let history = vec![
        json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"let me check"}]}),
        json!({"type":"function_call","call_id":"c1","name":"exec","arguments":"{\"cmd\":\"ls\"}"}),
    ];
    let req = make_request(r#"{"model":"x","input":"next"}"#);
    let chat = to_chat_request(&req, &history, "m", false);
    assert_eq!(chat.messages.len(), 2);
    assert_eq!(chat.messages[0].role, "assistant");
    assert_eq!(chat.messages[0].text_content(), "let me check");
    assert!(chat.messages[0].tool_calls.is_some());
    assert_eq!(chat.messages[0].tool_calls.as_ref().unwrap().len(), 1);
    assert_eq!(chat.messages[1].role, "user");
}

#[test]
fn merges_inline_transcript_in_input_items() {
    // Codex CLI with store=false replays its full transcript inline in
    // `input` (no previous_response_id). The same-turn assistant text +
    // function_call merge must apply there too, or the stateless Chat
    // upstream sees a dangling assistant message followed by another.
    let req = make_request(
        r#"{"model":"x","input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"ls please"}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"let me check"}]},
                {"type":"function_call","call_id":"c1","name":"exec","arguments":"{\"cmd\":\"ls\"}"},
                {"type":"function_call_output","call_id":"c1","output":"file_a\nfile_b"},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"thanks"}]}
            ]}"#,
    );
    let chat = to_chat_request(&req, &[], "m", false);
    let roles: Vec<&str> = chat.messages.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, vec!["user", "assistant", "tool", "user"]);
    // The assistant message carries BOTH text and the merged tool call.
    assert_eq!(chat.messages[1].text_content(), "let me check");
    assert_eq!(chat.messages[1].tool_calls.as_ref().unwrap().len(), 1);
    // The tool output stays attached to the merged message.
    assert_eq!(chat.messages[2].tool_call_id.as_deref(), Some("c1"));
    assert_eq!(chat.messages[2].text_content(), "file_a\nfile_b");
}

#[test]
fn merges_multiple_tool_calls_with_text() {
    let history = vec![
        json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}),
        json!({"type":"function_call","call_id":"c1","name":"a","arguments":"{}"}),
        json!({"type":"function_call","call_id":"c2","name":"b","arguments":"{}"}),
    ];
    let req = make_request(r#"{"model":"x","input":"next"}"#);
    let chat = to_chat_request(&req, &history, "m", false);
    assert_eq!(chat.messages.len(), 2);
    assert_eq!(chat.messages[0].role, "assistant");
    assert_eq!(chat.messages[0].tool_calls.as_ref().unwrap().len(), 2);
}

#[test]
fn merges_reasoning_text_and_tool_call() {
    let history = vec![
        json!({"type":"reasoning","summary":[{"type":"summary_text","text":"thinking..."}]}),
        json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}),
        json!({"type":"function_call","call_id":"c1","name":"exec","arguments":"{}"}),
    ];
    let req = make_request(r#"{"model":"x","input":"next"}"#);
    let chat = to_chat_request(&req, &history, "m", false);
    assert_eq!(chat.messages.len(), 2);
    assert_eq!(chat.messages[0].role, "assistant");
    assert_eq!(chat.messages[0].text_content(), "answer");
    assert_eq!(
        chat.messages[0].reasoning_content.as_deref(),
        Some("thinking...")
    );
    assert!(chat.messages[0].tool_calls.is_some());
}

#[test]
fn standalone_tool_call_without_message_merges() {
    let history =
        vec![json!({"type":"function_call","call_id":"c1","name":"exec","arguments":"{}"})];
    let req = make_request(r#"{"model":"x","input":"next"}"#);
    let chat = to_chat_request(&req, &history, "m", false);
    assert_eq!(chat.messages.len(), 2);
    assert_eq!(chat.messages[0].role, "assistant");
    assert!(chat.messages[0].tool_calls.is_some());
    assert_eq!(chat.messages[0].tool_calls.as_ref().unwrap().len(), 1);
}

#[test]
fn assistant_item_reasoning_content_survives_replay() {
    // An assistant message item carrying `reasoning_content` directly
    // (as produced by some clients / passthrough shapes) must
    // keep it through the inline merge branch, not just via a preceding
    // standalone reasoning item.
    let history = vec![json!({
        "type": "message",
        "role": "assistant",
        "content": [{"type": "output_text", "text": "answer"}],
        "reasoning_content": "inline thinking"
    })];
    let req = make_request(r#"{"model":"x","input":"next"}"#);
    let chat = to_chat_request(&req, &history, "m", false);
    assert_eq!(chat.messages.len(), 2);
    assert_eq!(chat.messages[0].role, "assistant");
    assert_eq!(
        chat.messages[0].reasoning_content.as_deref(),
        Some("inline thinking")
    );
}

#[test]
fn empty_reasoning_summary_does_not_shadow_item_reasoning() {
    // A reasoning item with an empty summary (Some("")) must not mask
    // the assistant message's own reasoning_content: Option::or treats
    // it as present, which would replay "" instead of the real text.
    let history = vec![
        json!({"type":"reasoning","summary":[{"type":"summary_text","text":""}]}),
        json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "answer"}],
            "reasoning_content": "inline thinking"
        }),
    ];
    let req = make_request(r#"{"model":"x","input":"next"}"#);
    let chat = to_chat_request(&req, &history, "m", false);
    assert_eq!(
        chat.messages[0].reasoning_content.as_deref(),
        Some("inline thinking")
    );
}

#[test]
fn empty_reasoning_summary_does_not_clear_buffered_reasoning() {
    // An empty summary must not overwrite an earlier buffered one.
    let history = vec![
        json!({"type":"reasoning","summary":[{"type":"summary_text","text":"part1"}]}),
        json!({"type":"reasoning","summary":[{"type":"summary_text","text":""}]}),
        json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}),
    ];
    let req = make_request(r#"{"model":"x","input":"next"}"#);
    let chat = to_chat_request(&req, &history, "m", false);
    assert_eq!(chat.messages[0].reasoning_content.as_deref(), Some("part1"));
}

#[test]
fn preceding_reasoning_item_wins_over_item_reasoning_content() {
    // Pre-merge behavior: a preceding standalone reasoning item takes
    // precedence over the message item's own `reasoning_content`.
    let history = vec![
        json!({"type":"reasoning","summary":[{"type":"summary_text","text":"buffered"}]}),
        json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "answer"}],
            "reasoning_content": "inline thinking"
        }),
    ];
    let req = make_request(r#"{"model":"x","input":"next"}"#);
    let chat = to_chat_request(&req, &history, "m", false);
    assert_eq!(chat.messages[0].role, "assistant");
    assert_eq!(
        chat.messages[0].reasoning_content.as_deref(),
        Some("buffered")
    );
}

#[test]
fn round_trip_text_before_reasoning_attaches_reasoning_on_replay() {
    // When text deltas arrive before reasoning
    // deltas, the persisted session items must still replay with the
    // reasoning attached to the same turn's assistant message - not
    // dangling after it (which would drop it on the next turn).
    use crate::convert::response::StreamConverter;
    use crate::wire::chat::{ChatDelta, ChatStreamChoice, ChatStreamChunk};

    fn chunk(delta: ChatDelta, finish: Option<&str>) -> ChatStreamChunk {
        ChatStreamChunk {
            choices: vec![ChatStreamChoice {
                index: 0,
                delta,
                finish_reason: finish.map(String::from),
            }],
            usage: None,
        }
    }

    let mut conv = StreamConverter::new(
        "resp_rt".into(),
        "m".into(),
        crate::heal::HealGates::default(),
        crate::normalize::NamespaceToolMap::new(),
    );
    conv.on_chunk(&chunk(
        ChatDelta {
            content: Some("answer".into()),
            ..Default::default()
        },
        None,
    ));
    conv.on_chunk(&chunk(
        ChatDelta {
            reasoning_content: Some("thinking".into()),
            ..Default::default()
        },
        None,
    ));
    conv.on_chunk(&chunk(ChatDelta::default(), Some("stop")));
    conv.finish();

    let history = conv.acc.items.clone();
    let req = make_request(r#"{"model":"x","input":"next"}"#);
    let chat = to_chat_request(&req, &history, "m", false);
    assert_eq!(chat.messages.len(), 2);
    assert_eq!(chat.messages[0].role, "assistant");
    assert_eq!(chat.messages[0].text_content(), "answer");
    assert_eq!(
        chat.messages[0].reasoning_content.as_deref(),
        Some("thinking")
    );
    assert_eq!(chat.messages[1].role, "user");
}

#[test]
fn converts_image_input() {
    let req = make_request(
        r#"{"model":"x","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"what is this"},{"type":"input_image","image_url":"data:image/png;base64,abc"}]}]}"#,
    );
    let chat = to_chat_request(&req, &[], "m", false);
    let content = chat.messages[0].content.as_ref().unwrap();
    assert!(content.is_array());
    let parts = content.as_array().unwrap();
    assert_eq!(parts[0]["type"], "text");
    assert_eq!(parts[1]["type"], "image_url");
}

#[test]
fn converts_single_text_part_to_string() {
    // Spec §7.1: pure text content should use plain string form.
    let item =
        json!({"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]});
    let msg = convert_responses_item_to_chat(&item).unwrap();
    assert_eq!(msg.content.as_ref().unwrap().as_str(), Some("hello"));
}

#[test]
fn single_image_part_stays_array() {
    let item = json!({"type":"message","role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,abc"}]});
    let msg = convert_responses_item_to_chat(&item).unwrap();
    let content = msg.content.as_ref().unwrap();
    assert!(content.is_array());
    assert_eq!(content.as_array().unwrap()[0]["type"], "image_url");
}

#[test]
fn empty_or_unknown_parts_yield_none() {
    assert!(convert_content(&json!([])).is_none());
    assert!(convert_content(&json!([{"type":"weird","x":1}])).is_none());
}

#[test]
fn replays_function_call_item() {
    let item =
        json!({"type":"function_call","call_id":"c1","name":"exec","arguments":"{\"cmd\":\"ls\"}"});
    let msg = convert_responses_item_to_chat(&item).unwrap();
    assert_eq!(msg.role, "assistant");
    let tc = msg.tool_calls.as_ref().unwrap()[0].clone();
    assert_eq!(tc["id"], "c1");
    assert_eq!(tc["function"]["name"], "exec");
}

#[test]
fn converts_custom_tool_to_function() {
    let req = make_request(
        r#"{"model":"x","input":"hi","tools":[{"type":"custom","name":"apply_patch","description":"patch"}]}"#,
    );
    let chat = to_chat_request(&req, &[], "m", false);
    assert_eq!(chat.tools.len(), 1);
    assert_eq!(chat.tools[0]["function"]["name"], "apply_patch");
}

#[test]
fn system_field_becomes_system_message() {
    let req = make_request(r#"{"model":"x","input":"hi","system":"be terse"}"#);
    let chat = to_chat_request(&req, &[], "m", false);
    assert_eq!(chat.messages[0].role, "system");
    assert_eq!(chat.messages[0].text_content(), "be terse");
}

#[test]
fn preserves_reasoning_across_history_replay() {
    // A session stores [reasoning, message] items (from StreamConverter or
    // chat_response_to_items). On replay, the reasoning must be attached to
    // the following assistant message as reasoning_content.
    let history = vec![
        json!({"type":"reasoning","summary":[{"type":"summary_text","text":"thinking..."}]}),
        json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"the answer"}]}),
    ];
    let req = make_request(r#"{"model":"x","input":"next"}"#);
    let chat = to_chat_request(&req, &history, "m", false);
    assert_eq!(chat.messages.len(), 2);
    assert_eq!(chat.messages[0].role, "assistant");
    assert_eq!(chat.messages[0].text_content(), "the answer");
    assert_eq!(
        chat.messages[0].reasoning_content.as_deref(),
        Some("thinking...")
    );
    assert_eq!(chat.messages[1].role, "user");
}

#[test]
fn trailing_reasoning_without_assistant_is_dropped() {
    // A standalone reasoning item at the end of history with no following
    // assistant message must not panic; the reasoning is simply dropped.
    let history =
        vec![json!({"type":"reasoning","summary":[{"type":"summary_text","text":"thinking..."}]})];
    let req = make_request(r#"{"model":"x","input":"next"}"#);
    let chat = to_chat_request(&req, &history, "m", false);
    assert_eq!(chat.messages.len(), 1);
    assert_eq!(chat.messages[0].role, "user");
    assert_eq!(chat.messages[0].text_content(), "next");
    assert!(chat.messages[0].reasoning_content.is_none());
}

#[test]
fn passes_through_presence_and_frequency_penalty() {
    // Spec §7.1: presence_penalty and frequency_penalty are passthrough fields.
    let req = make_request(
        r#"{"model":"x","input":"hi","presence_penalty":0.5,"frequency_penalty":1.2}"#,
    );
    let chat = to_chat_request(&req, &[], "m", false);
    assert_eq!(chat.presence_penalty, Some(0.5));
    assert_eq!(chat.frequency_penalty, Some(1.2));
}

#[test]
fn passes_through_tool_choice() {
    // Spec §7.1: tool_choice passthrough.
    let req = make_request(r#"{"model":"x","input":"hi","tool_choice":"auto"}"#);
    let chat = to_chat_request(&req, &[], "m", false);
    assert_eq!(chat.tool_choice, Some(json!("auto")));
}

#[test]
fn tool_choice_none_when_absent() {
    let req = make_request(r#"{"model":"x","input":"hi"}"#);
    let chat = to_chat_request(&req, &[], "m", false);
    assert!(chat.tool_choice.is_none());
}

#[test]
fn presence_and_frequency_penalty_absent_when_not_set() {
    let req = make_request(r#"{"model":"x","input":"hi"}"#);
    let chat = to_chat_request(&req, &[], "m", false);
    assert!(chat.presence_penalty.is_none());
    assert!(chat.frequency_penalty.is_none());
    // Verify they are skipped in serialization
    let json = serde_json::to_string(&chat).unwrap();
    assert!(!json.contains("presence_penalty"));
    assert!(!json.contains("frequency_penalty"));
}

#[test]
fn namespace_tools_flatten_into_chat_request() {
    let req = make_request(
        r#"{"model":"x","input":"hi","tools":[
                {"type":"namespace","name":"multi_agent_v1","tools":[
                    {"type":"function","name":"get_goal","description":"g","parameters":{"type":"object"}}
                ]}
            ]}"#,
    );
    let chat = to_chat_request(&req, &[], "m", false);
    let body = serde_json::to_value(&chat).unwrap();
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    // Namespace-inner function tools are encoded to `{namespace}-{name}`
    // when flattened for Chat upstreams (spec §7), so the upstream binds
    // `multi_agent_v1-get_goal` instead of the bare `get_goal`.
    assert_eq!(tools[0]["function"]["name"], "multi_agent_v1-get_goal");
    assert_eq!(tools[0]["function"]["description"], "g");
}

#[test]
fn additional_tools_not_flagged_unknown_but_unknowns_are() {
    // Parity with the responses path: `additional_tools` is a CONSUMED
    // known dialect (the hoist extracts its tools), so the per-request
    // unknown-item warn must NOT count it — counting it would emit
    // expected WARN noise on every tool-using chat request and drown out
    // genuinely new dialects. Genuine unknowns must still be surfaced.
    let probe = "normalize_test_chat_warn_e";
    let req = make_request(
        r#"{"model":"x","input":[
                {"type":"additional_tools","tools":[{"type":"function","name":"exec_command"}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
                {"type":"normalize_test_chat_warn_e"}
            ]}"#,
    );
    let before = crate::normalize::unknown_type_counts()
        .get(probe)
        .copied()
        .unwrap_or(0);
    to_chat_request(&req, &[], "m", false);
    // The consumed dialect is never flagged...
    assert!(!crate::normalize::unknown_type_counts().contains_key("additional_tools"));
    // ...while a genuine unknown input item type IS counted (delta +1).
    let after = crate::normalize::unknown_type_counts()
        .get(probe)
        .copied()
        .unwrap_or(0);
    assert_eq!(after, before + 1);
}

#[test]
fn chat_path_warns_unmappable_tool_dropped_from_additional_tools_namespace() {
    // An unmappable tool type inside an `additional_tools` namespace is
    // dropped by the hoist on the chat path too; it must be surfaced
    // once per request, never silently swallowed (spec §1 visibility).
    let probe = "normalize_test_chat_tool_warn_j";
    let req = make_request(
        r#"{"model":"x","input":[
                {"type":"additional_tools","tools":[
                    {"type":"namespace","name":"functions","tools":[
                        {"type":"function","name":"exec_command"},
                        {"type":"normalize_test_chat_tool_warn_j"}
                    ]}
                ]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
            ]}"#,
    );
    let before = crate::normalize::unknown_type_counts()
        .get(probe)
        .copied()
        .unwrap_or(0);
    to_chat_request(&req, &[], "m", false);
    let after = crate::normalize::unknown_type_counts()
        .get(probe)
        .copied()
        .unwrap_or(0);
    assert_eq!(after, before + 1);
}

#[test]
fn chat_tools_dedup_across_top_level_and_additional_tools() {
    // Parity pin: a tool bound both top-level and inside an
    // `additional_tools` input item must reach the Chat upstream exactly
    // once (the hoist and normalize_chat_tools both dedup by name).
    let req = make_request(
        r#"{"model":"x","input":[
                {"type":"additional_tools","tools":[
                    {"type":"function","name":"get_goal"}
                ]}
            ],"tools":[
                {"type":"function","name":"get_goal","description":"g","parameters":{"type":"object"}}
            ]}"#,
    );
    let chat = to_chat_request(&req, &[], "m", false);
    let body = serde_json::to_value(&chat).unwrap();
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["function"]["name"], "get_goal");
    assert_eq!(tools[0]["function"]["description"], "g");
}

#[test]
fn function_call_with_namespace_replays_encoded_chat_name() {
    let fc = json!({
        "type": "function_call",
        "call_id": "c1",
        "namespace": "multi_agent_v1",
        "name": "spawn_agent",
        "arguments": "{}",
    });
    let tc = function_call_to_tool_call(&fc);
    assert_eq!(tc["function"]["name"], "multi_agent_v1-spawn_agent");
}

#[test]
fn function_call_without_namespace_replays_flat_name() {
    let fc = json!({
        "type": "function_call",
        "call_id": "c1",
        "name": "exec_command",
        "arguments": "{\"cmd\":\"ls\"}",
    });
    let tc = function_call_to_tool_call(&fc);
    assert_eq!(tc["function"]["name"], "exec_command");
}
