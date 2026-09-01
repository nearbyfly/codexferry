//! Unit tests for [`crate::heal::FragmentedItemMerger`].
//!
//! Spec fixture IDs (docs/superpowers/specs/2026-08-30-fragmented-items-merger-design.md §Testing):
//! M1/M3/M5/K1 land in Task 2; M2/M4/M6/M7 in Task 3; M8/M9 in Task 4;
//! W1–W5 in Task 5; E1–E4 in Task 6; S1–S3 in Task 7.

use crate::heal::FragmentedItemMerger;
use bytes::Bytes;

/// Build a single `event: <event>\ndata: <data>\n\n` SSE block from
/// `event` and a JSON-shaped `data` string.
fn sse(event: &str, data: &str) -> Vec<u8> {
    format!("event: {event}\ndata: {data}\n\n").into_bytes()
}

fn push_raw(merger: &mut FragmentedItemMerger, event_block: &[u8]) -> Vec<Bytes> {
    // Parse the event name out of the block so push_event's `_event` arg
    // is correct in fixtures where the merger later starts reading it.
    let text = std::str::from_utf8(event_block).unwrap();
    let event_name = text
        .lines()
        .find_map(|l| l.strip_prefix("event: ").map(str::to_string));
    let data = text
        .lines()
        .find_map(|l| l.strip_prefix("data: ").map(str::to_string))
        .unwrap_or_default();
    merger.push_event(event_block, event_name.as_deref(), &data)
}

fn concat(out: Vec<Bytes>) -> Vec<u8> {
    out.into_iter().flat_map(|b| b.to_vec()).collect()
}

/// Spec M1: a single message item (healthy stream, run length = 1)
/// must pass through verbatim and trigger no merge behavior.
#[test]
fn m1_single_message_passthrough() {
    let mut m = FragmentedItemMerger::new(true);
    let raw = sse(
        "response.output_item.added",
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_0","role":"assistant","status":"in_progress","content":[]}}"#,
    );
    let out = push_raw(&mut m, &raw);
    assert_eq!(concat(out), raw);
}

/// Spec M3: a single reasoning item passes through verbatim.
#[test]
fn m3_single_reasoning_passthrough() {
    let mut m = FragmentedItemMerger::new(true);
    let raw = sse(
        "response.output_item.added",
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_0","summary":[{"type":"summary_text","text":""}]}}"#,
    );
    let out = push_raw(&mut m, &raw);
    assert_eq!(concat(out), raw);
}

/// Spec M5: a single function_call item passes through verbatim.
#[test]
fn m5_single_function_call_passthrough() {
    let mut m = FragmentedItemMerger::new(true);
    let raw = sse(
        "response.output_item.added",
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_0","call_id":"call_0","name":"shell","arguments":"","status":"in_progress"}}"#,
    );
    let out = push_raw(&mut m, &raw);
    assert_eq!(concat(out), raw);
}

/// Spec K1: when the `merge_fragmented` quirk is disabled, the merger
/// is an identity passthrough — same posture as
/// `DsmlStreamFilter::new(false)`. (The cross-quirk use case: gate off
/// here, but a sibling quirk like `dsml_heal` still wants every event on
/// the wire; the merger must not drop them.)
#[test]
fn k1_disabled_passes_through_verbatim() {
    let mut m = FragmentedItemMerger::new(false);
    let raw = sse(
        "response.output_item.added",
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_0","role":"assistant","status":"in_progress","content":[]}}"#,
    );
    let out = push_raw(&mut m, &raw);
    assert_eq!(
        concat(out),
        raw,
        "disabled merger must pass through verbatim"
    );
}

/// Spec M2: N consecutive message fragments merge into a single item.
/// The first fragment's `output_item.added` passes through verbatim;
/// the rest are suppressed. (Subsequent deltas + done rewriting land in
/// Task 5; this fixture only asserts the added-event suppression.)
#[test]
fn m2_message_run_suppresses_subsequent_added() {
    let mut m = FragmentedItemMerger::new(true);
    let added = |id: &str, idx: u64| {
        sse(
            "response.output_item.added",
            &format!(
                r#"{{"type":"response.output_item.added","output_index":{idx},"item":{{"type":"message","id":"{id}","role":"assistant","status":"in_progress","content":[]}}}}"#
            ),
        )
    };
    let a0 = added("msg_0", 0);
    let a1 = added("msg_9", 1);
    let a2 = added("msg_10", 2);
    let out0 = push_raw(&mut m, &a0);
    let out1 = push_raw(&mut m, &a1);
    let out2 = push_raw(&mut m, &a2);
    assert_eq!(concat(out0), a0, "first fragment passes through");
    assert!(out1.is_empty(), "second fragment suppressed");
    assert!(out2.is_empty(), "third fragment suppressed");
}

