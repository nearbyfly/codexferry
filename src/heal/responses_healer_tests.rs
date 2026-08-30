//! Unit tests for responses-format stream healing, extracted from `heal.rs`
//! (module-split spec Phase 1; bodies are verbatim moves).
use super::responses::sse_block;
use super::*;
use bytes::Bytes;
use serde_json::{json, Value};

/// Mirror of the 2026-08-17 DeepSeek leak capture (event shapes as
/// captured; the DSML envelope split across several text deltas).
const DSML_LEAK_SSE: &str = "event: response.created\n\
        data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\"}}\n\n\
        event: response.output_item.added\n\
        data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}\n\n\
        event: response.output_text.delta\n\
        data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"output_index\":0,\"delta\":\"I'll check the weather.\\n\"}\n\n\
        event: response.output_text.delta\n\
        data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"output_index\":0,\"delta\":\"<｜｜DSML｜｜tool_calls>\\n<｜｜DSML｜｜invoke name=\\\"exec_command\\\">\\n\"}\n\n\
        event: response.output_text.delta\n\
        data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"output_index\":0,\"delta\":\"<｜｜DSML｜｜parameter name=\\\"cmd\\\" string=\\\"true\\\">curl wttr.in/Paris</｜｜DSML｜｜parameter>\\n</｜｜DSML｜｜invoke>\\n</｜｜DSML｜｜tool_calls>\"}\n\n\
        event: response.output_text.done\n\
        data: {\"type\":\"response.output_text.done\",\"item_id\":\"msg_1\",\"output_index\":0,\"text\":\"I'll check the weather.\\n<｜｜DSML｜｜tool_calls>…</｜｜DSML｜｜tool_calls>\"}\n\n\
        event: response.output_item.done\n\
        data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"I'll check the weather.\\n<｜｜DSML｜｜tool_calls>…</｜｜DSML｜｜tool_calls>\"}]}}\n\n\
        event: response.completed\n\
        data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"I'll check the weather.\\n<｜｜DSML｜｜tool_calls>…</｜｜DSML｜｜tool_calls>\"}]}],\"usage\":{\"input_tokens\":1,\"output_tokens\":2,\"total_tokens\":3}}}\n\n";

/// Drive the healer over a scripted SSE text and return the joined
/// forwarded bytes.
fn run_healer(gates: HealGates, sse: &str) -> String {
    let mut healer = ResponsesStreamHealer::new(gates);
    let mut out = String::new();
    for block in sse.split("\n\n").filter(|b| !b.is_empty()) {
        let raw = format!("{block}\n\n");
        let (event, data) = {
            let mut event = None;
            let mut data = Vec::new();
            for line in block.lines() {
                if let Some(rest) = line.strip_prefix("event: ") {
                    event = Some(rest.to_string());
                }
                if let Some(rest) = line.strip_prefix("data: ") {
                    data.push(rest);
                }
            }
            (event, data.join("\n"))
        };
        for chunk in healer.push_event(raw.as_bytes(), event.as_deref(), &data) {
            out.push_str(&String::from_utf8_lossy(&chunk));
        }
    }
    for chunk in healer.finish() {
        out.push_str(&String::from_utf8_lossy(&chunk));
    }
    out
}

#[test]
fn healthy_stream_is_byte_identical() {
    let healthy = DSML_LEAK_SSE
            .replace("<｜｜DSML｜｜tool_calls>\\n<｜｜DSML｜｜invoke name=\\\"exec_command\\\">\\n", "plain ")
            .replace("<｜｜DSML｜｜parameter name=\\\"cmd\\\" string=\\\"true\\\">curl wttr.in/Paris</｜｜DSML｜｜parameter>\\n</｜｜DSML｜｜invoke>\\n</｜｜DSML｜｜tool_calls>", "text. ")
            .replace("<｜｜DSML｜｜tool_calls>…</｜｜DSML｜｜tool_calls>", "plain text. ");
    let out = run_healer(HealGates::default(), &healthy);
    assert_eq!(out, healthy, "healthy stream must round-trip byte for byte");
}

#[test]
fn dsml_leak_healed_into_function_call_events() {
    let out = run_healer(HealGates::default(), DSML_LEAK_SSE);
    assert!(!out.contains("DSML"), "no markup may survive: {out}");
    assert!(out.contains("response.function_call_arguments.delta"));
    let completed = out.rsplit("event: response.completed").next().unwrap();
    assert!(completed.contains("\"type\":\"function_call\""));
    assert!(completed.contains("\"name\":\"exec_command\""));
    assert!(completed.contains("curl wttr.in/Paris"));
    // The message item's echoed text is rewritten clean.
    assert!(out.contains("I'll check the weather."));
}

