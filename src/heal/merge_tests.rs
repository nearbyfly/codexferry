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
/// drops all events (the ResponsesStreamHealer downstream still receives
/// the raw bytes via its own push_event path; in Task 9, the passthrough
/// wiring will only invoke the merger when the gate is on, so this
/// short-circuit isn't strictly needed but mirrors DsmlStreamFilter::new(false)).
#[test]
fn k1_disabled_drops_all_events() {
    let mut m = FragmentedItemMerger::new(false);
    let raw = sse(
        "response.output_item.added",
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_0","role":"assistant","status":"in_progress","content":[]}}"#,
    );
    let out = push_raw(&mut m, &raw);
    assert!(out.is_empty(), "disabled merger must drop events");
}