/// Spec M4: N consecutive reasoning fragments merge.
#[test]
fn m4_reasoning_run_suppresses_subsequent_added() {
    let mut m = FragmentedItemMerger::new(true);
    let added = |id: &str, idx: u64| {
        sse(
            "response.output_item.added",
            &format!(
                r#"{{"type":"response.output_item.added","output_index":{idx},"item":{{"type":"reasoning","id":"{id}","summary":[{{"type":"summary_text","text":""}}]}}}}"#
            ),
        )
    };
    let a0 = added("rs_0", 0);
    let a1 = added("rs_1", 1);
    let out0 = push_raw(&mut m, &a0);
    let out1 = push_raw(&mut m, &a1);
    assert_eq!(concat(out0), a0);
    assert!(out1.is_empty());
}

/// Spec M6: N consecutive function_call fragments with the same
/// `call_id` merge (same logical call split across items — Responses
/// contract violation by the upstream).
#[test]
fn m6_function_call_same_call_id_merges() {
    let mut m = FragmentedItemMerger::new(true);
    let added = |idx: u64| {
        sse(
            "response.output_item.added",
            &format!(
                r#"{{"type":"response.output_item.added","output_index":{idx},"item":{{"type":"function_call","id":"fc_{idx}","call_id":"call_shared","name":"shell","arguments":"","status":"in_progress"}}}}"#
            ),
        )
    };
    let out0 = push_raw(&mut m, &added(0));
    let out1 = push_raw(&mut m, &added(1));
    assert_eq!(concat(out0), added(0));
    assert!(
        out1.is_empty(),
        "same call_id → merge (suppress second added)"
    );
}

/// Spec M7: function_call fragments with DIFFERENT call_ids must NOT
/// merge — they are independent tool calls.
#[test]
fn m7_function_call_different_call_ids_dont_merge() {
    let mut m = FragmentedItemMerger::new(true);
    let added = |idx: u64, cid: &str| {
        sse(
            "response.output_item.added",
            &format!(
                r#"{{"type":"response.output_item.added","output_index":{idx},"item":{{"type":"function_call","id":"fc_{idx}","call_id":"{cid}","name":"shell","arguments":"","status":"in_progress"}}}}"#
            ),
        )
    };
    let out0 = push_raw(&mut m, &added(0, "call_a"));
    let out1 = push_raw(&mut m, &added(1, "call_b"));
    assert_eq!(concat(out0), added(0, "call_a"));
    // Different call_id → tracked as a new run, second added passes through
    // (run length just became 1 again; the first was discarded because
    // length=1 had no merge). Task 4 handles type switches; for type-same
    // but id-different, the second added passes through verbatim.
    assert_eq!(concat(out1), added(1, "call_b"));
}

/// Spec M8: alternating message / reasoning / function_call items
/// each pass through as their own item (type-switch boundary).
#[test]
fn m8_type_switches_each_pass_through() {
    let mut m = FragmentedItemMerger::new(true);
    let msg_added = || {
        sse(
            "response.output_item.added",
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_0","role":"assistant","status":"in_progress","content":[]}}"#,
        )
    };
    let rs_added = || {
        sse(
            "response.output_item.added",
            r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"reasoning","id":"rs_0","summary":[{"type":"summary_text","text":""}]}}"#,
        )
    };
    let fc_added = || {
        sse(
            "response.output_item.added",
            r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"fc_0","call_id":"c0","name":"shell","arguments":"","status":"in_progress"}}"#,
        )
    };
    let a = push_raw(&mut m, &msg_added());
    let b = push_raw(&mut m, &rs_added());
    let c = push_raw(&mut m, &fc_added());
    assert_eq!(concat(a), msg_added());
    assert_eq!(concat(b), rs_added());
    assert_eq!(concat(c), fc_added());
}