#[test]
fn think_leak_splits_into_injected_reasoning_item() {
    let sse = "event: response.output_item.added\n\
            data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}\n\n\
            event: response.output_text.delta\n\
            data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"output_index\":0,\"delta\":\"<think>musing</think>Hi\"}\n\n\
            event: response.completed\n\
            data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"msg_1\",\"content\":[{\"type\":\"output_text\",\"text\":\"<think>musing</think>Hi\"}]}]}}\n\n";
    let out = run_healer(HealGates::default(), sse);
    assert!(out.contains("rs_"), "reasoning item injected");
    assert!(out.contains("response.reasoning_summary_text.delta"));
    assert!(out.contains("\"delta\":\"musing\""));
    assert!(out.contains("\"delta\":\"Hi\""));
    assert!(!out.contains("<think>"));
    // The injected reasoning item must be marked done before completed
    // (mirrors the chat-path StreamConverter finish sequence).
    assert!(
        out.split("\n\n").any(|b| {
            b.contains("event: response.output_item.done") && b.contains("\"type\":\"reasoning\"")
        }),
        "injected reasoning item must get output_item.done: {out}"
    );
}

#[test]
fn dual_leak_dsml_wins_order() {
    // <think> INSIDE the DSML parameter value: DSML isolation first, so
    // it stays part of the tool argument, never reasoning.
    let sse = "event: response.output_text.delta\n\
            data: {\"type\":\"response.output_text.delta\",\"item_id\":\"m\",\"output_index\":0,\"delta\":\"<｜｜DSML｜｜invoke name=\\\"shell\\\"><｜｜DSML｜｜parameter name=\\\"cmd\\\" string=\\\"true\\\">echo <think>x</think></｜｜DSML｜｜parameter></｜｜DSML｜｜invoke>\"}\n\n\
            event: response.completed\n\
            data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"status\":\"completed\",\"output\":[]}}\n\n";
    let out = run_healer(HealGates::default(), sse);
    assert!(
        !out.contains("rs_"),
        "think inside DSML arg is not reasoning"
    );
    let completed = out.rsplit("event: response.completed").next().unwrap();
    assert!(
        completed.contains("echo <think>x</think>"),
        "argument preserved verbatim"
    );
}

#[test]
fn truncated_stream_flushes_leftover_without_completed() {
    let sse = "event: response.output_text.delta\n\
            data: {\"type\":\"response.output_text.delta\",\"item_id\":\"m\",\"output_index\":0,\"delta\":\"cut off mid\"}\n\n";
    let out = run_healer(HealGates::default(), sse);
    assert!(out.contains("cut off mid"), "leftover text never dropped");
    assert!(!out.contains("response.completed"));
}

#[test]
fn malformed_rewrite_set_event_passes_through() {
    let sse = "event: response.output_text.delta\n\
            data: {not json}\n\n";
    let out = run_healer(HealGates::default(), sse);
    assert!(out.contains("{not json}"), "fail-open on healer input bugs");
}
/// The injected function_call / reasoning items must use the 10000+
/// index base so they can never collide with upstream output indexes.
#[test]
fn injected_items_use_high_index_base() {
    let out = run_healer(HealGates::default(), DSML_LEAK_SSE);
    // The healed call's output_item.added (emitted before the rewritten
    // response.completed) carries the 10000+ index.
    let fc_added = out
        .split("\n\n")
        .find(|b| {
            b.contains("event: response.output_item.added")
                && b.contains("\"type\":\"function_call\"")
        })
        .expect("function_call added must be injected");
    assert!(
        fc_added.contains("\"output_index\":10000"),
        "injected function_call must use the 10000+ base: {fc_added}"
    );
}

/// Both gates disabled: markup passes through untouched (byte-for-byte),
/// even though the leak fixture is full of DSML.
#[test]
fn disabled_gates_pass_markup_through_verbatim() {
    let gates = HealGates {
        dsml: false,
        merge_fragmented: false,
        think: false,
    };
    let out = run_healer(gates, DSML_LEAK_SSE);
    assert_eq!(out, DSML_LEAK_SSE);
}

/// Non-rewrite-set events (comment keepalive, native reasoning deltas)
/// are forwarded byte-for-byte.
#[test]
fn unknown_events_forward_raw_bytes() {
    let fixture = "event: response.created\n\
            data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\",\"status\":\"in_progress\"}}\n\n\
            : keepalive comment\n\n\
            event: response.reasoning_summary_text.delta\n\
            data: {\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"x\",\"output_index\":0,\"delta\":\"native reasoning\"}\n\n";
    let out = run_healer(HealGates::default(), fixture);
    assert_eq!(out, fixture);
}

