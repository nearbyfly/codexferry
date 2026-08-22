//! Responses-path healing: rewrite an upstream Responses SSE stream or
//! blocking JSON body in place when DSML/think quirks fire
//! (`ResponsesStreamHealer`, `heal_responses_body`). Extracted from
//! `heal.rs` (module-split spec Phase 3).

use super::dsml::INJECT_INDEX_BASE;
use super::synthesize_call_id;
use super::HealGates;
use super::{
    contains_think_markup, parse_leaked_tool_calls, DsmlStreamFilter, DsmlToolCall, ThinkSplit,
    ThinkStreamFilter,
};
use bytes::Bytes;
use serde_json::Value;

pub(super) fn sse_block(event: &str, payload: &Value) -> Bytes {
    Bytes::from(format!("event: {event}\ndata: {payload}\n\n"))
}

/// Healing rewriter for a Responses SSE passthrough stream (quirks
/// `dsml_heal` + `think_tags` on the responses-format path).
///
/// Feed each upstream event through [`ResponsesStreamHealer::push_event`]
/// (the structure-preserving splitter's raw bytes, event name, data payload)
/// and forward the returned byte chunks; call [`ResponsesStreamHealer`::
/// finish`] at stream end. Healthy streams pass through untouched: events are
/// only rewritten after a filter actually fires (trigger-based,
/// self-deactivating — same posture as the chat path). Two no-fire caveats:
/// a text delta ending in a marker-prefix tail (`<`, `<t`, `｜`, …) is
/// re-encoded and its withheld tail released as a separate delta before the
/// done/completed events, and `split_sse_events` drops a trailing run of pure
/// whitespace.
///
/// Rewrite set: `response.output_text.delta`; the full-text echoes
/// (`response.output_text.done`, `response.content_part.done`);
/// `response.output_item.done` for the streamed message;
/// `response.completed`. Injections mirror the chat-path StreamConverter
/// shapes: a synthesized `reasoning` item for healed `<think>` text,
/// `function_call` items (added → arguments.delta → done) for healed DSML
/// calls, emitted before the rewritten `response.completed`. Unparseable
/// rewrite-set events are forwarded verbatim with a warn (fail-open).
pub struct ResponsesStreamHealer {
    dsml: DsmlStreamFilter,
    think: ThinkStreamFilter,
    /// The streamed message item (from its output_item.added), if seen.
    message_item_id: Option<String>,
    /// The streamed message item's output_index (from its output_item.added).
    message_output_index: Option<usize>,
    /// Cleaned text actually emitted — the rewrite source of truth.
    ///
    /// Rewriting assumes the upstream's full-text echoes (`output_text.done`
    /// / `response.completed`) equal the sum of the prior text deltas; the
    /// echoes are replaced wholesale with this accumulated string (holds for
    /// DeepSeek; by-plan design).
    healed_text: String,
    /// Whether any filter has withheld/rerouted text so far.
    healing_fired: bool,
    /// Synthesized reasoning item (id, output_index), created lazily.
    reasoning: Option<(String, usize)>,
    reasoning_text: String,
    /// Healed calls carried over to the completed rewrite:
    /// (fc item id, call_id, name, arguments, output_index).
    injected_calls: Vec<(String, String, String, String, usize)>,
    /// Filters finished (at completed or stream end), idempotence guard.
    finished: bool,
    next_index: usize,
}

impl ResponsesStreamHealer {
    /// Create a healer honoring the given per-request gates (`dsml_heal` +
    /// `think_tags`); a disabled gate passes all text through untouched.
    pub fn new(gates: HealGates) -> Self {
        Self {
            dsml: DsmlStreamFilter::new(gates.dsml),
            think: ThinkStreamFilter::new(gates.think),
            message_item_id: None,
            message_output_index: None,
            healed_text: String::new(),
            healing_fired: false,
            reasoning: None,
            reasoning_text: String::new(),
            injected_calls: Vec::new(),
            finished: false,
            next_index: INJECT_INDEX_BASE,
        }
    }