/// Spec M9: interleaved runs (msg×N → reasoning×1 → msg×M) — each
/// run is independent; reasoning item is its own item.
#[test]
fn m9_interleaved_runs_each_merge_independently() {
    let mut m = FragmentedItemMerger::new(true);
    let msg = |idx: u64| {
        sse(
            "response.output_item.added",
            &format!(
                r#"{{"type":"response.output_item.added","output_index":{idx},"item":{{"type":"message","id":"msg_{idx}","role":"assistant","status":"in_progress","content":[]}}}}"#
            ),
        )
    };
    let rs = sse(
        "response.output_item.added",
        r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"reasoning","id":"rs_0","summary":[{"type":"summary_text","text":""}]}}"#,
    );
    // First msg run (idx 0, 1) merges.
    let out0 = push_raw(&mut m, &msg(0));
    let out1 = push_raw(&mut m, &msg(1));
    // Reasoning item — type switch; the prior msg run had length=2 but
    // Task 3 doesn't flush yet (synthesis is in Task 5). For this
    // fixture, only assert that the reasoning passes through and the
    // subsequent msg run starts a fresh tracked item.
    let out_rs = push_raw(&mut m, &rs);
    let out3 = push_raw(&mut m, &msg(3));
    let out4 = push_raw(&mut m, &msg(4));
    assert_eq!(concat(out0), msg(0));
    assert!(
        out1.is_empty(),
        "second msg fragment suppressed (run in progress)"
    );
    assert_eq!(
        concat(out_rs),
        rs,
        "reasoning item unaffected by type switch"
    );
    assert_eq!(
        concat(out3),
        msg(3),
        "second msg run starts fresh (passes through)"
    );
    assert!(out4.is_empty(), "second msg run's 2nd fragment suppressed");
}