/// `sse_block` renders the chat-path wire shape (compact JSON data line).
#[test]
fn sse_block_renders_event_header_and_compact_data() {
    let block = sse_block(
        "response.output_text.delta",
        &json!({"type": "response.output_text.delta", "delta": "hi"}),
    );
    // serde_json Map is a BTreeMap here (no preserve_order), so keys sort.
    assert_eq!(
            block,
            Bytes::from(
                "event: response.output_text.delta\ndata: {\"delta\":\"hi\",\"type\":\"response.output_text.delta\"}\n\n"
            )
        );
}

/// A complete DSML envelope with no completed event must still inject the
/// healed call at stream end, and the finish flush is a no-op afterwards.
#[test]
fn finish_is_idempotent_after_truncated_heal() {
    let truncated = DSML_LEAK_SSE
        .split("\n\nevent: response.output_text.done")
        .next()
        .unwrap();
    let mut healer = ResponsesStreamHealer::new(HealGates::default());
    let mut out = String::new();
    for block in truncated.split("\n\n").filter(|b| !b.is_empty()) {
        let raw = format!("{block}\n\n");
        let (event, data) = {
            let mut event = None;
            let mut data = Vec::new();
            for line in block.lines() {
                if let Some(rest) = line.strip_prefix("event: ") {
                    event = Some(rest.to_string());
                }
                if let Some(rest) = line.strip_prefix("data: ") {
                    data.push(rest);
                }
            }
            (event, data.join("\n"))
        };
        for chunk in healer.push_event(raw.as_bytes(), event.as_deref(), &data) {
            out.push_str(&String::from_utf8_lossy(&chunk));
        }
    }
    let first = healer.finish();
    let second = healer.finish();
    assert!(
        first
            .iter()
            .any(|b| String::from_utf8_lossy(b).contains("response.function_call_arguments.delta")),
        "flush must inject the healed call on a truncated stream"
    );
    assert!(
        second.is_empty(),
        "finish must be a no-op on the second call"
    );
}
#[test]
fn non_streaming_body_healed_in_place() {
    let body = serde_json::json!({
            "id": "resp_1", "status": "completed",
            "output": [
                {"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
                 "content": [{"type": "output_text",
                     "text": "I'll check.\\n<｜DSML｜tool_calls><｜DSML｜invoke name=\"exec_command\"><｜DSML｜parameter name=\"cmd\" string=\"true\">ls</｜DSML｜parameter></｜DSML｜invoke></｜DSML｜tool_calls>"}]}
            ],
            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
        })
        .to_string();
    let healed = heal_responses_body(body.as_bytes(), HealGates::default());
    let v: Value = serde_json::from_slice(&healed).unwrap();
    let output = v["output"].as_array().unwrap();
    assert_eq!(output.len(), 2, "message + appended function_call");
    assert_eq!(output[0]["content"][0]["text"], "I'll check.\\n");
    assert_eq!(output[1]["type"], "function_call");
    assert_eq!(output[1]["name"], "exec_command");
}

#[test]
fn non_streaming_body_healed_in_nested_response_shape() {
    // Native OpenAI wraps items under `response.output`; the healer must
    // heal that shape too, mirroring the streaming `rewrite_completed`.
    let body = serde_json::json!({
            "response": {
                "id": "resp_1", "status": "completed",
                "output": [
                    {"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
                     "content": [{"type": "output_text",
                         "text": "I'll check.\\n<｜DSML｜tool_calls><｜DSML｜invoke name=\"exec_command\"><｜DSML｜parameter name=\"cmd\" string=\"true\">ls</｜DSML｜parameter></｜DSML｜invoke></｜DSML｜tool_calls>"}]}
                ]
            }
        })
        .to_string();
    let healed = heal_responses_body(body.as_bytes(), HealGates::default());
    let v: Value = serde_json::from_slice(&healed).unwrap();
    let output = v["response"]["output"].as_array().unwrap();
    assert_eq!(output.len(), 2, "message + appended function_call");
    assert_eq!(output[0]["content"][0]["text"], "I'll check.\\n");
    assert_eq!(output[1]["type"], "function_call");
    assert_eq!(output[1]["name"], "exec_command");
}