    /// Process one upstream SSE event; returns the byte chunks to forward
    /// (0 = withheld, 1 = passthrough/rewrite, >1 = rewrite + injections).
    pub fn push_event(&mut self, raw: &[u8], event: Option<&str>, data: &str) -> Vec<Bytes> {
        match event {
            Some("response.output_item.added") => {
                if let Ok(v) = serde_json::from_str::<Value>(data) {
                    if v["item"]["type"] == "message" {
                        self.message_item_id = v["item"]["id"].as_str().map(String::from);
                        self.message_output_index = v["output_index"].as_u64().map(|i| i as usize);
                    }
                }
                vec![Bytes::copy_from_slice(raw)]
            }
            Some("response.output_text.delta") => self.heal_text_delta(raw, data),
            Some("response.output_text.done") | Some("response.content_part.done") => {
                let mut out = self.release_untracked_tail();
                out.extend(self.rewrite_text_echo(raw, event.unwrap(), data));
                out
            }
            Some("response.output_item.done") => {
                let mut out = self.release_untracked_tail();
                out.extend(self.rewrite_item_done(raw, data));
                out
            }
            Some("response.completed") => {
                let mut out = self.release_untracked_tail();
                out.extend(self.rewrite_completed(raw, data));
                out
            }
            _ => vec![Bytes::copy_from_slice(raw)],
        }
    }

    /// On a healthy stream (nothing fired), a text delta ending in a
    /// marker-prefix tail (`<`, `<t`, `｜`, …) leaves that tail withheld in
    /// the filters pending disambiguation. A non-delta event
    /// (done/completed) arriving means no further delta can resolve it into a
    /// real marker, so the tail is released as text BEFORE the event is
    /// forwarded - otherwise `finish()` would emit it after the terminal
    /// `response.completed` (wire-order violation). No-op once healing has
    /// fired (that path flushes inside `rewrite_completed`).
    fn release_untracked_tail(&mut self) -> Vec<Bytes> {
        let mut out = Vec::new();
        if self.healing_fired {
            return out;
        }
        let (dsml_tail, _calls) = self.dsml.take();
        let mut split = self.think.push(&dsml_tail);
        let tail = self.think.take();
        split.reasoning.push_str(&tail.reasoning);
        split.text.push_str(&tail.text);
        // Nothing has fired, so only plain marker-prefix text can be pending
        // (reasoning / calls only appear once a marker is confirmed).
        self.flush_tail(&split, &mut out);
        out
    }

    /// Stream end: flush whatever the filters still withheld. With no
    /// `response.completed` seen, complete healed calls are still injected
    /// (otherwise already-withheld calls would be dropped); there is just
    /// no completed payload to rewrite.
    pub fn finish(&mut self) -> Vec<Bytes> {
        let mut out = Vec::new();
        if !self.finished {
            self.finished = true;
            let (dsml_tail, calls) = self.dsml.take();
            let mut split = self.think.push(&dsml_tail);
            let tail = self.think.take();
            split.reasoning.push_str(&tail.reasoning);
            split.text.push_str(&tail.text);
            // TODO(phase-b review #10): on a truncated stream (no
            // `response.completed`) the reasoning item injected by
            // `flush_tail` -> `emit_reasoning` never receives its
            // `output_item.done` - that is only emitted inside
            // `rewrite_completed`, which never runs here, while the injected
            // function_call items DO get their done. The reasoning item would
            // stay `in_progress` forever. Emit the reasoning done in
            // `finish()` too; rare in practice (truncated streams), and no
            // session is persisted without a completed id, so replay is
            // unaffected.
            self.flush_tail(&split, &mut out);
            self.inject_calls(&calls, &mut out);
        }
        out
    }