/// Spec W1: subsequent-fragment deltas have `item_id` and
/// `output_index` rewritten to the first fragment's.
#[test]
fn w1_subsequent_deltas_rewritten_to_first_fragment() {
    let mut m = FragmentedItemMerger::new(true);
    let msg = |idx: u64, id: &str| {
        sse(
            "response.output_item.added",
            &format!(
                r#"{{"type":"response.output_item.added","output_index":{idx},"item":{{"type":"message","id":"{id}","role":"assistant","status":"in_progress","content":[]}}}}"#
            ),
        )
    };
    let delta = |item_id: &str, idx: u64, text: &str| {
        sse(
            "response.output_text.delta",
            &format!(
                r#"{{"type":"response.output_text.delta","item_id":"{item_id}","output_index":{idx},"delta":"{text}"}}"#
            ),
        )
    };
    let _ = push_raw(&mut m, &msg(0, "msg_0"));
    let _ = push_raw(&mut m, &msg(1, "msg_9")); // suppress; run length ≥ 2
    let _d1 = push_raw(&mut m, &delta("msg_0", 0, "Hello "));
    let d2 = push_raw(&mut m, &delta("msg_9", 1, "world")); // rewrite to msg_0, idx 0
                                                            // d1 is identity (msg_0's own delta)
    let d2_bytes = concat(d2);
    let d2_str = std::str::from_utf8(&d2_bytes).unwrap();
    assert!(
        d2_str.contains(r#""item_id":"msg_0""#),
        "d2 item_id rewritten: {d2_str}"
    );
    assert!(
        d2_str.contains(r#""output_index":0"#),
        "d2 output_index rewritten: {d2_str}"
    );
    assert!(
        d2_str.contains(r#""delta":"world""#),
        "d2 text unchanged: {d2_str}"
    );
}

/// Spec W2: subsequent `content_part.added` from later fragments is
/// suppressed (the first fragment's already emitted).
#[test]
fn w2_subsequent_content_part_added_suppressed() {
    let mut m = FragmentedItemMerger::new(true);
    let msg = |id: &str| {
        sse(
            "response.output_item.added",
            &format!(
                r#"{{"type":"response.output_item.added","output_index":0,"item":{{"type":"message","id":"{id}","role":"assistant","status":"in_progress","content":[]}}}}"#
            ),
        )
    };
    let part = |item_id: &str| {
        sse(
            "response.content_part.added",
            &format!(
                r#"{{"type":"response.content_part.added","item_id":"{item_id}","output_index":0,"part":{{"type":"output_text","text":""}}}}"#
            ),
        )
    };
    let _ = push_raw(&mut m, &msg("msg_0"));
    let _ = push_raw(&mut m, &msg("msg_1"));
    let p0 = push_raw(&mut m, &part("msg_0")); // first: pass through
    let p1 = push_raw(&mut m, &part("msg_1")); // second: suppress
    assert!(!p0.is_empty());
    assert!(p1.is_empty(), "subsequent content_part.added suppressed");
}

/// Spec W3: subsequent-fragment `output_item.done` and `content_part.done`
/// are suppressed when the run has accumulated content (merged text non-empty).
/// Length-1 runs with no merged content pass done through unchanged (Task 6
/// tighten). This fixture adds deltas so merged_text is non-empty.
#[test]
fn w3_subsequent_dones_suppressed() {
    let mut m = FragmentedItemMerger::new(true);
    let msg = |id: &str| {
        sse(
            "response.output_item.added",
            &format!(
                r#"{{"type":"response.output_item.added","output_index":0,"item":{{"type":"message","id":"{id}","role":"assistant","status":"in_progress","content":[]}}}}"#
            ),
        )
    };
    let delta = |text: &str| {
        sse(
            "response.output_text.delta",
            &format!(
                r#"{{"type":"response.output_text.delta","item_id":"msg_0","output_index":0,"delta":"{}"}}"#,
                text
            ),
        )
    };
    let cpd = |item_id: &str| {
        sse(
            "response.content_part.done",
            &format!(
                r#"{{"type":"response.content_part.done","item_id":"{item_id}","output_index":0}}"#
            ),
        )
    };
    let oid = |item_id: &str| {
        sse(
            "response.output_item.done",
            &format!(
                r#"{{"type":"response.output_item.done","item_id":"{item_id}","output_index":0,"item":{{"type":"message","id":"{item_id}","status":"completed"}}}}"#
            ),
        )
    };
    let _ = push_raw(&mut m, &msg("msg_0"));
    let _ = push_raw(&mut m, &msg("msg_1")); // run length >= 2
                                             // Feed deltas so merged_text is non-empty; then done is suppressed.
    let _ = push_raw(&mut m, &delta("hello"));
    let _ = push_raw(&mut m, &delta("world"));
    let cpd_out = push_raw(&mut m, &cpd("msg_1"));
    let oid_out = push_raw(&mut m, &oid("msg_1"));
    assert!(
        cpd_out.is_empty(),
        "msg_1's content_part.done suppressed (merged)"
    );
    assert!(
        oid_out.is_empty(),
        "msg_1's output_item.done suppressed (merged)"
    );
}

/// Spec W4: when a type switch flushes the run, the synthesized
/// `content_part.done` carries the merged text accumulated from the run.
#[test]
fn w4_synthesized_content_part_done_carries_merged_text() {
    let mut m = FragmentedItemMerger::new(true);
    let msg = |id: &str| {
        sse(
            "response.output_item.added",
            &format!(
                r#"{{"type":"response.output_item.added","output_index":0,"item":{{"type":"message","id":"{id}","role":"assistant","status":"in_progress","content":[]}}}}"#
            ),
        )
    };
    let delta = |item_id: &str, text: &str| {
        sse(
            "response.output_text.delta",
            &format!(
                r#"{{"type":"response.output_text.delta","item_id":"{item_id}","output_index":0,"delta":"{text}"}}"#
            ),
        )
    };
    let rs = sse(
        "response.output_item.added",
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"reasoning","id":"rs_0","summary":[{"type":"summary_text","text":""}]}}"#,
    );
    let _ = push_raw(&mut m, &msg("msg_0"));
    let _ = push_raw(&mut m, &msg("msg_1")); // suppress; merge mode
    let _ = push_raw(&mut m, &delta("msg_0", "Hi "));
    let _ = push_raw(&mut m, &delta("msg_1", "there")); // rewrite
                                                        // Type switch to reasoning triggers run flush.
    let rs_out = push_raw(&mut m, &rs);
    let all: Vec<u8> = rs_out.into_iter().flat_map(|b| b.to_vec()).collect();
    let all_str = std::str::from_utf8(&all).unwrap();
    assert!(
        all_str.contains("response.content_part.done"),
        "synthesized cpd present: {all_str}"
    );
    assert!(
        all_str.contains(r#""text":"Hi there""#),
        "merged text 'Hi there' present: {all_str}"
    );
}

/// Spec W5: the synthesized `output_item.done` item content has the
/// merged text.
#[test]
fn w5_synthesized_output_item_done_has_merged_content() {
    let mut m = FragmentedItemMerger::new(true);
    let msg = |id: &str| {
        sse(
            "response.output_item.added",
            &format!(
                r#"{{"type":"response.output_item.added","output_index":0,"item":{{"type":"message","id":"{id}","role":"assistant","status":"in_progress","content":[]}}}}"#
            ),
        )
    };
    let delta = |item_id: &str, text: &str| {
        sse(
            "response.output_text.delta",
            &format!(
                r#"{{"type":"response.output_text.delta","item_id":"{item_id}","output_index":0,"delta":"{text}"}}"#
            ),
        )
    };
    let rs = sse(
        "response.output_item.added",
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"reasoning","id":"rs_0","summary":[{"type":"summary_text","text":""}]}}"#,
    );
    let _ = push_raw(&mut m, &msg("msg_0"));
    let _ = push_raw(&mut m, &msg("msg_1"));
    let _ = push_raw(&mut m, &delta("msg_0", "abc"));
    let _ = push_raw(&mut m, &delta("msg_1", "def"));
    let rs_out = push_raw(&mut m, &rs);
    let all: Vec<u8> = rs_out.into_iter().flat_map(|b| b.to_vec()).collect();
    let all_str = std::str::from_utf8(&all).unwrap();
    assert!(
        all_str.contains("response.output_item.done"),
        "synthesized oid present: {all_str}"
    );
    assert!(
        all_str.contains(r#""text":"abcdef""#),
        "merged content 'abcdef' present: {all_str}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Task 6: Boundaries and failure modes (E1–E4)
// ─────────────────────────────────────────────────────────────────────────────

/// Spec E1: a 14-fragment run (the MiniMax M3 worst case from
/// NOTES-2026-08-28 §2) merges cleanly.
#[test]
fn e1_fourteen_fragment_run_merges() {
    let mut m = FragmentedItemMerger::new(true);
    let msg = |idx: u64| {
        sse(
            "response.output_item.added",
            &format!(
                "{{\"type\":\"response.output_item.added\",\"output_index\":{},\"item\":{{\"type\":\"message\",\"id\":\"msg_{}\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}}}",
                idx, idx
            ),
        )
    };
    let mut emitted = 0;
    for i in 0..14 {
        let out = push_raw(&mut m, &msg(i));
        if !out.is_empty() {
            emitted += 1;
        }
    }
    assert_eq!(
        emitted, 1,
        "only the first fragment passes through, 13 suppressed"
    );
}

/// Spec E2: a message run that switches to function_call flushes the
/// synthesized dones BEFORE the function_call item is emitted.
#[test]
fn e2_run_flushes_done_before_type_switch() {
    let mut m = FragmentedItemMerger::new(true);
    // Distinct output_index for each message item so the run accumulates
    // two separate added events before the type switch. Deltas are fed so
    // the run has merged text to synthesize a done event.
    let msg = |idx: u64, id: &str| {
        sse(
            "response.output_item.added",
            &format!(
                "{{\"type\":\"response.output_item.added\",\"output_index\":{},\"item\":{{\"type\":\"message\",\"id\":\"{}\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}}}",
                idx, id
            ),
        )
    };
    let fc = sse(
        "response.output_item.added",
        r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"fc_0","call_id":"c0","name":"shell","arguments":"","status":"in_progress"}}"#,
    );
    let delta = |text: &str| {
        sse(
            "response.output_text.delta",
            &format!(
                "{{\"type\":\"response.output_text.delta\",\"item_id\":\"msg_0\",\"output_index\":0,\"delta\":\"{}\"}}",
                text
            ),
        )
    };
    let _ = push_raw(&mut m, &msg(0, "msg_0"));
    let _ = push_raw(&mut m, &msg(1, "msg_1")); // suppress: same type, same run
    let _ = push_raw(&mut m, &delta("hello"));
    let _ = push_raw(&mut m, &delta("world"));
    let fc_out = push_raw(&mut m, &fc);
    let all: Vec<u8> = fc_out.into_iter().flat_map(|b| b.to_vec()).collect();
    let s = std::str::from_utf8(&all).unwrap();
    // Synthesized done should appear before the function_call's added.
    let done_pos = s.find("response.output_item.done").unwrap_or(usize::MAX);
    let fc_pos = s.find(r#""type":"function_call""#).unwrap_or(usize::MAX);
    assert!(
        done_pos < fc_pos,
        "synthesized done must precede function_call: {s}"
    );
}

/// Spec E3: a truncated run (only first fragment + a few deltas, then
/// stream ends) does NOT synthesize done in finish() — gamma-1.
#[test]
fn e3_truncated_run_no_synthesized_done_in_finish() {
    let mut m = FragmentedItemMerger::new(true);
    let msg = |id: &str| {
        sse(
            "response.output_item.added",
            &format!(
                "{{\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{{\"type\":\"message\",\"id\":\"{}\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}}}",
                id
            ),
        )
    };
    let delta = |text: &str| {
        sse(
            "response.output_text.delta",
            &format!(
                "{{\"type\":\"response.output_text.delta\",\"item_id\":\"msg_0\",\"output_index\":0,\"delta\":\"{}\"}}",
                text
            ),
        )
    };
    let _ = push_raw(&mut m, &msg("msg_0"));
    let _ = push_raw(&mut m, &msg("msg_1"));
    let _ = push_raw(&mut m, &delta("half"));
    // Stream ends without response.completed.
    let finish_out = m.finish();
    assert!(
        finish_out.is_empty(),
        "finish() must not synthesize done (gamma-1)"
    );
}

/// Spec E4: a fragment with an empty `item.id` passes through (the
/// merger tolerates; downstream healer handles).
#[test]
fn e4_empty_item_id_tolerated() {
    let mut m = FragmentedItemMerger::new(true);
    let raw = sse(
        "response.output_item.added",
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"","role":"assistant","status":"in_progress","content":[]}}"#,
    );
    let out = push_raw(&mut m, &raw);
    assert_eq!(concat(out), raw);
}