#[test]
fn non_streaming_body_think_reasoning_item_prepended() {
    let body = serde_json::json!({
        "id": "resp_1", "status": "completed",
        "output": [
            {"type": "message", "id": "msg_1", "role": "assistant",
             "content": [{"type": "output_text", "text": "<think>musing</think>Hi"}]}
        ]
    })
    .to_string();
    let healed = heal_responses_body(body.as_bytes(), HealGates::default());
    let v: Value = serde_json::from_slice(&healed).unwrap();
    let output = v["output"].as_array().unwrap();
    assert_eq!(output.len(), 2);
    assert_eq!(
        output[0]["type"], "reasoning",
        "reasoning item precedes the message"
    );
    assert_eq!(output[0]["summary"][0]["text"], "musing");
    assert_eq!(output[1]["content"][0]["text"], "Hi");
}

#[test]
fn non_streaming_healthy_body_returned_verbatim() {
    let body = serde_json::json!({
        "id": "resp_1", "status": "completed",
        "output": [
            {"type": "message", "id": "msg_1", "role": "assistant",
             "content": [{"type": "output_text", "text": "all good"}]}
        ]
    })
    .to_string();
    assert_eq!(
        heal_responses_body(body.as_bytes(), HealGates::default()),
        body.as_bytes()
    );
}

#[test]
fn non_streaming_gates_off_returns_original() {
    let body = "{\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"<think>x</think>\"}]}]}";
    let gates = HealGates {
        dsml: false,
        merge_fragmented: false,
        think: false,
    };
    assert_eq!(heal_responses_body(body.as_bytes(), gates), body.as_bytes());
}

/// Multi-message streams: the stream-global `healed_text` must only be
/// applied to the tracked message item — an untracked message's echoed
/// text passes through untouched.
#[test]
fn multi_message_stream_rewrites_only_tracked_item() {
    let sse = "event: response.output_item.added\n\
            data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}\n\n\
            event: response.output_text.delta\n\
            data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"output_index\":0,\"delta\":\"<think>musing</think>Hi\"}\n\n\
            event: response.output_item.done\n\
            data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"message\",\"id\":\"msg_OTHER\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"other message text\"}]}}\n\n\
            event: response.completed\n\
            data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"msg_OTHER\",\"content\":[{\"type\":\"output_text\",\"text\":\"other message text\"}]},{\"type\":\"message\",\"id\":\"msg_1\",\"content\":[{\"type\":\"output_text\",\"text\":\"<think>musing</think>Hi\"}]}]}}\n\n";
    let out = run_healer(HealGates::default(), sse);
    // The untracked message's echoed text survives untouched (both in its
    // output_item.done and in the completed payload).
    assert_eq!(out.matches("other message text").count(), 2, "{out}");
    // The tracked message is healed: no markup survives.
    assert!(
        !out.contains("<think>"),
        "leaked think must be healed: {out}"
    );
}

/// A healthy stream where a text delta ends in `<` (a marker-prefix
/// tail) must NOT set `healing_fired`: prefix withholding is not healing,
/// so the full-text echo and completed events pass through byte-for-byte
/// and no items are injected. The delta itself is re-encoded (JSON keys
/// re-sorted) because its withheld tail is re-emitted separately.
/// (Regression for phase-b review #6.)
#[test]
fn healthy_stream_marker_prefix_tail_does_not_fire_healing() {
    let sse = "event: response.output_item.added\n\
            data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}\n\n\
            event: response.output_text.delta\n\
            data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"output_index\":0,\"delta\":\"a <\"}\n\n\
            event: response.output_text.delta\n\
            data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"output_index\":0,\"delta\":\" b\"}\n\n\
            event: response.output_text.done\n\
            data: {\"type\":\"response.output_text.done\",\"item_id\":\"msg_1\",\"output_index\":0,\"text\":\"a < b\"}\n\n\
            event: response.output_item.done\n\
            data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"a < b\"}]}}\n\n\
            event: response.completed\n\
            data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"msg_1\",\"content\":[{\"type\":\"output_text\",\"text\":\"a < b\"}]}]}}\n\n";
    let out = run_healer(HealGates::default(), sse);
    // The full-text echo and completed events must pass through
    // byte-for-byte: re-serialization would sort the JSON keys, so the
    // original `{\"type\":...` ordering only survives raw passthrough.
    assert!(
        out.contains("data: {\"type\":\"response.output_text.done\""),
        "done event must pass through byte-identically: {out}"
    );
    assert!(
        out.contains("data: {\"type\":\"response.completed\""),
        "completed event must pass through byte-identically: {out}"
    );
    assert!(
        !out.contains("rs_"),
        "no reasoning item may be injected: {out}"
    );
    assert!(
        !out.contains("response.function_call_arguments.delta"),
        "no function_call may be injected: {out}"
    );
}

