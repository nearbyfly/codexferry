//! Fragmented-items merger (responses-format path): collapse upstream SSE
//! runs of same-type output items (`message` / `reasoning` /
//! `function_call` with matching `call_id`) into a single Responses-
//! conformant item. Class-B heal analogous to [`ResponsesStreamHealer`]:
//! identity on healthy streams (run length always = 1), self-deactivating
//! when the upstream stops emitting fragmented runs.
//!
//! Spec: docs/superpowers/specs/2026-08-30-fragmented-items-merger-design.md
//!
//! Identity guarantee: the spec's state machine distinguishes *tracking*
//! (run length = 1 — everything passes through verbatim, including the
//! upstream's own done events and the `response.completed` payload) from
//! *merging* (run length ≥ 2 — subsequent added/done events are
//! suppressed, deltas rewritten, dones synthesized at run boundaries, and
//! the completed payload's output array rewritten). The wire is touched
//! ONLY once some run has actually merged; a healthy stream is
//! byte-identical to a disabled merger.
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

    /// The SSE delta event name that carries this item type's payload
    /// (`response.<...>.delta`). Used to validate that an incoming delta
    /// actually belongs to the tracked run's type before rewriting it.
    fn delta_event(&self) -> &'static str {
        match self {
            ItemType::Message => "response.output_text.delta",
            ItemType::Reasoning => "response.reasoning_summary_text.delta",
            ItemType::FunctionCall => "response.function_call_arguments.delta",
        }
    }
}

/// Active-run state. `Some` means we've seen the first fragment of a run
/// (run length ≥ 1). The merge-mode suppression / rewriting kicks in only
/// when the run observes its second same-type fragment (`len >= 2`).
///
/// Accumulator fields (`merged_text` / `merged_reasoning` /
/// `merged_arguments`) capture the run's payload from the FIRST delta on
/// — even while `len == 1` and the wire is untouched — because the second
/// fragment can arrive after the first fragment's deltas have already
/// been passed through verbatim; the synthesized done at run end must
/// carry the full text, not just the post-merge tail.
#[derive(Debug)]
struct RunState {
    item_type: ItemType,
    start_idx: usize,
    start_id: String,
    call_id: Option<String>,
    /// Function name captured from the first fragment of a function_call
    /// run (item_type == FunctionCall). The synthesized `output_item.done`
    /// and the rewritten `response.completed.output` array both use this
    /// so the client and the session store see a canonical tool name
    /// instead of the upstream's fragmented copies. `None` for non-
    /// function_call runs.
    start_name: Option<String>,
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
    /// Number of fragments observed in this run so far. 1 = tracking
    /// (identity on the wire); ≥ 2 = merging (suppress / rewrite /
    /// synthesize). This is the spec's 跟踪中 vs 合并中 distinction.
    len: usize,
    /// Item ids of every fragment in this run (start fragment first).
    /// Collected so `on_completed` can replace exactly these entries of
    /// the upstream `response.output` array with the single merged item.
    fragment_ids: Vec<String>,
}

/// A run that was merged and closed (type switch or `response.completed`).
/// Retained until the end of the response so the final
/// `response.completed.output` rewrite can splice every merged item back
/// in at its first fragment's position — earlier implementations
/// overwrote the array with only the last run's item, silently dropping
/// all preceding output (reasoning, earlier merged runs) from the client
/// stream and the session capture.
#[derive(Debug)]
struct MergedRunRecord {
    fragment_ids: Vec<String>,
    item: Value,
}

/// Quirk gate wrapper around the per-request merger (spec §Interface).
#[derive(Debug)]
pub struct FragmentedItemMerger {
    /// Whether the `merge_fragmented` quirk is enabled for this request.
    /// When `false`, the merger is an identity over `push_event`/`finish`
    /// (matches `DsmlStreamFilter::new(false)` precedent).
    enabled: bool,
    /// Active tracked run, or `None` if no first fragment has been seen
    /// (or the previous run was discarded / not mergeable).
    run: Option<RunState>,
    /// Closed merged runs (len ≥ 2 at close time), in stream order. Read
    /// by `on_completed` to rebuild `response.output`. Runs that never
    /// merged (len = 1) are NOT recorded — their upstream items stay in
    /// the completed payload untouched.
    merged_runs: Vec<MergedRunRecord>,
}