    /// Filter a text delta through dsml → think (chat-path order) and
    /// emit passthrough/rewrite/injection chunks.
    fn heal_text_delta(&mut self, raw: &[u8], data: &str) -> Vec<Bytes> {
        let Ok(mut v) = serde_json::from_str::<Value>(data) else {
            tracing::warn!("healer: unparseable output_text.delta forwarded verbatim");
            return vec![Bytes::copy_from_slice(raw)];
        };
        let delta = v["delta"].as_str().unwrap_or("").to_string();
        // True healing trigger: a filter detected its marker in THIS delta.
        // Plain marker-prefix withholding (e.g. a delta ending in `<`) is
        // not healing — the withheld bytes are released on a later delta, so
        // it must NOT set `healing_fired` (which would rewrite the full-text
        // echoes even though nothing leaked).
        let dsml_fired_before = self.dsml.fired();
        let think_fired_before = self.think.fired();
        let dsml_text = self.dsml.push(&delta);
        let split = self.think.push(&dsml_text);
        let fired_now =
            self.dsml.fired() != dsml_fired_before || self.think.fired() != think_fired_before;
        let mut out = Vec::new();
        if !split.reasoning.is_empty() {
            self.healing_fired = true;
            self.emit_reasoning(&split.reasoning, &mut out);
        }
        self.healed_text.push_str(&split.text);
        if fired_now {
            self.healing_fired = true;
        }
        if !fired_now && split.reasoning.is_empty() && split.text == delta {
            // Nothing detected and nothing withheld: byte-for-byte passthrough.
            return vec![Bytes::copy_from_slice(raw)];
        }
        if !split.text.is_empty() {
            v["delta"] = Value::String(split.text);
            out.push(sse_block("response.output_text.delta", &v));
        }
        // empty → the event is withheld entirely (nothing to forward)
        out
    }

    /// Rewrite a full-text echo event (output_text.done / content_part.done)
    /// to the cleaned accumulated text.
    ///
    /// TODO(phase-b review #5): this has no item_id guard — in the
    /// effectively-nonexistent multi-message upstream stream it would rewrite
    /// an untracked message's echo with the combined healed text. Add an
    /// `item_id` check against `self.message_item_id` if multi-message
    /// streams ever surface.
    fn rewrite_text_echo(&mut self, raw: &[u8], event: &str, data: &str) -> Vec<Bytes> {
        if !self.healing_fired {
            return vec![Bytes::copy_from_slice(raw)];
        }
        let Ok(mut v) = serde_json::from_str::<Value>(data) else {
            tracing::warn!("healer: unparseable {event} forwarded verbatim");
            return vec![Bytes::copy_from_slice(raw)];
        };
        let full = self.healed_text.clone();
        if v.get("text").is_some() {
            v["text"] = Value::String(full.clone());
        }
        if v.get("part").and_then(|p| p.get("text")).is_some() {
            v["part"]["text"] = Value::String(full);
        }
        vec![sse_block(event, &v)]
    }

    /// Rewrite the streamed message's output_item.done payload.
    fn rewrite_item_done(&mut self, raw: &[u8], data: &str) -> Vec<Bytes> {
        if !self.healing_fired {
            return vec![Bytes::copy_from_slice(raw)];
        }
        let Ok(mut v) = serde_json::from_str::<Value>(data) else {
            tracing::warn!("healer: unparseable output_item.done forwarded verbatim");
            return vec![Bytes::copy_from_slice(raw)];
        };
        if v["item"]["type"] != "message" {
            return vec![Bytes::copy_from_slice(raw)];
        }
        // Only the tracked streamed message carries the healed text; other
        // message items (multi-message streams are effectively unseen
        // upstream) pass through untouched. When no output_item.added was
        // seen (completed-only streams) the fallback is to rewrite message
        // items as before.
        if let Some(expected) = &self.message_item_id {
            if v["item"]["id"].as_str() != Some(expected.as_str()) {
                return vec![Bytes::copy_from_slice(raw)];
            }
        }
        set_message_text(&mut v["item"], &self.healed_text.clone());
        vec![sse_block("response.output_item.done", &v)]
    }

