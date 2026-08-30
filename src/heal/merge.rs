//! Fragmented-items merger (responses-format path): collapse upstream SSE
//! runs of same-type output items (`message` / `reasoning` /
//! `function_call` with matching `call_id`) into a single Responses-
//! conformant item. Class-B heal analogous to [`ResponsesStreamHealer`]:
//! identity on healthy streams (run length always = 1), self-deactivating
//! when the upstream stops emitting fragmented runs.
//!
//! Spec: docs/superpowers/specs/2026-08-30-fragmented-items-merger-design.md
//!
//! Module split: this file owns the state machine; `mod.rs` re-exports the
//! public type. Mirrors the `responses.rs` / `dsml.rs` / `think.rs`
//! pattern.

use bytes::Bytes;
use serde_json::{json, Value};

/// The three item types the merger knows how to combine (spec §Q2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemType {
    Message,
    Reasoning,
    FunctionCall,
}

impl ItemType {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "message" => Some(ItemType::Message),
            "reasoning" => Some(ItemType::Reasoning),
            "function_call" => Some(ItemType::FunctionCall),
            _ => None,
        }
    }
}

/// Active-run state. `Some` means we've seen the first fragment of a run
/// (run length ≥ 1). The merge-mode suppression / rewriting kicks in only
/// when we observe the second same-type fragment.
///
/// Accumulator fields (`merged_text` / `merged_reasoning` /
/// `merged_arguments`) carry the run's accumulated payload between
/// fragments; the type-switch and `response.completed` flushes read
/// them when synthesizing `content_part.done` + `output_item.done`.
#[derive(Debug)]
struct RunState {
    item_type: ItemType,
    start_idx: usize,
    start_id: String,
    call_id: Option<String>,
    /// Accumulated message text (set when item_type == Message).
    /// Empty for non-message runs (spec §R2 + Q2).
    merged_text: String,
    /// Accumulated reasoning summary text (Reasoning). Empty otherwise.
    merged_reasoning: String,
    /// Accumulated function arguments (FunctionCall). Empty otherwise.
    merged_arguments: String,
    /// Whether the first fragment's `content_part.added` has been
    /// forwarded yet. Subsequent fragments' `content_part.added` are
    /// suppressed (spec §Event rewriting rules).
    part_added_emitted: bool,
}

/// Quirk gate wrapper around the per-request merger (spec §Interface).
#[derive(Debug)]
pub struct FragmentedItemMerger {
    /// Whether the `merge_fragmented` quirk is enabled for this request.
    /// When `false`, the merger is an identity over `push_event`/`finish`
    /// (matches `DsmlStreamFilter::new(false)` precedent).
    enabled: bool,
    /// Active tracked run, or `None` if no first fragment has been seen
    /// (or the previous run was discarded). Two same-type fragments in a
    /// row transition this from length-1 (passthrough) to length-≥2
    /// (suppression / rewriting of subsequent added events).
    run: Option<RunState>,
}

