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

/// Quirk gate wrapper around the per-request merger.
///
/// The merger tracks an active run of same-type `output_item.added`
/// events (spec §State machine). A run with length ≥ 2 triggers the
/// merging path; length 1 (the healthy case) is a verbatim passthrough.
#[derive(Debug)]
pub struct FragmentedItemMerger {
    /// Whether the `merge_fragmented` quirk is enabled for this request.
    /// When `false`, the merger is an identity over `push_event`/`finish`.
    enabled: bool,
}

impl FragmentedItemMerger {
    /// A new merger honoring the per-request gate. `enabled = false`
    /// makes `push_event` return the input bytes verbatim and `finish`
    /// return empty — same posture as `DsmlStreamFilter::new(false)`.
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    /// Process one upstream SSE event; returns the byte chunks to forward.
    ///
    /// In Task 2 this is identity (full state machine lands in Tasks 3–7).
    /// The signature mirrors `ResponsesStreamHealer::push_event` so the
    /// passthrough wiring in Task 9 is a drop-in chain.
    pub fn push_event(&mut self, raw: &[u8], _event: Option<&str>, _data: &str) -> Vec<Bytes> {
        if self.enabled {
            vec![Bytes::copy_from_slice(raw)]
        } else {
            // disabled quirk: drop the event entirely (K1 fixture).
            Vec::new()
        }
    }

    /// Stream end. Returns nothing in this task; Tasks 5–6 will flush
    /// synthesized `content_part.done` / `output_item.done` from active
    /// runs on the response.completed boundary instead.
    pub fn finish(&mut self) -> Vec<Bytes> {
        Vec::new()
    }
}