    /// Tail flush + function_call injection + completed rewrite.
    fn rewrite_completed(&mut self, raw: &[u8], data: &str) -> Vec<Bytes> {
        if !self.healing_fired {
            return vec![Bytes::copy_from_slice(raw)];
        }
        let Ok(mut v) = serde_json::from_str::<Value>(data) else {
            tracing::warn!("healer: unparseable response.completed forwarded verbatim");
            return vec![Bytes::copy_from_slice(raw)];
        };
        // TODO(phase-b review #8): the withheld tail is flushed HERE, at
        // `response.completed` — i.e. AFTER the upstream's `output_text.done`
        // / `output_item.done` were already forwarded — the reverse of the
        // chat path's flush-before-done order. The classic leak shape has an
        // empty tail (the whole envelope was buffered), so it does not
        // trigger in practice; revisit if a leak ending mid-tag shows up.
        let mut out = Vec::new();
        if !self.finished {
            self.finished = true;
            let (dsml_tail, calls) = self.dsml.take();
            if !calls.is_empty() {
                let count = calls.len();
                tracing::warn!(
                    "quirk dsml_heal fired: healed {count} leaked DSML tool call(s) from responses stream"
                );
            }
            let mut split = self.think.push(&dsml_tail);
            let tail = self.think.take();
            split.reasoning.push_str(&tail.reasoning);
            split.text.push_str(&tail.text);
            // TODO(phase-b review #11): the `quirk think_tags fired` warn is
            // keyed on `self.reasoning.is_some()`, evaluated BEFORE
            // `flush_tail` pushes the DSML-cleaned tail through the think
            // filter. When the only `<think>` markup arrives inside that tail
            // (dual leak: think text after the DSML marker), the reasoning is
            // healed and injected but the warn is never emitted, losing the
            // class-B telemetry. The chat path logs from `think_filter.fired()`
            // after the tail push; do the same here (telemetry only, no wire
            // impact).
            if self.reasoning.is_some() {
                tracing::warn!(
                    "quirk think_tags fired: split leaked <think> markup from responses stream"
                );
            }
            self.flush_tail(&split, &mut out);
            self.inject_calls(&calls, &mut out);
        }
        // Rewrite every echoed message text in the completed payload, then
        // append the injected items (reasoning first: canonical turn order).
        let healed_text = self.healed_text.clone();
        // Native OpenAI wraps items under `response.output`; some providers
        // are flat. Either way, rewrite the array in place.
        let target = if v.get("response").is_some() {
            v.get_mut("response").expect("checked above")
        } else {
            &mut v
        };
        if let Some(output) = target.get_mut("output").and_then(Value::as_array_mut) {
            for item in output.iter_mut() {
                // Only the tracked streamed message carries the healed text;
                // other message items pass through untouched. When no
                // output_item.added was seen (completed-only streams) the
                // fallback is to rewrite message items as before.
                let is_tracked = self
                    .message_item_id
                    .as_ref()
                    .is_none_or(|expected| item["id"].as_str() == Some(expected.as_str()));
                if item["type"] == "message" && is_tracked {
                    set_message_text(item, &healed_text);
                }
            }
            let insert_at = output
                .iter()
                .position(|i| i["type"] == "message")
                .unwrap_or(output.len());
            // The injected reasoning item is inserted BEFORE its message in
            // the completed output array — canonical reasoning-before-message
            // turn order (AGENTS.md §8a) for session replay — even though its
            // injected output_index is 10000+ and the array is therefore not
            // index-monotonic. Deliberate: clients correlate items by id, not
            // array position.
            // TODO(phase-b review #7): `insert_at` targets the FIRST message
            // item, not necessarily the tracked one; in a multi-message
            // stream the reasoning would land before an untracked message.
            // Revisit if multi-message streams ever surface.
            if let Some((id, _)) = &self.reasoning {
                output.insert(
                    insert_at,
                    serde_json::json!({
                        "type": "reasoning", "id": id,
                        "summary": [{"type": "summary_text", "text": self.reasoning_text}]
                    }),
                );
            }
            for (fc_id, call_id, name, args, _) in &self.injected_calls {
                output.push(serde_json::json!({
                    "type": "function_call", "id": fc_id, "call_id": call_id,
                    "name": name, "arguments": args, "status": "completed"
                }));
            }
        }
        // Mark the injected reasoning item done right before completed,
        // mirroring the chat-path StreamConverter finish order (message done,
        // reasoning done, then completed).
        if let Some((id, idx)) = &self.reasoning {
            out.push(sse_block(
                "response.output_item.done",
                &serde_json::json!({
                    "type": "response.output_item.done", "output_index": idx,
                    "item": {"type": "reasoning", "id": id,
                             "summary": [{"type": "summary_text", "text": self.reasoning_text}]}
                }),
            ));
        }
        out.push(sse_block("response.completed", &v));
        out
    }