impl FragmentedItemMerger {
    /// A new merger honoring the per-request gate. `enabled = false`
    /// makes `push_event` return the input bytes verbatim and `finish`
    /// return empty — same posture as `DsmlStreamFilter::new(false)`.
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            run: None,
        }
    }

    /// Process one upstream SSE event; returns the byte chunks to forward.
    ///
    /// Task 5 wires the full event-handling matrix:
    /// - `output_item.added` — run tracker (`on_added`)
    /// - `output_text.delta` / `reasoning_summary_text.delta` /
    ///   `function_call_arguments.delta` — rewrite `item_id` /
    ///   `output_index` to the run's first fragment + accumulate
    ///   merged_* (`on_delta`)
    /// - `content_part.added` — forward first, suppress subsequent
    ///   (`on_content_part_added`)
    /// - `output_text.done` / `content_part.done` / `output_item.done` —
    ///   suppress; synthesized versions land at run boundaries
    ///   (`on_done`)
    /// - `response.completed` — flush any active run's synthesized done,
    ///   then forward verbatim (`on_completed`)
    /// - everything else — identity passthrough.
    pub fn push_event(&mut self, raw: &[u8], event: Option<&str>, data: &str) -> Vec<Bytes> {
        if !self.enabled {
            // disabled quirk: identity passthrough (matches
            // `DsmlStreamFilter::new(false)`). The gate is honored at the
            // call site in `passthrough.rs` (Task 8); this branch keeps the
            // merger a safe no-op when invoked unconditionally.
            return vec![Bytes::copy_from_slice(raw)];
        }
        match event {
            Some("response.output_item.added") => self.on_added(raw, data),
            Some(
                "response.output_text.delta"
                | "response.reasoning_summary_text.delta"
                | "response.function_call_arguments.delta",
            ) => self.on_delta(raw, data),
            Some("response.content_part.added") => self.on_content_part_added(raw),
            Some(
                "response.output_text.done"
                | "response.content_part.done"
                | "response.output_item.done",
            ) => self.on_done(raw),
            Some("response.completed") => self.on_completed(raw),
            _ => vec![Bytes::copy_from_slice(raw)],
        }
    }

    /// Dispatch an `output_item.added` event through the run tracker.
    ///
    /// First fragment: start a tracked run (run length = 1), pass through
    /// verbatim. Second fragment of the same type + same `call_id`
    /// (function_call only): suppress (run length ≥ 2; rewriting kicks in
    /// in Tasks 5–6). Different type or different `call_id`: discard the
    /// length-1 run (no merge content to flush), start a fresh run with
    /// the new fragment as its first member.
    fn on_added(&mut self, raw: &[u8], data: &str) -> Vec<Bytes> {
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return vec![Bytes::copy_from_slice(raw)];
        };
        let Some(item_type) = v
            .get("item")
            .and_then(|i| i.get("type"))
            .and_then(Value::as_str)
            .and_then(ItemType::from_str)
        else {
            // Unknown item type: pass through verbatim, no run tracking.
            return vec![Bytes::copy_from_slice(raw)];
        };
        let Some(new_id) = v
            .get("item")
            .and_then(|i| i.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            // Defensive: malformed item without an id — passthrough.
            return vec![Bytes::copy_from_slice(raw)];
        };
        let new_idx = v
            .get("output_index")
            .and_then(Value::as_u64)
            .map(|i| i as usize)
            .unwrap_or(0);
        let new_call_id = v
            .get("item")
            .and_then(|i| i.get("call_id"))
            .and_then(Value::as_str)
            .map(str::to_string);

        match self.run.as_ref() {
            None => {
                // First fragment of a run: pass through verbatim and
                // start tracking. The next same-type added event will
                // decide whether this becomes a merge (length ≥ 2).
                self.run = Some(RunState {
                    item_type,
                    start_idx: new_idx,
                    start_id: new_id,
                    call_id: new_call_id,
                    merged_text: String::new(),
                    merged_reasoning: String::new(),
                    merged_arguments: String::new(),
                    part_added_emitted: false,
                });
                vec![Bytes::copy_from_slice(raw)]
            }
            Some(run) => {
                let same_type = run.item_type == item_type;
                let same_call_id = match (run.call_id.as_ref(), new_call_id.as_ref()) {
                    // function_call: identity match on call_id is required.
                    (Some(a), Some(b)) => a == b,
                    // Non-function_call items: both sides carry no call_id;
                    // a non-tracked run plus a missing call_id matches.
                    (None, None) => true,
                    // Mixed (one Some, one None): never matches — surfaces
                    // malformed upstreams that drop or fabricate call_id.
                    _ => false,
                };
                if same_type && same_call_id {
                    // Second+ fragment of the same logical item: suppress.
                    // Run length is now ≥ 2; delta rewriting and done
                    // suppression (W3) and synthesis (W4/W5) follow.
                    Vec::new()
                } else {
                    // Different logical item: flush the prior run (synthesized
                    // content_part.done + output_item.done) and start
                    // fresh with the new fragment. Same-type/different-
                    // call_id (M7) and type-switch (M8/M9) both pass through
                    // here. The per-type merged_* gate skips the flush
                    // for length-1 runs that never accumulated content;
                    // Task 6 will tighten this once the full semantics
                    // for length-1 done suppression are nailed down.
                    let mut flushed = Vec::new();
                    if let Some(prior) = self.run.take() {
                        let should_flush = match prior.item_type {
                            ItemType::Message => !prior.merged_text.is_empty(),
                            ItemType::Reasoning => !prior.merged_reasoning.is_empty(),
                            ItemType::FunctionCall => !prior.merged_arguments.is_empty(),
                        };
                        if should_flush {
                            flushed.extend(self.flush_run_synthesis(prior));
                        }
                    }
                    self.run = Some(RunState {
                        item_type,
                        start_idx: new_idx,
                        start_id: new_id,
                        call_id: new_call_id,
                        merged_text: String::new(),
                        merged_reasoning: String::new(),
                        merged_arguments: String::new(),
                        part_added_emitted: false,
                    });
                    flushed.push(Bytes::copy_from_slice(raw));
                    flushed
                }
            }
        }
    }

    /// Delta rewriting (spec §Event rewriting rules — deltas):
    /// rewrite `item_id` and `output_index` to the run's first fragment,
    /// preserve `delta` text unchanged, accumulate into the per-type
    /// merged_* field. No run active: identity passthrough (defensive —
    /// every event with a tracked-item state should reach here through
    /// `on_added`, but the merger never assumes the upstream sent items
    /// in a specific order).
    fn on_delta(&mut self, raw: &[u8], data: &str) -> Vec<Bytes> {
        let Some(run) = self.run.as_ref() else {
            // No tracked run: identity, since a delta with no matching
            // item add must surface as-is.
            return vec![Bytes::copy_from_slice(raw)];
        };
        let Ok(mut v) = serde_json::from_str::<Value>(data) else {
            // Malformed JSON: identity rather than dropping.
            return vec![Bytes::copy_from_slice(raw)];
        };
        // Rewrite item_id + output_index to the run's first fragment so
        // the downstream ResponsesStreamHealer (and the client) see a
        // single coherent item across N upstream items.
        v["item_id"] = Value::String(run.start_id.clone());
        v["output_index"] = Value::Number(run.start_idx.into());
        // Accumulate the delta text into the per-type merged_* field.
        // The run borrow above ends before the take-and-replace below;
        // we cannot mutate `self.run` while it's borrowed as `&run`,
        // so we clone the values we need and re-borrow mutably only
        // for the push_str.
        let item_type = run.item_type;
        if let Some(delta) = v.get("delta").and_then(Value::as_str) {
            let owned = delta.to_owned();
            match item_type {
                ItemType::Message => self.run.as_mut().unwrap().merged_text.push_str(&owned),
                ItemType::Reasoning => self
                    .run
                    .as_mut()
                    .unwrap()
                    .merged_reasoning
                    .push_str(&owned),
                ItemType::FunctionCall => self
                    .run
                    .as_mut()
                    .unwrap()
                    .merged_arguments
                    .push_str(&owned),
            }
        }
        // Derive the event name from the data payload itself so the
        // outgoing SSE block is tagged correctly even when the merger
        // is invoked with `event = None` (defensive — the current
        // call sites pass `Some(...)`, but `push_event`'s signature
        // accepts both).
        let event_name = if data.contains("function_call_arguments") {
            "response.function_call_arguments.delta"
        } else if data.contains("reasoning_summary_text") {
            "response.reasoning_summary_text.delta"
        } else {
            "response.output_text.delta"
        };
        vec![sse_block(event_name, &v)]
    }

    /// First fragment's `content_part.added` passes through; subsequent
    /// fragments' are suppressed (the merge synthesizes one
    /// `content_part.done` at run end carrying the merged text, so only
    /// the first part.added belongs on the wire).
    fn on_content_part_added(&mut self, raw: &[u8]) -> Vec<Bytes> {
        let Some(run) = self.run.as_mut() else {
            // No tracked run: identity (defensive).
            return vec![Bytes::copy_from_slice(raw)];
        };
        if run.part_added_emitted {
            return Vec::new();
        }
        run.part_added_emitted = true;
        vec![Bytes::copy_from_slice(raw)]
    }

    /// Suppress upstream `output_text.done` / `content_part.done` /
    /// `output_item.done` when the active run has accumulated content.
    /// For length-1 runs (no merge content) or when no run is active,
    /// pass the upstream done through unchanged so the client sees the
    /// upstream's own close for that single fragment.
    ///
    /// The synthesized done for merged runs lands at run boundaries:
    /// type switch in `on_added` and `response.completed` in
    /// `on_completed`. Spec §Event rewriting rules (done events).
    fn on_done(&mut self, raw: &[u8]) -> Vec<Bytes> {
        let Some(run) = self.run.as_ref() else {
            return vec![Bytes::copy_from_slice(raw)];
        };
        let has_content = match run.item_type {
            ItemType::Message => !run.merged_text.is_empty(),
            ItemType::Reasoning => !run.merged_reasoning.is_empty(),
            ItemType::FunctionCall => !run.merged_arguments.is_empty(),
        };
        if has_content {
            // Merged run: suppress upstream done; the synthesized one from
            // flush_run_synthesis closes the run at the boundary.
            Vec::new()
        } else {
            // Length-1 run (or run with no deltas): upstream's own done
            // is the correct close for that fragment.
            vec![Bytes::copy_from_slice(raw)]
        }
    }

    /// Flush any active run's synthesized done, then forward the
    /// upstream's own `response.completed` verbatim. Same per-type
    /// gate as the type-switch flush in `on_added`.
    fn on_completed(&mut self, raw: &[u8]) -> Vec<Bytes> {
        let mut out = Vec::new();
        if let Some(run) = self.run.take() {
            let should_flush = match run.item_type {
                ItemType::Message => !run.merged_text.is_empty(),
                ItemType::Reasoning => !run.merged_reasoning.is_empty(),
                ItemType::FunctionCall => !run.merged_arguments.is_empty(),
            };
            if should_flush {
                out.extend(self.flush_run_synthesis(run));
            }
        }
        out.push(Bytes::copy_from_slice(raw));
        out
    }

    /// Synthesize the run's flush bytes: `content_part.done` (messages
    /// only) + `output_item.done` for message / reasoning / function_call
    /// based on the accumulated merged_* fields. Spec §Event rewriting
    /// rules. Called on type switches (`on_added`) and at
    /// `response.completed` (`on_completed`).
    ///
    /// Function name on the synthesized `output_item.done` for a merged
    /// function_call is `"<merged>"` — the merger doesn't recover a
    /// canonical name from fragments (the upstream is responsible for
    /// the name; if it fragmented the name too, the caller is broken).
    fn flush_run_synthesis(&self, run: RunState) -> Vec<Bytes> {
        let mut out = Vec::new();
        match run.item_type {
            ItemType::Message => {
                out.push(sse_block(
                    "response.content_part.done",
                    &json!({
                        "type": "response.content_part.done",
                        "item_id": run.start_id,
                        "output_index": run.start_idx,
                        "part": { "type": "output_text", "text": run.merged_text }
                    }),
                ));
                out.push(sse_block(
                    "response.output_item.done",
                    &json!({
                        "type": "response.output_item.done",
                        "output_index": run.start_idx,
                        "item": {
                            "type": "message",
                            "id": run.start_id,
                            "role": "assistant",
                            "status": "completed",
                            "content": [{ "type": "output_text", "text": run.merged_text }]
                        }
                    }),
                ));
            }
            ItemType::Reasoning => {
                out.push(sse_block(
                    "response.output_item.done",
                    &json!({
                        "type": "response.output_item.done",
                        "output_index": run.start_idx,
                        "item": {
                            "type": "reasoning",
                            "id": run.start_id,
                            "summary": [{ "type": "summary_text", "text": run.merged_reasoning }]
                        }
                    }),
                ));
            }
            ItemType::FunctionCall => {
                out.push(sse_block(
                    "response.output_item.done",
                    &json!({
                        "type": "response.output_item.done",
                        "output_index": run.start_idx,
                        "item": {
                            "type": "function_call",
                            "id": run.start_id,
                            "call_id": run.call_id.clone().unwrap_or_default(),
                            "name": "<merged>",
                            "arguments": run.merged_arguments,
                            "status": "completed"
                        }
                    }),
                ));
            }
        }
        out
    }

    /// Stream end. Returns nothing in this task — γ-1 (spec §Out of
    /// scope / state machine): never synthesize `output_item.done` at
    /// `finish()`. Passthrough's `response.failed` event handles
    /// truncated turns. `on_completed` flushes synthesized done from
    /// active runs on the `response.completed` boundary instead.
    pub fn finish(&mut self) -> Vec<Bytes> {
        self.run = None;
        Vec::new()
    }
}

/// Module-level helper: assemble one SSE event block. Mirrors
/// `src/heal/responses.rs::sse_block` (kept local to avoid coupling —
/// both modules use the same wire shape but evolve independently).
fn sse_block(event: &str, payload: &Value) -> Bytes {
    Bytes::from(format!("event: {event}\ndata: {payload}\n\n"))
}