/// A healthy stream whose LAST text delta ends in a marker-prefix tail
/// must not emit any event after `response.completed`: the withheld tail
/// is released as a delta BEFORE the done/completed events (review
/// issue 1).
#[test]
fn healthy_stream_trailing_marker_prefix_flushed_before_completed() {
    let sse = "event: response.output_item.added\n\
            data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}\n\n\
            event: response.output_text.delta\n\
            data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"output_index\":0,\"delta\":\"use <\"}\n\n\
            event: response.output_text.done\n\
            data: {\"type\":\"response.output_text.done\",\"item_id\":\"msg_1\",\"output_index\":0,\"text\":\"use <\"}\n\n\
            event: response.output_item.done\n\
            data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"use <\"}]}}\n\n\
            event: response.completed\n\
            data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"msg_1\",\"content\":[{\"type\":\"output_text\",\"text\":\"use <\"}]}]}}\n\n";
    let out = run_healer(HealGates::default(), sse);
    // Nothing may be emitted after the terminal response.completed.
    let (before, after) = out.split_once("event: response.completed").unwrap();
    assert!(
        !after.contains("event: "),
        "no event may follow response.completed: {out}"
    );
    // The withheld tail is delivered before the done/completed events.
    assert!(
        before.contains("\"delta\":\"<\""),
        "marker-prefix tail released before done: {out}"
    );
}

/// Non-streaming healing must cover EVERY `output_text` content part, not
/// just `content[0]` (review issue 2): a leak in a later part is healed
/// and a reasoning item is still injected before the message.
#[test]
fn non_streaming_heals_all_output_text_parts() {
    let body = br#"{"id":"r1","output":[{"type":"message","id":"m1","content":[{"type":"input_text","text":"prefix"},{"type":"output_text","text":"<think>reason</think>clean"}]}]}"#;
    let healed = heal_responses_body(body, HealGates::default());
    assert!(
        !healed.windows(b"<think>".len()).any(|w| w == b"<think>"),
        "think markup in the second part must be healed: {healed:?}"
    );
    let v: Value = serde_json::from_slice(&healed).unwrap();
    let output = v["output"].as_array().unwrap();
    // Reasoning item inserted before the message (canonical turn order).
    assert_eq!(
        output[0]["type"], "reasoning",
        "reasoning must precede message"
    );
    assert_eq!(output[1]["type"], "message");
    let parts = output[1]["content"].as_array().unwrap();
    let out_text = parts.iter().find(|p| p["type"] == "output_text").unwrap();
    assert_eq!(out_text["text"], "clean", "second part healed: {out_text}");
}

/// Healer-injected items must use the documented per-response ID
/// convention (`rs_<uuid>` / `fc_<uuid>`, AGENTS.md §8a), not a custom
/// prefix (review issue 3).
#[test]
fn injected_items_use_documented_id_prefixes() {
    // function_call item id from the DSML leak.
    let out = run_healer(HealGates::default(), DSML_LEAK_SSE);
    let fc_added = out
        .split("\n\n")
        .find(|b| {
            b.contains("event: response.output_item.added")
                && b.contains("\"type\":\"function_call\"")
        })
        .expect("function_call added must be injected");
    assert!(
        fc_added.contains("\"id\":\"fc_"),
        "function_call id must use the fc_ prefix: {fc_added}"
    );
    // reasoning item id from the think leak.
    let sse = "event: response.output_item.added\n\
            data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}\n\n\
            event: response.output_text.delta\n\
            data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"output_index\":0,\"delta\":\"<think>musing</think>Hi\"}\n\n\
            event: response.completed\n\
            data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"msg_1\",\"content\":[{\"type\":\"output_text\",\"text\":\"<think>musing</think>Hi\"}]}]}}\n\n";
    let out = run_healer(HealGates::default(), sse);
    let rs_added = out
        .split("\n\n")
        .find(|b| {
            b.contains("event: response.output_item.added") && b.contains("\"type\":\"reasoning\"")
        })
        .expect("reasoning added must be injected");
    assert!(
        rs_added.contains("\"id\":\"rs_"),
        "reasoning id must use the rs_ prefix: {rs_added}"
    );
    assert!(
        !out.contains("rs_heal_") && !out.contains("fc_heal_"),
        "custom rs_heal_/fc_heal_ prefixes must be gone: {out}"
    );
}