    /// Emit the filters' tail text/reasoning as delta events (nothing healed
    /// may be dropped).
    fn flush_tail(&mut self, split: &ThinkSplit, out: &mut Vec<Bytes>) {
        if !split.reasoning.is_empty() {
            self.healing_fired = true;
            self.emit_reasoning(&split.reasoning, out);
        }
        if !split.text.is_empty() {
            self.healed_text.push_str(&split.text);
            out.push(sse_block(
                "response.output_text.delta",
                &serde_json::json!({
                    // TODO(phase-b review #12): when no `response.output_item.
                    // added` was seen for the message, `item_id` defaults to
                    // `""` and `output_index` to 0 - an empty item_id is
                    // malformed for strict Responses clients and index 0 can
                    // collide with a real item. Only reachable when healing
                    // fired AND the flushed tail contains text AND the
                    // upstream omitted output_item.added (the classic leak
                    // buffers the whole envelope, leaving an empty tail), so
                    // it is corner-of-corner; track a fallback id or skip the
                    // delta when untracked.
                    "type": "response.output_text.delta",
                    "item_id": self.message_item_id.clone().unwrap_or_default(),
                    "output_index": self.message_output_index.unwrap_or(0),
                    "delta": split.text
                }),
            ));
        }
    }

    /// Emit the function_call triple for each healed call.
    fn inject_calls(&mut self, calls: &[DsmlToolCall], out: &mut Vec<Bytes>) {
        for call in calls {
            let idx = self.next_index;
            self.next_index += 1;
            let fc_id = format!("fc_{}", uuid::Uuid::new_v4().simple());
            let call_id = synthesize_call_id();
            out.push(sse_block(
                "response.output_item.added",
                &serde_json::json!({
                    "type": "response.output_item.added", "output_index": idx,
                    "item": {"type": "function_call", "id": fc_id, "call_id": call_id,
                             "name": call.name, "arguments": "", "status": "in_progress"}
                }),
            ));
            out.push(sse_block(
                "response.function_call_arguments.delta",
                &serde_json::json!({
                    "type": "response.function_call_arguments.delta", "item_id": fc_id,
                    "output_index": idx, "delta": call.arguments
                }),
            ));
            out.push(sse_block(
                "response.output_item.done",
                &serde_json::json!({
                    "type": "response.output_item.done", "output_index": idx,
                    "item": {"type": "function_call", "id": fc_id, "call_id": call_id,
                             "name": call.name, "arguments": call.arguments, "status": "completed"}
                }),
            ));
            self.injected_calls.push((
                fc_id,
                call_id,
                call.name.clone(),
                call.arguments.clone(),
                idx,
            ));
        }
    }

    /// Emit healed reasoning, lazily creating the synthesized reasoning item.
    fn emit_reasoning(&mut self, chunk: &str, out: &mut Vec<Bytes>) {
        if self.reasoning.is_none() {
            let idx = self.next_index;
            self.next_index += 1;
            let id = format!("rs_{}", uuid::Uuid::new_v4().simple());
            out.push(sse_block(
                "response.output_item.added",
                &serde_json::json!({
                    "type": "response.output_item.added", "output_index": idx,
                    "item": {"type": "reasoning", "id": id,
                             "summary": [{"type": "summary_text", "text": ""}]}
                }),
            ));
            self.reasoning = Some((id, idx));
        }
        if let Some((id, idx)) = &self.reasoning {
            self.reasoning_text.push_str(chunk);
            out.push(sse_block(
                "response.reasoning_summary_text.delta",
                &serde_json::json!({
                    "type": "response.reasoning_summary_text.delta", "item_id": id,
                    "output_index": idx, "summary_index": 0, "delta": chunk
                }),
            ));
        }
    }
}

/// Replace the text of an item's output_text content parts in place.
fn set_message_text(item: &mut Value, text: &str) {
    if let Some(content) = item.get_mut("content").and_then(Value::as_array_mut) {
        for part in content.iter_mut() {
            if part.get("type").and_then(Value::as_str) == Some("output_text") {
                part["text"] = Value::String(text.to_string());
            }
        }
    }
}