// ─────────────────────────────────────────────────────────────────────────────
// Task 9 (post-review): regression fixtures for the 3 bugs flagged in
// the PR #7 review comment (id 186) — see src/heal/merge.rs doc on the
// `flush_run_synthesis` + `on_completed` changes that motivated them.
// ─────────────────────────────────────────────────────────────────────────────

/// Spec W6: when a merged message run is closed by `response.completed`,
/// the emitted `response.completed` event's `response.output` array
/// contains ONLY the merged item — not the original fragment items.
/// `last_completed_payload` reads the last `response.completed` in the
/// forwarded bytes (src/proxy/capture.rs) and uses its `output` array
/// to populate the session store; if the upstream's fragmented array
/// is forwarded unchanged, cross-provider replay replays the fragments.
///
/// Regression for review finding #2.
#[test]
fn w6_synthesized_response_completed_has_merged_output() {
    let mut m = FragmentedItemMerger::new(true);
    let msg = |id: &str| {
        sse(
            "response.output_item.added",
            &format!(
                r#"{{"type":"response.output_item.added","output_index":0,"item":{{"type":"message","id":"{id}","role":"assistant","status":"in_progress","content":[]}}}}"#
            ),
        )
    };
    let delta = |item_id: &str, text: &str| {
        sse(
            "response.output_text.delta",
            &format!(
                r#"{{"type":"response.output_text.delta","item_id":"{item_id}","output_index":0,"delta":"{text}"}}"#
            ),
        )
    };
    let _ = push_raw(&mut m, &msg("msg_0"));
    let _ = push_raw(&mut m, &msg("msg_1")); // suppress; merge mode
    let _ = push_raw(&mut m, &delta("msg_0", "Hello "));
    let _ = push_raw(&mut m, &delta("msg_1", "world"));

    // Upstream `response.completed` carries the fragmented output array
    // (what MiniMax M3 actually sends in the wild).
    let completed_data = r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[{"type":"message","id":"msg_0","content":[{"type":"output_text","text":"Hello "}]},{"type":"message","id":"msg_1","content":[{"type":"output_text","text":"world"}]}]}}"#;
    let raw = sse("response.completed", completed_data);
    let out = push_raw(&mut m, &raw);
    let all: Vec<u8> = out.into_iter().flat_map(|b| b.to_vec()).collect();
    let all_str = std::str::from_utf8(&all).unwrap();

    // Find the LAST `response.completed` event's data payload. We slice
    // from the byte offset AFTER the match — `match_indices` returns the
    // matched substring itself, not what follows it.
    let last_completed = all_str
        .match_indices("event: response.completed\ndata: ")
        .last()
        .map(|(pos, m)| &all_str[pos + m.len()..])
        .expect("response.completed must be present");
    let last_data = last_completed
        .split("\n\n")
        .next()
        .unwrap_or_default()
        .trim();
    let parsed: serde_json::Value =
        serde_json::from_str(last_data).expect("completed data must be valid JSON");
    let output = parsed
        .get("response")
        .and_then(|r| r.get("output"))
        .and_then(|o| o.as_array())
        .expect("response.output must be an array");

    assert_eq!(
        output.len(),
        1,
        "merged output array must contain exactly 1 item, got: {output:?}"
    );
    let merged_msg = &output[0];
    assert_eq!(
        merged_msg.get("id").and_then(|v| v.as_str()),
        Some("msg_0"),
        "merged item keeps first-fragment id: {merged_msg:?}"
    );
    let merged_text = merged_msg
        .get("content")
        .and_then(|c| c.get(0))
        .and_then(|p| p.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or_default();
    assert_eq!(
        merged_text, "Hello world",
        "merged text in completed.output: {merged_msg:?}"
    );
    // And the second fragment must be gone.
    assert!(
        !last_data.contains("\"id\":\"msg_1\""),
        "fragment id msg_1 must not appear in the rewritten completed: {last_data}"
    );
}