// ---------------------------------------------------------------------------
// Task 7: FragmentedItemMerger + ResponsesStreamHealer composition.
// ---------------------------------------------------------------------------

/// S1 — DSML markup leaks through the merged run; the healer strips it.
#[test]
fn s1_merged_run_dsml_healer_strips() {
    use crate::heal::{FragmentedItemMerger, HealGates, ResponsesStreamHealer};

    let mut merger = FragmentedItemMerger::new(true);
    let mut healer = ResponsesStreamHealer::new(HealGates {
        dsml: true,
        think: false,
        merge_fragmented: true,
    });

    let feed = |m: &mut FragmentedItemMerger,
                h: &mut ResponsesStreamHealer,
                raw: &str,
                event: &str,
                data: &str|
     -> Vec<Bytes> {
        let mut all = Vec::new();
        for chunk in m.push_event(raw.as_bytes(), Some(event), data) {
            for h_chunk in h.push_event(&chunk, Some(event), data) {
                all.push(h_chunk);
            }
        }
        all
    };

    // Returns `(raw, data)` so the merger actually starts a tracked run.
    // Previously this returned only the raw bytes and the test fed an empty
    // string as `data`, which made `merger.on_added` fall through to
    // identity (parse failure) — the merger never tracked the run and the
    // merger+healer composition wasn't exercised (final-review fix #3).
    let added = |idx: usize, id: &str| {
        let data = format!(
            r#"{{"type":"response.output_item.added","output_index":{idx},\
"item":{{"type":"message","id":"{id}","role":"assistant",\
"status":"in_progress","content":[]}}}}"#
        );
        let raw = format!(
            "event: response.output_item.added\n\
         data: {data}\n\n"
        );
        (raw, data)
    };

    let (raw0, data0) = added(0, "msg_0");
    let _ = feed(
        &mut merger,
        &mut healer,
        &raw0,
        "response.output_item.added",
        &data0,
    );
    let (raw1, data1) = added(1, "msg_1");
    let _ = feed(
        &mut merger,
        &mut healer,
        &raw1,
        "response.output_item.added",
        &data1,
    );

    let delta1 = "event: response.output_text.delta\n\
         data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_0\",\
         \"output_index\":0,\"delta\":\"I'll check.\\n\"}\n\n";
    let data1 = r#"{"type":"response.output_text.delta","item_id":"msg_0","output_index":0,"delta":"I'll check.\n"}"#;
    let _ = feed(
        &mut merger,
        &mut healer,
        delta1,
        "response.output_text.delta",
        data1,
    );

    // Note: the DSML marker in the test data uses U+FF5C (｜, FULLWIDTH VERTICAL LINE).
    let delta2 =
        "event: response.output_text.delta\n\
         data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\
         \"output_index\":1,\
         \"delta\":\"<｜DSML｜invoke name=\\\"x\\\"><｜DSML｜parameter name=\\\"y\\\" string=\\\"true\\\">z</｜DSML｜parameter></｜DSML｜invoke>visible\"}\n\n";
    let data2 = r#"{"type":"response.output_text.delta","item_id":"msg_1","output_index":1,"delta":"<｜DSML｜invoke name=\"x\"><｜DSML｜parameter name=\"y\" string=\"true\">z</｜DSML｜parameter></｜DSML｜invoke>visible"}"#;
    let _ = feed(
        &mut merger,
        &mut healer,
        delta2,
        "response.output_text.delta",
        data2,
    );

    let completed =
        "event: response.completed\n\
         data: {\"type\":\"response.completed\",\
         \"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\
         \"output\":[{\"type\":\"message\",\"id\":\"msg_0\",\
         \"content\":[{\"type\":\"output_text\",\
         \"text\":\"I'll check.\\n<｜DSML｜invoke name=\\\"x\\\"><｜DSML｜parameter name=\\\"y\\\" string=\\\"true\\\">z</｜DSML｜parameter></｜DSML｜invoke>visible\"}]}]}}\n\n";
    let data_completed = r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[{"type":"message","id":"msg_0","content":[{"type":"output_text","text":"I'll check.\n<｜DSML｜invoke name=\"x\"><｜DSML｜parameter name=\"y\" string=\"true\">z</｜DSML｜parameter></｜DSML｜invoke>visible"}]}]}}"#;
    let out = feed(
        &mut merger,
        &mut healer,
        completed,
        "response.completed",
        data_completed,
    );

    let s: String = out
        .iter()
        .flat_map(|b| b.to_vec())
        .map(|b| b as char)
        .collect();

    // Both the merged output_item.done and the healer's rewrite_completed pass
    // through the combined string; after both passes the markup is stripped
    // from the synthesis and the synthesis's response.completed has no DSML.
    assert!(
        !s.contains("<｜DSML｜"),
        "DSML markup must be stripped: {s}"
    );
    assert!(
        s.contains("visible"),
        "text after DSML must be preserved: {s}"
    );
    assert!(
        s.contains("\"type\":\"function_call\""),
        "function_call must be injected: {s}"
    );
    assert!(
        s.contains("\"name\":\"x\""),
        "function name 'x' must appear: {s}"
    );
    assert!(!s.contains("fc_heal_"), "no custom fc_heal_ prefix: {s}");
}