/// Heal a non-streaming Responses JSON body in place (quirks `dsml_heal` +
/// `think_tags`, responses-format path).
///
/// For every `message` output item the text is run through the same
/// two-stage pipeline as streaming (DSML isolation first, then think
/// split). Healed calls are appended as `function_call` items; think-split
/// reasoning becomes a `reasoning` item inserted BEFORE its message
/// (canonical turn order, matching chat-path session storage). The original
/// bytes are returned unchanged when nothing fires, the body is not JSON,
/// or both gates are off.
pub fn heal_responses_body(body: &[u8], gates: HealGates) -> Vec<u8> {
    if !gates.dsml && !gates.think {
        return body.to_vec();
    }
    let Ok(mut val) = serde_json::from_slice::<Value>(body) else {
        return body.to_vec();
    };
    // Native OpenAI wraps items under `response.output`; some providers are
    // flat. Heal either shape, mirroring the streaming `rewrite_completed`.
    let target = if val.get("response").is_some() {
        val.get_mut("response").expect("checked above")
    } else {
        &mut val
    };
    let Some(outputs) = target.get_mut("output").and_then(Value::as_array_mut) else {
        return body.to_vec();
    };

    let mut fired = false;
    // (insert_before_index, reasoning item) and healed function_call items.
    let mut reasoning_inserts: Vec<(usize, Value)> = Vec::new();
    let mut fc_items: Vec<Value> = Vec::new();

    for (position, item) in outputs.iter_mut().enumerate() {
        if item.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        // Heal EVERY `output_text` content part (not just the first), so a
        // leak in a later part or after a non-text part is still caught -
        // mirroring the streaming path's `set_message_text`.
        let Some(parts) = item.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for part in parts.iter_mut() {
            if part.get("type").and_then(Value::as_str) != Some("output_text") {
                continue;
            }
            let Some(text) = part.get("text").and_then(Value::as_str).map(String::from) else {
                continue;
            };
            // Stage 1: DSML isolation (healed calls are pushed to
            // `fc_items` inline; only the cleaned text is carried on).
            let cleaned = if gates.dsml {
                match parse_leaked_tool_calls(&text) {
                    Some((cleaned, calls)) => {
                        fired = true;
                        for call in calls {
                            fc_items.push(serde_json::json!({
                                "type": "function_call",
                                "id": format!("fc_{}", uuid::Uuid::new_v4().simple()),
                                "call_id": synthesize_call_id(),
                                "name": call.name, "arguments": call.arguments,
                                "status": "completed"
                            }));
                        }
                        cleaned
                    }
                    None => text.clone(),
                }
            } else {
                text.clone()
            };
            // Stage 2: think split on the DSML-cleaned text.
            let final_text = if gates.think && contains_think_markup(&cleaned) {
                let mut filter = ThinkStreamFilter::new(true);
                let mut split = filter.push(&cleaned);
                let tail = filter.finish();
                split.reasoning.push_str(&tail.reasoning);
                split.text.push_str(&tail.text);
                // TODO(phase-b review #9): `fired` is only set when the think
                // split yields NON-empty reasoning, so an empty think block
                // (`<think></think>`) is stripped in memory (final_text write-back
                // below) but leaves `fired` false - the function then returns the
                // original `body` bytes and the markup survives on the wire,
                // unlike the streaming path (which strips empty tags via the
                // filter's own `fired`). Set `fired` from `filter.fired()` (any
                // open marker seen) instead of the reasoning length.
                if !split.reasoning.is_empty() {
                    fired = true;
                    reasoning_inserts.push((
                        position,
                        serde_json::json!({
                            "type": "reasoning",
                            "id": format!("rs_{}", uuid::Uuid::new_v4().simple()),
                            "summary": [{"type": "summary_text", "text": split.reasoning}]
                        }),
                    ));
                }
                split.text
            } else {
                cleaned
            };
            if final_text != text {
                part["text"] = Value::String(final_text);
            }
        }
    }

    if !fired {
        return body.to_vec();
    }
    // Apply inserts (reverse order so earlier indexes stay valid), append calls.
    for (position, rs) in reasoning_inserts.into_iter().rev() {
        outputs.insert(position, rs);
    }
    outputs.append(&mut fc_items);
    serde_json::to_vec(&val).unwrap_or_else(|_| body.to_vec())
}