/// Spec W7: a fragmented function_call run with a stable `name` on the
/// first fragment must preserve that name in the synthesized
/// `output_item.done`. The previous implementation hardcoded
/// `"name": "<merged>"` (see review finding #3); this fixture asserts
/// the first-fragment name reaches the wire.
///
/// Regression for review finding #3.
#[test]
fn w7_synthesized_function_call_preserves_first_fragment_name() {
    let mut m = FragmentedItemMerger::new(true);
    // Two fragments with the same call_id and the same `name` — the
    // merger should preserve the FIRST fragment's name.
    let r0 = sse(
        "response.output_item.added",
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_0","call_id":"call_shared","name":"get_weather","arguments":"","status":"in_progress"}}"#,
    );
    let r1 = sse(
        "response.output_item.added",
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_shared","name":"get_weather","arguments":"","status":"in_progress"}}"#,
    );
    let _ = push_raw(&mut m, &r0);
    let _ = push_raw(&mut m, &r1);
    let d0 = sse(
        "response.function_call_arguments.delta",
        r#"{"type":"response.function_call_arguments.delta","item_id":"fc_0","output_index":0,"delta":"{\"city\":"}"#,
    );
    let d1 = sse(
        "response.function_call_arguments.delta",
        r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":0,"delta":"\"SF\"}"}"#,
    );
    let _ = push_raw(&mut m, &d0);
    let _ = push_raw(&mut m, &d1);

    // Type-switch to reasoning to flush the function_call run.
    let rs = sse(
        "response.output_item.added",
        r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"reasoning","id":"rs_0","summary":[{"type":"summary_text","text":""}]}}"#,
    );
    let rs_out = push_raw(&mut m, &rs);
    let all: Vec<u8> = rs_out.into_iter().flat_map(|b| b.to_vec()).collect();
    let all_str = std::str::from_utf8(&all).unwrap();

    assert!(
        all_str.contains(r#""name":"get_weather""#),
        "first-fragment name 'get_weather' must be preserved in synthesis: {all_str}"
    );
    assert!(
        !all_str.contains(r#""name":"<merged>""#),
        "placeholder name '<merged>' must not appear in synthesis: {all_str}"
    );
    // Arguments are still merged.
    assert!(
        all_str.contains(r#""arguments":"{\"city\":\"SF\"}""#),
        "merged arguments present in synthesis: {all_str}"
    );
}