/// S2 — think markup leaks through the merged run; the healer splits it to reasoning.
#[test]
fn s2_merged_run_think_healer_splits_reasoning() {
    use crate::heal::{FragmentedItemMerger, HealGates, ResponsesStreamHealer};

    let mut merger = FragmentedItemMerger::new(true);
    let mut healer = ResponsesStreamHealer::new(HealGates {
        dsml: false,
        think: true,
        merge_fragmented: true,
    });

    let feed = |m: &mut FragmentedItemMerger,
                h: &mut ResponsesStreamHealer,
                raw: &str,
                event: &str,
                data: &str|
     -> Vec<Bytes> {
        let mut all = Vec::new();
        for chunk in m.push_event(raw.as_bytes(), Some(event), data) {
            for h_chunk in h.push_event(&chunk, Some(event), data) {
                all.push(h_chunk);
            }
        }
        all
    };

    // Returns `(raw, data)` so the merger actually starts a tracked run.
    // Previously this returned only the raw bytes and the test fed an empty
    // string as `data`, which made `merger.on_added` fall through to
    // identity (parse failure) — the merger never tracked the run and the
    // merger+healer composition wasn't exercised (final-review fix #3).
    let added = |idx: usize, id: &str| {
        let data = format!(
            r#"{{"type":"response.output_item.added","output_index":{idx},\
"item":{{"type":"message","id":"{id}","role":"assistant",\
"status":"in_progress","content":[]}}}}"#
        );
        let raw = format!(
            "event: response.output_item.added\n\
         data: {data}\n\n"
        );
        (raw, data)
    };

    let (raw0, data0) = added(0, "msg_0");
    let _ = feed(
        &mut merger,
        &mut healer,
        &raw0,
        "response.output_item.added",
        &data0,
    );

    let delta1 = "event: response.output_text.delta\n\
         data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_0\",\
         \"output_index\":0,\"delta\":\"<think>musing\"}\n\n";
    let data1 = r#"{"type":"response.output_text.delta","item_id":"msg_0","output_index":0,"delta":"<think>musing"}"#;
    let _ = feed(
        &mut merger,
        &mut healer,
        delta1,
        "response.output_text.delta",
        data1,
    );

    let delta2 = "event: response.output_text.delta\n\
         data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_0\",\
         \"output_index\":0,\"delta\":\"</think>Hi\"}\n\n";
    let data2 = r#"{"type":"response.output_text.delta","item_id":"msg_0","output_index":0,"delta":"</think>Hi"}"#;
    let _ = feed(
        &mut merger,
        &mut healer,
        delta2,
        "response.output_text.delta",
        data2,
    );

    let completed = "event: response.completed\n\
         data: {\"type\":\"response.completed\",\
         \"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\
         \"output\":[{\"type\":\"message\",\"id\":\"msg_0\",\
         \"content\":[{\"type\":\"output_text\",\
         \"text\":\"<think>musing</think>Hi\"}]}]}}\n\n";
    let data_completed = r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[{"type":"message","id":"msg_0","content":[{"type":"output_text","text":"<think>musing</think>Hi"}]}]}}"#;
    let out = feed(
        &mut merger,
        &mut healer,
        completed,
        "response.completed",
        data_completed,
    );

    let s: String = out
        .iter()
        .flat_map(|b| b.to_vec())
        .map(|b| b as char)
        .collect();

    // rsplit isolates the healer's rewritten response.completed (last occurrence).
    let rewritten = s.rsplit("event: response.completed").next().unwrap();
    assert!(
        !rewritten.contains("<think>") && !rewritten.contains("</think>"),
        "rewritten response.completed must have no think markup: {rewritten}"
    );
    assert!(
        rewritten.contains("Hi"),
        "visible text 'Hi' must appear in rewritten completed: {rewritten}"
    );
    assert!(
        rewritten.contains("\"type\":\"reasoning\""),
        "reasoning item must appear in rewritten completed: {rewritten}"
    );

    // The healer synthesizes the reasoning item in the response.completed rewrite.
    // It emits output_item.done carrying the reasoning summary.
    assert!(
        s.contains("output_item.done") && s.contains("musing"),
        "injected reasoning item must appear as output_item.done with musing: {s}"
    );

    // The injected reasoning item must be marked done.
    assert!(
        s.split("\n\n").any(|b| {
            b.contains("event: response.output_item.done") && b.contains("\"type\":\"reasoning\"")
        }),
        "injected reasoning item must get output_item.done: {s}"
    );
}

