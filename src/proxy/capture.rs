//! Session/usage capture helpers shared by the chat and passthrough paths.
//! Extracted from `proxy/mod.rs` (module-split spec Phase 2).
//! All items are `pub(super)` — visible within `crate::proxy` only.

use crate::session::SessionStore;
use crate::wire::responses::ResponsesRequest;
use serde_json::Value;

/// Extract the new input items from a Responses request as Responses-format
/// `Value`s (spec §8.2).
///
/// An array `input` is cloned as-is; a plain string `input` is normalized into
/// a single user message with one `input_text` part, so session storage always
/// uses the same item shape.
pub(super) fn input_items(req: &ResponsesRequest) -> Vec<Value> {
    match &req.input {
        crate::wire::responses::ResponsesInput::Items(items) => items.clone(),
        crate::wire::responses::ResponsesInput::Text(text) => {
            vec![
                serde_json::json!({"type":"message","role":"user","content":[{"type":"input_text","text":text}]}),
            ]
        }
    }
}

/// Whether this request asks the router to persist the session (Responses
/// API `store` field semantics): only an explicit `store: false` skips
/// persistence; absent (or `true`) stores as before (spec §8.2). Codex CLI
/// sends `store: false` and replays its full transcript inline every turn,
/// so a stored snapshot would never be read back (issue #9).
pub(super) fn store_enabled(req: &ResponsesRequest) -> bool {
    !matches!(req.extra.get("store"), Some(Value::Bool(false)))
}

/// Store the full conversation context under `id` in the session store.
///
/// Session entries are full-context snapshots (spec §8.2): `history` (from
/// `previous_response_id`) followed by this turn's `input_items` and
/// `output_items`. Snapshotting the complete context rather than deltas keeps
/// storage provider-agnostic and simple, at the cost of O(n²) total growth
/// (bounded by TTL + LRU + the memory budget).
pub(super) async fn save_session(
    sessions: &SessionStore,
    id: String,
    history: &[Value],
    input_items: Vec<Value>,
    output_items: Vec<Value>,
) {
    let mut full_context = history.to_vec();
    full_context.extend(input_items);
    full_context.extend(output_items);
    sessions.save(id, full_context).await;
}

/// Parse the JSON payload of the last `event: response.completed` SSE event
/// in a raw upstream byte buffer. Returns the whole parsed event data; the
/// `response` wrapper (native OpenAI nesting) is unwrapped by
/// [`completed_capture`].
///
/// One lossy decode of the buffer, `rfind` for the last completed-event
/// marker, then ALL consecutive `data:` lines following it are collected and
/// joined - the payload may be split across multiple `data:` lines per the
/// SSE spec. A single whole-buffer decode avoids the split-UTF-8 hazards of
/// per-chunk parsing; `rfind` (rather than `find`) guards against multiple
/// completed events in one stream. The relay trims the buffer to the FIRST
/// completed-event marker via [`trim_completed_prefix`] (issue #15 item 2),
/// so this normally decodes only the final payload tail. Returns None when
/// no completed event, no data lines, or an unparseable payload was found.
pub(super) fn last_completed_payload(raw: &[u8]) -> Option<Value> {
    let text = String::from_utf8_lossy(raw);
    let idx = text.rfind("event: response.completed")?;
    let rest = &text[idx..];
    let data_lines: Vec<&str> = rest
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("data:"))
        .take_while(|l| l.trim_start().starts_with("data:"))
        .map(|l| {
            l.trim_start()
                .strip_prefix("data:")
                .unwrap_or("")
                .strip_prefix(' ')
                .unwrap_or("")
        })
        .collect();
    if data_lines.is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(&data_lines.join("\n")).ok()
}

/// `event: response.completed` SSE marker the relay trims on.
const COMPLETED_EVENT_MARKER: &[u8] = b"event: response.completed";

/// Incrementally drop everything BEFORE the first `event: response.completed`
/// marker so the passthrough relay's `raw` buffer stays O(tail) instead of
/// O(entire stream) (issue #15 item 2). Call after each `extend_from_slice`,
/// passing the buffer length BEFORE the append (`prev_len`); only the last
/// `MARKER_LEN - 1` bytes of the previous content plus the newly appended
/// bytes are scanned, so a marker split across chunk boundaries is still
/// found. Returns true (and trims) the first time the marker is complete;
/// callers then stop trimming — later markers are intentionally kept so
/// [`last_completed_payload`]'s `rfind` still selects the LAST event.
pub(super) fn trim_completed_prefix(raw: &mut Vec<u8>, prev_len: usize) -> bool {
    let marker_len = COMPLETED_EVENT_MARKER.len();
    let start = prev_len.saturating_sub(marker_len - 1);
    let hit = raw[start..]
        .windows(marker_len)
        .position(|w| w == COMPLETED_EVENT_MARKER)
        .map(|p| start + p);
    if let Some(p) = hit {
        if p > 0 {
            raw.drain(..p);
        }
        true
    } else {
        false
    }
}

/// Read `(input_tokens, output_tokens)` from a Responses `usage` object.
///
/// Missing or non-numeric fields degrade to 0 - these counts feed only the
/// per-request log line and accounting, never control flow.
pub(super) fn usage_tokens(usage: &Value) -> (u32, u32) {
    let input_tokens = usage
        .get("input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let output_tokens = usage
        .get("output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    (input_tokens, output_tokens)
}

/// Derive the session-capture fields from a parsed completed payload:
/// `(response_id, output items, token usage)`.
///
/// The fields are read from the `response` wrapper when present (native
/// OpenAI nesting) or top-level otherwise (some providers). Takes the owned
/// [`Value`] so the output array is MOVED out (`mem::take`) instead of
/// deep-copied. This is the single extraction shared by the streaming
/// passthrough (SSE buffer via [`last_completed_payload`]) and the
/// non-streaming passthrough (JSON body) - one parse, one extraction per
/// request.
pub(super) fn completed_capture(
    mut val: Value,
) -> (Option<String>, Vec<Value>, Option<(u32, u32)>) {
    let obj = match val.get_mut("response") {
        Some(inner) => inner,
        None => &mut val,
    };
    let id = obj.get("id").and_then(|v| v.as_str()).map(String::from);
    let output = obj
        .get_mut("output")
        .and_then(|o| o.as_array_mut())
        .map(std::mem::take)
        .unwrap_or_default();
    let usage = obj.get("usage").map(usage_tokens);
    (id, output, usage)
}

#[cfg(test)]
mod tests;