impl FragmentedItemMerger {
    /// A new merger honoring the per-request gate. `enabled = false`
    /// makes `push_event` return the input bytes verbatim and `finish`
    /// return empty — same posture as `DsmlStreamFilter::new(false)`.
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            run: None,
            merged_runs: Vec::new(),
        }
    }

    /// Process one upstream SSE event; returns the byte chunks to forward.
    ///
    /// Event matrix (spec §State machine):
    /// - `output_item.added` — run tracker (`on_added`); unknown item
    ///   types act as a type switch (flush + stop tracking)
    /// - `output_text.delta` / `reasoning_summary_text.delta` /
    ///   `function_call_arguments.delta` — rewrite `item_id` /
    ///   `output_index` to the run's first fragment + accumulate
    ///   merged_* (`on_delta`); verbatim while the run hasn't merged
    /// - `content_part.added` — forward first, suppress subsequent
    ///   (`on_content_part_added`)
    /// - `output_text.done` / `content_part.done` / `output_item.done` —
    ///   suppress once the run is merging; synthesized versions land at
    ///   run boundaries (`on_done`)
    /// - `response.completed` — flush any merging run's synthesized done,
    ///   then rewrite `response.output` if (and only if) some run merged
    ///   (`on_completed`)
    /// - everything else — identity passthrough.
    pub fn push_event(&mut self, raw: &[u8], event: Option<&str>, data: &str) -> Vec<Bytes> {
        if !self.enabled {
            // disabled quirk: identity passthrough (matches
            // `DsmlStreamFilter::new(false)`). The gate is honored at the
            // call site in `passthrough.rs`; this branch keeps the
            // merger a safe no-op when invoked unconditionally.
            return vec![Bytes::copy_from_slice(raw)];
        }
        match event {
            Some("response.output_item.added") => self.on_added(raw, data),
            Some(
                "response.output_text.delta"
                | "response.reasoning_summary_text.delta"
                | "response.function_call_arguments.delta",
            ) => self.on_delta(raw, event.unwrap(), data),
            Some("response.content_part.added") => self.on_content_part_added(raw, data),
            Some(
                "response.output_text.done"
                | "response.content_part.done"
                | "response.output_item.done",
            ) => self.on_done(raw, data),
            Some("response.completed") => self.on_completed(raw, data),
            _ => vec![Bytes::copy_from_slice(raw)],
        }
    }

    /// Dispatch an `output_item.added` event through the run tracker.
    ///
    /// First fragment: start a tracked run (len = 1), pass through
    /// verbatim. Second fragment of the same type + same `call_id`
    /// (function_call only): suppress (len ≥ 2 — merge mode begins).
    /// Different type, different `call_id`, or an item type the merger
    /// does not cover: close the prior run (synthesizing its dones only
    /// if it merged) and pass the new added through as the start of a
    /// fresh run — unknown types break the run entirely (spec: runs are
    /// strictly adjacent; a non-covered item in between means the next
    /// same-type item is a NEW logical item, not a continuation).
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
            // Unknown item type (file_search, web_search, …): flush any
            // merging run, stop tracking, pass through. Its done events
            // then flow through `on_done` untouched (no active run) —
            // never suppressed on behalf of a stale run.
            return self.close_run_then(vec![Bytes::copy_from_slice(raw)], None);
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
        let new_name = v
            .get("item")
            .and_then(|i| i.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string);

        let new_run = RunState {
            item_type,
            start_idx: new_idx,
            start_id: new_id.clone(),
            call_id: new_call_id,
            start_name: new_name,
            merged_text: String::new(),
            merged_reasoning: String::new(),
            merged_arguments: String::new(),
            part_added_emitted: false,
            len: 1,
            fragment_ids: vec![new_id],
        };

        let continues_run = match self.run.as_ref() {
            None => false,
            Some(run) => {
                let same_type = run.item_type == item_type;
                let same_call_id = match (run.call_id.as_ref(), new_run.call_id.as_ref()) {
                    // function_call: identity match on call_id is required.
                    (Some(a), Some(b)) => a == b,
                    // Non-function_call items: both sides carry no call_id.
                    (None, None) => true,
                    // Mixed (one Some, one None): never matches — surfaces
                    // malformed upstreams that drop or fabricate call_id.
                    _ => false,
                };
                same_type && same_call_id
            }
        };

        if continues_run {
            // Fragment 2..N of the same logical item: suppress the added,
            // remember the fragment id for the completed-output rewrite.
            let run = self.run.as_mut().expect("checked Some above");
            run.len += 1;
            run.fragment_ids.push(new_run.start_id);
            Vec::new()
        } else {
            // Different logical item: flush the prior run's synthesized
            // dones (only if it merged — a len=1 run's own upstream dones
            // already passed through and need no replacement), then start
            // tracking the new fragment.
            self.close_run_then(vec![Bytes::copy_from_slice(raw)], Some(new_run))
        }
    }

    /// Close the currently tracked run (if any), emitting its synthesized
    /// dones and recording it for the completed-output rewrite when it
    /// actually merged (len ≥ 2). Then install `new_run` (or clear
    /// tracking) and return the flush bytes followed by `passthrough`.
    fn close_run_then(
        &mut self,
        mut passthrough: Vec<Bytes>,
        new_run: Option<RunState>,
    ) -> Vec<Bytes> {
        let mut out = Vec::new();
        if let Some(run) = self.run.take() {
            if run.len >= 2 {
                let item = self.synthesize_merged_item_value(&run);
                out.extend(self.flush_run_synthesis(&run, &item));
                self.merged_runs.push(MergedRunRecord {
                    fragment_ids: run.fragment_ids,
                    item,
                });
            }
        }
        self.run = new_run;
        out.append(&mut passthrough);
        out
    }

    /// Delta handling (spec §Event rewriting rules — deltas).
    ///
    /// While the run is merely tracked (len = 1): forward verbatim — but
    /// still accumulate, because the run may merge later and the
    /// synthesized done must carry the full text. Once merged (len ≥ 2):
    /// rewrite `item_id` / `output_index` to the run's first fragment,
    /// preserve the `delta` text unchanged, accumulate. A delta whose
    /// event type does not match the tracked run's type (or arrives with
    /// no tracked run / unparseable JSON) is forwarded as-is — the merger
    /// never drops or rewrites events it cannot attribute.
    fn on_delta(&mut self, raw: &[u8], event: &str, data: &str) -> Vec<Bytes> {
        let Some(run) = self.run.as_ref() else {
            // No tracked run: identity, since a delta with no matching
            // item add must surface as-is.
            return vec![Bytes::copy_from_slice(raw)];
        };
        if event != run.item_type.delta_event() {
            // Delta belongs to an item type we're not tracking (e.g. an
            // untracked interleaved item): identity.
            return vec![Bytes::copy_from_slice(raw)];
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            // Malformed JSON: identity rather than dropping.
            return vec![Bytes::copy_from_slice(raw)];
        };
        // Accumulate from the first delta on (see RunState doc): the run
        // may merge after these deltas have already been forwarded.
        if let Some(delta) = v.get("delta").and_then(Value::as_str) {
            let run = self.run.as_mut().expect("checked Some above");
            match run.item_type {
                ItemType::Message => run.merged_text.push_str(delta),
                ItemType::Reasoning => run.merged_reasoning.push_str(delta),
                ItemType::FunctionCall => run.merged_arguments.push_str(delta),
            }
        }
        let run = self.run.as_ref().expect("checked Some above");
        if run.len < 2 {
            // Tracking only: byte-identical passthrough (healthy streams
            // stay identity — spec §State machine 跟踪中).
            return vec![Bytes::copy_from_slice(raw)];
        }
        let mut v = v;
        // Rewrite item_id + output_index to the run's first fragment so
        // the downstream ResponsesStreamHealer (and the client) see a
        // single coherent item across N upstream items.
        v["item_id"] = Value::String(run.start_id.clone());
        v["output_index"] = Value::Number(run.start_idx.into());
        vec![sse_block(event, &v)]
    }

    /// First fragment's `content_part.added` passes through; subsequent
    /// fragments' are suppressed (the merge synthesizes one
    /// `content_part.done` at run end carrying the merged text, so only
    /// the first part.added belongs on the wire). Suppression applies
    /// only to events owned by the tracked run (matching `item_id`) — a
    /// part.added for some other item interleaved into the run is
    /// forwarded untouched.
    fn on_content_part_added(&mut self, raw: &[u8], data: &str) -> Vec<Bytes> {
        let Some(run) = self.run.as_mut() else {
            // No tracked run: identity (defensive).
            return vec![Bytes::copy_from_slice(raw)];
        };
        if !event_owned_by_run(run, data) {
            return vec![Bytes::copy_from_slice(raw)];
        }
        if run.part_added_emitted {
            return Vec::new();
        }
        run.part_added_emitted = true;
        vec![Bytes::copy_from_slice(raw)]
    }

    /// Done suppression is gated on the run actually MERGING (len ≥ 2)
    /// AND the done belonging to that run (its item id matching a
    /// fragment of the run): a tracked (len = 1) run's upstream done is
    /// the item's only close and must pass through unchanged — this is
    /// what keeps healthy streams identity — and a done for some OTHER
    /// item interleaved into a merging run must not be eaten on the
    /// run's behalf. Once merging, the run's own upstream dones are
    /// suppressed (only the first fragment was ever announced to the
    /// client) and replaced by the single synthesized done at the run
    /// boundary — including zero-delta runs, whose later fragments'
    /// dones would otherwise close items the client never saw added.
    fn on_done(&mut self, raw: &[u8], data: &str) -> Vec<Bytes> {
        match self.run.as_ref() {
            Some(run) if run.len >= 2 && event_owned_by_run(run, data) => Vec::new(),
            _ => vec![Bytes::copy_from_slice(raw)],
        }
    }

    /// Flush any merging run's synthesized done, then forward the
    /// upstream's own `response.completed` — rewriting
    /// `response.output` only when at least one run merged: every
    /// recorded merged run's fragment items collapse into the single
    /// merged item (placed at the first fragment's position), and all
    /// other items pass through untouched. When nothing merged (healthy
    /// stream), the completed payload is forwarded byte-for-byte.
    ///
    /// `data` is the upstream `response.completed` JSON payload. If it
    /// fails to parse or lacks an output array while a rewrite is
    /// needed, the raw payload is forwarded unchanged (γ-1 fallback:
    /// never silently drop the closing event).
    fn on_completed(&mut self, raw: &[u8], data: &str) -> Vec<Bytes> {
        // Close the trailing run without a new one; its flush bytes
        // precede the completed event on the wire.
        let mut out = Vec::new();
        if let Some(run) = self.run.take() {
            if run.len >= 2 {
                let item = self.synthesize_merged_item_value(&run);
                out.extend(self.flush_run_synthesis(&run, &item));
                self.merged_runs.push(MergedRunRecord {
                    fragment_ids: run.fragment_ids,
                    item,
                });
            }
        }
        if self.merged_runs.is_empty() {
            // Healthy stream: no run ever merged — verbatim forward.
            out.push(Bytes::copy_from_slice(raw));
            return out;
        }
        let rewritten = serde_json::from_str::<Value>(data).ok().and_then(|mut v| {
            let output = match v.get_mut("response")?.get_mut("output")?.as_array_mut() {
                Some(arr) => arr,
                None => return None,
            };
            // Replace each merged run's fragments with the merged item at
            // the first fragment's position; drop the rest; keep every
            // unmerged item as-is.
            let mut new_output: Vec<Value> = Vec::with_capacity(output.len());
            let mut emitted: Vec<usize> = Vec::new();
            for item in output.iter() {
                let id = item.get("id").and_then(Value::as_str);
                let record = id.and_then(|id| {
                    self.merged_runs
                        .iter()
                        .position(|r| r.fragment_ids.iter().any(|f| f == id))
                });
                match record {
                    Some(idx) if !emitted.contains(&idx) => {
                        emitted.push(idx);
                        new_output.push(self.merged_runs[idx].item.clone());
                    }
                    Some(_) => {} // later fragment of an already-emitted run: drop
                    None => new_output.push(item.clone()),
                }
            }
            *output = new_output;
            Some(sse_block("response.completed", &v))
        });
        match rewritten {
            Some(block) => out.push(block),
            // Parse failure / missing output array: forward upstream raw
            // rather than dropping (γ-1 fallback).
            None => out.push(Bytes::copy_from_slice(raw)),
        }
        out
    }

    /// Build the JSON `Value` of the merged item in the canonical
    /// Responses item shape — used by `flush_run_synthesis` (the
    /// `item` field of the synthesized `output_item.done`) and by
    /// `on_completed` (each merged run's entry in the rewritten
    /// `response.output`). Keeping both call sites on the same helper
    /// guarantees the streaming close and the final completed payload
    /// never disagree.
    fn synthesize_merged_item_value(&self, run: &RunState) -> Value {
        match run.item_type {
            ItemType::Message => json!({
                "type": "message",
                "id": run.start_id,
                "role": "assistant",
                "status": "completed",
                "content": [{ "type": "output_text", "text": run.merged_text }]
            }),
            ItemType::Reasoning => json!({
                "type": "reasoning",
                "id": run.start_id,
                "summary": [{ "type": "summary_text", "text": run.merged_reasoning }]
            }),
            ItemType::FunctionCall => json!({
                "type": "function_call",
                "id": run.start_id,
                "call_id": run.call_id.clone().unwrap_or_default(),
                "name": run.start_name.clone().unwrap_or_else(|| "<merged>".to_string()),
                "arguments": run.merged_arguments,
                "status": "completed"
            }),
        }
    }

    /// Synthesize the run's flush bytes: `content_part.done` (messages
    /// only) + `output_item.done` for message / reasoning / function_call
    /// based on the accumulated merged_* fields. Spec §Event rewriting
    /// rules. Called on run boundaries — type switches (`on_added`) and
    /// `response.completed` (`on_completed`) — and only for runs that
    /// actually merged (len ≥ 2). `item` is the precomputed merged item
    /// value (shared with the completed-output rewrite so the two never
    /// disagree).
    fn flush_run_synthesis(&self, run: &RunState, item: &Value) -> Vec<Bytes> {
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
                        "item": item
                    }),
                ));
            }
            ItemType::Reasoning => {
                out.push(sse_block(
                    "response.output_item.done",
                    &json!({
                        "type": "response.output_item.done",
                        "output_index": run.start_idx,
                        "item": item
                    }),
                ));
            }
            ItemType::FunctionCall => {
                out.push(sse_block(
                    "response.output_item.done",
                    &json!({
                        "type": "response.output_item.done",
                        "output_index": run.start_idx,
                        "item": item
                    }),
                ));
            }
        }
        out
    }

    /// Stream end. Returns nothing — γ-1 (spec §Out of scope / state
    /// machine): never synthesize `output_item.done` at `finish()`.
    /// Passthrough's `response.failed` event handles truncated turns.
    /// `on_completed` flushes synthesized dones from merging runs on the
    /// `response.completed` boundary instead.
    ///
    /// `#[cfg_attr(not(test), allow(dead_code))]`: passthrough's
    /// streaming loop deliberately omits the call (γ-1 — the method
    /// exists for API symmetry with `ResponsesStreamHealer` and for
    /// direct unit-test invocation, not for the relay loop). Tests
    /// exercise it (see E3).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn finish(&mut self) -> Vec<Bytes> {
        self.run = None;
        self.merged_runs.clear();
        Vec::new()
    }
}

/// Whether an SSE event's data payload belongs to `run` by item id —
/// reads `item_id` (delta / part events) or `item.id`
/// (`output_item.done`). Unparseable payloads are assumed owned
/// (conservative: keeps suppression behavior for malformed fragments).
fn event_owned_by_run(run: &RunState, data: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(data) else {
        return true;
    };
    let id = v.get("item_id").and_then(Value::as_str).or_else(|| {
        v.get("item")
            .and_then(|i| i.get("id"))
            .and_then(Value::as_str)
    });
    match id {
        Some(id) => run.fragment_ids.iter().any(|f| f == id),
        // No id at all: assume it belongs to the run (upstreams that
        // omit ids on dones are exactly the broken ones we heal).
        None => true,
    }
}

/// Module-level helper: assemble one SSE event block. Mirrors
/// `src/heal/responses.rs::sse_block` (kept local to avoid coupling —
/// both modules use the same wire shape but evolve independently).
fn sse_block(event: &str, payload: &Value) -> Bytes {
    Bytes::from(format!("event: {event}\ndata: {payload}\n\n"))
}