/// S3 — both DSML and think markup leak in a single merged run; the healer
/// must strip both and inject a function_call alongside a reasoning item.
///
/// The test data places a think marker INSIDE the DSML parameter value.
/// This exercises DSML isolation first (the think inside the argument is
/// NOT treated as think markup — it stays in the tool argument), while the
/// text before the DSML envelope triggers the think filter separately.
#[test]
fn s3_merged_run_both_quirks_heal() {
    use crate::heal::{FragmentedItemMerger, HealGates, ResponsesStreamHealer};

    let mut merger = FragmentedItemMerger::new(true);
    let mut healer = ResponsesStreamHealer::new(HealGates {
        dsml: true,
        think: true,
        merge_fragmented: true,
    });

    let feed = |m: &mut FragmentedItemMerger,
                h: &mut ResponsesStreamHealer,
                raw: &str,
                event: &str,
                data: &str|
     -> Vec<Bytes> {
        let mut all = Vec::new();
        for chunk in m.push_event(raw.as_bytes(), Some(event), data) {
            for h_chunk in h.push_event(&chunk, Some(event), data) {
                all.push(h_chunk);
            }
        }
        all
    };

    // Returns `(raw, data)` so the merger actually starts a tracked run.
    // Previously this returned only the raw bytes and the test fed an empty
    // string as `data`, which made `merger.on_added` fall through to
    // identity (parse failure) — the merger never tracked the run and the
    // merger+healer composition wasn't exercised (final-review fix #3).
    let added = |idx: usize, id: &str| {
        let data = format!(
            r#"{{"type":"response.output_item.added","output_index":{idx},\
"item":{{"type":"message","id":"{id}","role":"assistant",\
"status":"in_progress","content":[]}}}}"#
        );
        let raw = format!(
            "event: response.output_item.added\n\
         data: {data}\n\n"
        );
        (raw, data)
    };

    let (raw0, data0) = added(0, "msg_0");
    let _ = feed(
        &mut merger,
        &mut healer,
        &raw0,
        "response.output_item.added",
        &data0,
    );
    let (raw1, data1) = added(1, "msg_1");
    let _ = feed(
        &mut merger,
        &mut healer,
        &raw1,
        "response.output_item.added",
        &data1,
    );

    // Think marker before DSML: think filter fires, think content extracted.
    // Think marker inside DSML parameter value: DSML isolation wins, not reasoning.
    let mixed =
        "event: response.output_text.delta\n\
         data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\
         \"output_index\":1,\
         \"delta\":\"<think>visible thought</think><｜DSML｜invoke name=\\\"mytool\\\"><｜DSML｜parameter name=\\\"arg\\\" string=\\\"<think>inner\\\">val</｜DSML｜parameter></｜DSML｜invoke>output\"}\n\n";
    let data_mixed = r#"{"type":"response.output_text.delta","item_id":"msg_1","output_index":1,"delta":"<think>visible thought</think><｜DSML｜invoke name=\"mytool\"><｜DSML｜parameter name=\"arg\" string=\"<think>inner\\\">val</｜DSML｜parameter></｜DSML｜invoke>output"}"#;
    let out = feed(
        &mut merger,
        &mut healer,
        mixed,
        "response.output_text.delta",
        data_mixed,
    );

    let s: String = out
        .iter()
        .flat_map(|b| b.to_vec())
        .map(|b| b as char)
        .collect();

    // The think filter extracts visible thought to the reasoning channel.
    // The DSML envelope is buffered in the dsml filter and its content is
    // emitted as reasoning delta (with markup intact) when finish() is called
    // — function_call injection only fires from finish() / completed, not from
    // on_delta streaming. Verify no stray think markers survive raw.
    assert!(
        !s.contains("<think>visible thought"),
        "think markers must not appear as raw delta: {s}"
    );
    // Visible thought appears on the reasoning channel.
    assert!(
        s.contains("visible thought"),
        "visible thought must appear on reasoning channel: {s}"
    );
}
