//! Tests for capture.rs, extracted from proxy/tests.rs (module-split spec Phase 2).
use serde_json::json;

use super::*;

/// The exact call the streaming passthrough task makes (one parse, one
/// extraction), so the tests exercise the production path.
fn capture(raw: &[u8]) -> (Option<String>, Vec<Value>, Option<(u32, u32)>) {
    last_completed_payload(raw)
        .map(completed_capture)
        .unwrap_or((None, Vec::new(), None))
}

#[test]
fn completed_capture_nested_openai_shape() {
    let raw = b"event: response.created\ndata: {\"type\":\"response.created\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_abc\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\"}]}}\n";
    let (id, output, usage) = capture(raw);
    assert_eq!(id.as_deref(), Some("resp_abc"));
    assert_eq!(output.len(), 1);
    assert_eq!(output[0]["type"], "message");
    assert_eq!(usage, None);
}

#[test]
fn completed_capture_top_level_shape() {
    let raw = b"event: response.completed\ndata: {\"id\":\"resp_x\",\"output\":[{\"type\":\"message\"}],\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n";
    let (id, output, usage) = capture(raw);
    assert_eq!(id.as_deref(), Some("resp_x"));
    assert_eq!(output.len(), 1);
    assert_eq!(usage, Some((1, 2)));
}

#[test]
fn completed_capture_nested_usage() {
    let raw = b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_abc\",\"usage\":{\"input_tokens\":7,\"output_tokens\":3,\"total_tokens\":10}}}\n";
    let (id, _, usage) = capture(raw);
    assert_eq!(id.as_deref(), Some("resp_abc"));
    assert_eq!(usage, Some((7, 3)));
}

#[test]
fn completed_capture_absent_or_malformed() {
    // No completed event at all.
    let none = b"event: response.created\ndata: {\"type\":\"response.created\"}\n";
    let (id, output, usage) = capture(none);
    assert!(id.is_none() && output.is_empty() && usage.is_none());
    // Completed event present but payload is not JSON.
    let malformed = b"event: response.completed\ndata: {not json}\n";
    let (id, output, usage) = capture(malformed);
    assert!(id.is_none() && output.is_empty() && usage.is_none());
    // Completed event without a usage object.
    let no_usage = b"event: response.completed\ndata: {\"id\":\"resp_x\"}\n";
    let (id, output, usage) = capture(no_usage);
    assert_eq!(id.as_deref(), Some("resp_x"));
    assert!(output.is_empty() && usage.is_none());
}

#[test]
fn completed_capture_handles_multiline_data() {
    // A response.completed event whose data payload is split across
    // multiple data: lines (valid SSE for large payloads); both the
    // output items and the usage must come through.
    let raw = b"event: response.completed\ndata: {\"type\":\"response.completed\",\ndata: \"response\":{\"id\":\"resp_abc\",\"output\":[{\"type\":\"message\"}],\"usage\":{\"input_tokens\":3,\"output_tokens\":2,\"total_tokens\":5}}}\n\n";
    let (id, output, usage) = capture(raw);
    assert_eq!(id.as_deref(), Some("resp_abc"));
    assert_eq!(output.len(), 1);
    assert_eq!(output[0]["type"], "message");
    assert_eq!(usage, Some((3, 2)));
}

#[tokio::test]
async fn save_session_composes_history_input_output() {
    let store = SessionStore::new(168, 16, 16);
    let id = "resp_test".to_string();
    let history = vec![json!({"type": "message", "role": "user", "content": "old"})];
    let input = vec![json!({"type": "message", "role": "user", "content": "new"})];
    let output = vec![json!({"type": "message", "role": "assistant", "content": "reply"})];
    save_session(&store, id.clone(), &history, input, output).await;
    let got = store.get(&id).await.unwrap();
    assert_eq!(got.len(), 3);
    assert_eq!(got[0]["content"], "old");
    assert_eq!(got[1]["content"], "new");
    assert_eq!(got[2]["content"], "reply");
}

// --- Issue #15 item 2: incremental raw-tail trimming ------------------------

/// The production call shape: a relay loop extends `raw` chunk by chunk and
/// trims once the first completed-event marker has been fully received.
fn extend_and_trim(raw: &mut Vec<u8>, bytes: &[u8], trimmed: &mut bool) {
    let prev_len = raw.len();
    raw.extend_from_slice(bytes);
    if !*trimmed && trim_completed_prefix(raw, prev_len) {
        *trimmed = true;
    }
}

#[test]
fn trim_completed_prefix_drops_prefix_before_first_marker() {
    let mut raw = Vec::new();
    let mut trimmed = false;
    extend_and_trim(
        &mut raw,
        b"data: {\"type\":\"response.created\"}\n\nevent: response.completed\ndata: {\"id\":\"resp_abc\"}\n",
        &mut trimmed,
    );
    assert!(trimmed);
    assert!(raw.starts_with(b"event: response.completed"));
}

#[test]
fn trim_completed_prefix_finds_marker_split_across_chunks() {
    // The marker tail arrives in a later chunk; the overlap window must find
    // it (same carry discipline as other split-markers in the relay).
    let mut raw = Vec::new();
    let mut trimmed = false;
    extend_and_trim(&mut raw, b"data: x\n\nevent: response", &mut trimmed);
    assert!(!trimmed);
    extend_and_trim(
        &mut raw,
        b".completed\ndata: {\"id\":\"resp_abc\"}\n",
        &mut trimmed,
    );
    assert!(trimmed);
    assert!(raw.starts_with(b"event: response.completed"));
}

#[test]
fn trim_completed_prefix_finds_marker_inside_large_first_chunk() {
    // A first chunk that already contains the marker followed by plenty of
    // trailing bytes: the scan must still see it (the overlap window is
    // relative to the PREVIOUS length, not the new tail).
    let mut raw = Vec::new();
    let mut trimmed = false;
    extend_and_trim(
        &mut raw,
        b"noise noise noise event: response.completed\ndata: {\"id\":\"resp_x\"}\nand lots of trailing bytes here",
        &mut trimmed,
    );
    assert!(trimmed);
    assert!(raw.starts_with(b"event: response.completed"));
}

#[test]
fn trim_completed_prefix_returns_false_without_marker() {
    let mut raw = Vec::new();
    let mut trimmed = false;
    extend_and_trim(
        &mut raw,
        b"event: response.created\ndata: {\"type\":\"response.created\"}\n",
        &mut trimmed,
    );
    extend_and_trim(&mut raw, b"more bytes", &mut trimmed);
    assert!(!trimmed);
    assert_eq!(
        raw,
        b"event: response.created\ndata: {\"type\":\"response.created\"}\nmore bytes".to_vec()
    );
}

#[test]
fn trim_completed_prefix_keeps_later_markers_for_rfind() {
    // Two completed events in one stream: trimming to the FIRST marker must
    // not change last_completed_payload's semantics (it still returns the
    // LAST event's payload).
    let mut raw = Vec::new();
    let mut trimmed = false;
    extend_and_trim(
        &mut raw,
        b"event: response.completed\ndata: {\"id\":\"resp_first\"}\n\nevent: response.completed\ndata: {\"id\":\"resp_last\"}\n",
        &mut trimmed,
    );
    assert!(trimmed);
    let (id, _, _) = last_completed_payload(&raw)
        .map(completed_capture)
        .unwrap_or((None, Vec::new(), None));
    assert_eq!(id.as_deref(), Some("resp_last"));
}
