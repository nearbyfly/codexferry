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
    assert_eq!(concat(out), raw, "disabled merger must pass through verbatim");
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
    assert!(out1.is_empty(), "same call_id → merge (suppress second added)");
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
    let msg_added = || sse("response.output_item.added",
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_0","role":"assistant","status":"in_progress","content":[]}}"#);
    let rs_added = || sse("response.output_item.added",
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"reasoning","id":"rs_0","summary":[{"type":"summary_text","text":""}]}}"#);
    let fc_added = || sse("response.output_item.added",
        r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"fc_0","call_id":"c0","name":"shell","arguments":"","status":"in_progress"}}"#);
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
    let msg = |idx: u64| sse("response.output_item.added",
        &format!(r#"{{"type":"response.output_item.added","output_index":{idx},"item":{{"type":"message","id":"msg_{idx}","role":"assistant","status":"in_progress","content":[]}}}}"#));
    let rs = sse("response.output_item.added",
        r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"reasoning","id":"rs_0","summary":[{"type":"summary_text","text":""}]}}"#);
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
    assert!(out1.is_empty(), "second msg fragment suppressed (run in progress)");
    assert_eq!(concat(out_rs), rs, "reasoning item unaffected by type switch");
    assert_eq!(concat(out3), msg(3), "second msg run starts fresh (passes through)");
    assert!(out4.is_empty(), "second msg run's 2nd fragment suppressed");
}
