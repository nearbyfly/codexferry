# Fragmented-Items Merger Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `merge_fragmented` heal pass that collapses MiniMax M3 Responses-style fragmented output items (5–14 `output_item.added` events for one logical item, NOTES-2026-08-28 §2) into a single Responses-conformant item, so Codex TUI renders one assistant reply as one bullet.

**Architecture:** New `src/heal/merge.rs` exposes `FragmentedItemMerger` with the same `push_event/finish` API as `ResponsesStreamHealer`. The merger tracks an active "run" of same-type items (message / reasoning / function_call with matching `call_id`), rewrites downstream events to use the first fragment's `item_id` / `output_index`, and suppresses all `output_item.done` / `content_part.done` from the run — emitting synthesized dones at the run boundary (or at `response.completed`). It sits in front of `ResponsesStreamHealer` in `passthrough.rs`'s healed branch. Chat path is untouched (`StreamConverter` is naturally unfragmented, AGENTS.md §8a invariant).

**Tech Stack:** Rust (axum, bytes, serde_json, tracing); no new dependencies.

**Spec:** `docs/superpowers/specs/2026-08-30-fragmented-items-merger-design.md` — read it first; this plan argues from it.

## Global Constraints

- No new crate dependencies.
- Comments in English; `//!` module docs accurate; reference spec sections for non-obvious behavior (AGENTS.md #10).
- Tracing strings use `{field}` placeholders, never positional `{}` (AGENTS.md #3).
- All test runs from the repo root: `cargo test …`. Commit after every task.
- `merge_fragmented` defaults ON; off via `[quirks] disabled = ["merge_fragmented"]` (spec §Kill switch & defaults).
- `HealGates::default()` becomes `{ dsml: true, think: true, merge_fragmented: true }` (spec §Interface).
- Chat path is untouched; `StreamConverter` invariants preserved (spec §Overview).
- E2E deferred per NOTES-2026-08-28 §6.3 ("启动时机：再次复现时").
- Branch: `main` (spec §Branch).
- Public API names: `FragmentedItemMerger`, `RunState` (private), `ItemType` (private enum). API signature mirrors `ResponsesStreamHealer::push_event(raw: &[u8], event: Option<&str>, data: &str) -> Vec<Bytes>`.

---

### Task 1: Register `merge_fragmented` quirk + extend `HealGates`

**Files:**
- Modify: `src/quirks.rs:17` (`QUIRK_NAMES` constant)
- Modify: `src/heal/mod.rs:18-25` (`HealGates` struct + Default impl) and `src/heal/mod.rs:53-63` (module list + re-exports + tests)
- Test: existing inline tests in `src/quirks.rs:48-62` and `src/config.rs:723-758`

**Interfaces:**
- Consumes: nothing new.
- Produces:
  - `QUIRK_NAMES` now includes `"merge_fragmented"`.
  - `HealGates { dsml: bool, think: bool, merge_fragmented: bool }`.
  - `HealGates::default()` returns `{ dsml: true, think: true, merge_fragmented: true }`.

- [ ] **Step 1: Verify the failing tests**

The existing tests must currently fail (or compilation must fail) because the new field doesn't exist. Run:

```bash
cargo test --no-run 2>&1 | head -40
```

Expected: compile errors of the form `no field merge_fragmented on type HealGates` and `unknown quirk name merge_fragmented` (from `unknown_quirk_names` warn path; first compile, this is benign). If they don't fail, do not proceed — investigate.

Then append two tests to `src/config.rs` (inside the `quirks_config_tests` mod, near the existing tests around `src/config.rs:723-758`):

```rust
    #[test]
    fn merge_fragmented_defaults_to_enabled() {
        let cfg = parse_base_config();
        assert!(cfg.quirk_enabled("merge_fragmented"));
    }

    #[test]
    fn merge_fragmented_disables_via_list() {
        let cfg = parse_with_disabled(&["merge_fragmented"]);
        assert!(!cfg.quirk_enabled("merge_fragmented"));
        // other quirks unaffected
        assert!(cfg.quirk_enabled("dsml_heal"));
        assert!(cfg.quirk_enabled("think_tags"));
    }
```

`parse_base_config` / `parse_with_disabled` are existing helpers in the same test mod (re-use them; if their names differ, copy the patterns verbatim). Run:

```bash
cargo test merge_fragmented
```

Expected: FAIL — `no field merge_fragmented on type HealGates` (compile error).

- [ ] **Step 2: Implement**

In `src/quirks.rs`, append `"merge_fragmented"` to `QUIRK_NAMES`:

```rust
pub const QUIRK_NAMES: &[&str] = &[
    "glm_thinking",
    "missing_done",
    "dsml_heal",
    "think_tags",
    "merge_fragmented",
];
```

In `src/heal/mod.rs`, replace the `HealGates` struct + its `Default` impl:

```rust
pub struct HealGates {
    /// Quirk `dsml_heal`: heal leaked DSML tool-call markup.
    pub dsml: bool,
    /// Quirk `think_tags`: split leaked `<think>` markup onto the
    /// reasoning channel.
    pub think: bool,
    /// Quirk `merge_fragmented`: collapse upstream Responses SSE runs of
    /// same-type output items (e.g. MiniMax M3's per-chunk item
    /// fragmentation, NOTES-2026-08-28 §2) into a single Responses-
    /// conformant item. See
    /// docs/superpowers/specs/2026-08-30-fragmented-items-merger-design.md.
    pub merge_fragmented: bool,
}

impl Default for HealGates {
    /// All quirks default on: `[quirks] disabled = [...]` opts OUT, so the
    /// derived all-false default would silently disable healing everywhere.
    fn default() -> Self {
        HealGates {
            dsml: true,
            think: true,
            merge_fragmented: true,
        }
    }
}
```

- [ ] **Step 3: Run tests to verify they pass**

```bash
cargo test merge_fragmented
cargo test
```

Expected: PASS for `merge_fragmented` (2 tests) and `--lib` (no regressions; the new field's default-true matches the existing deny-by-default philosophy).

- [ ] **Step 4: Verify all upstream `HealGates::new` callers compile**

`HealGates` currently has two construction sites: `src/proxy/chat.rs:46-49` (chat path) and `src/proxy/passthrough.rs:45-48` (responses path). Both use the struct literal syntax `{ dsml, think }` — adding a third field **will break their compile** until Task 9 wires them.

Run:

```bash
cargo build 2>&1 | head -20
```

Expected: compile error `missing field merge_fragmented` at `src/proxy/chat.rs:46` and `src/proxy/passthrough.rs:45`. **This is expected at the end of Task 1** — do not fix yet; it is fixed in Task 9. Document this expectation in the commit message (Step 5).

- [ ] **Step 5: Commit**

```bash
git add src/quirks.rs src/heal/mod.rs src/config.rs
git commit -m "feat(heal): add merge_fragmented quirk name + HealGates field

Pre-work for the fragmented-items merger (see spec
docs/superpowers/specs/2026-08-30-fragmented-items-merger-design.md).

Does not yet wire the new gate anywhere; proxy/chat.rs and
proxy/passthrough.rs now fail to compile (missing field) until
the merger is implemented and the gates are read in the passthrough
path (next tasks)."
```

---

### Task 2: `FragmentedItemMerger` skeleton + single-item fixtures (M1, M3, M5, K1)

**Files:**
- Create: `src/heal/merge.rs` (full module; skeleton supports the passthrough identity case, plus first 4 fixtures)
- Create: `src/heal/merge_tests.rs` (with `#[cfg(test)] mod tests` re-exported via `src/heal/mod.rs`)
- Modify: `src/heal/mod.rs:53-63` (add `mod merge;` + `pub use merge::FragmentedItemMerger;` + `#[cfg(test)] mod merge_tests;`)

**Interfaces (initial scaffold):**
- `pub struct FragmentedItemMerger { enabled: bool }` — holds the gate.
- `pub fn new(enabled: bool) -> Self`.
- `pub fn push_event(&mut self, raw: &[u8], _event: Option<&str>, _data: &str) -> Vec<Bytes>` — **identity** in this task: returns the raw bytes verbatim.
- `pub fn finish(&mut self) -> Vec<Bytes>` — returns empty.
- (The full state machine comes in Task 3; this task proves the skeleton compiles and is wired into `HealGates`-using callers.)

- [ ] **Step 1: Create the skeleton `src/heal/merge.rs`**

```rust
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
```

- [ ] **Step 2: Wire `mod.rs`**

In `src/heal/mod.rs`, add to the existing module list (after `mod responses;`):

```rust
mod merge;
```

And add to the public re-exports (after the `responses::{...}` line):

```rust
pub use merge::FragmentedItemMerger;
```

And register the tests module (after the existing `#[cfg(test)] mod responses_healer_tests;`):

```rust
#[cfg(test)]
mod merge_tests;
```

- [ ] **Step 3: Create `src/heal/merge_tests.rs`**

```rust
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
```

- [ ] **Step 4: Run the four fixtures**

```bash
cargo test merge_tests
```

Expected: PASS — 4 tests.

- [ ] **Step 5: Commit**

```bash
git add src/heal/merge.rs src/heal/merge_tests.rs src/heal/mod.rs
git commit -m "feat(heal): FragmentedItemMerger skeleton + passthrough fixtures

Adds the merger module with a no-op push_event/finish (identity when
enabled, drop-all when disabled) so the wiring in Task 9 has a stable
shape to plug into. Covers M1/M3/M5 (single-item passthrough) and
K1 (kill switch) — the merging logic arrives in Tasks 3–7."
```

---

### Task 3: Same-type run merging (M2, M4, M6, M7)

**Files:**
- Modify: `src/heal/merge.rs` — add state machine fields, extend `push_event`
- Modify: `src/heal/merge_tests.rs` — add 4 fixtures

**Interfaces:**
- `FragmentedItemMerger` gains a `run: Option<RunState>` field and a private `ItemType` enum + `RunState` struct.
- `push_event` learns to detect consecutive same-type `output_item.added` events and rewrite downstream deltas to the first fragment's `item_id`/`output_index`.

- [ ] **Step 1: Add the failing fixtures**

Append to `src/heal/merge_tests.rs`:

```rust
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
```

Run:

```bash
cargo test merge_tests::m2_
cargo test merge_tests::m4_
cargo test merge_tests::m6_
cargo test merge_tests::m7_
```

Expected: `m2_`/`m4_`/`m6_` FAIL on `out1.is_empty()`; `m7_` FAIL on `assert_eq!(concat(out1), added(1, "call_b"))` (current identity impl returns the bytes, which would actually pass — but the test is correct: different call_id starts a new tracked run, not a fresh passthrough). **Note:** because the Task 2 implementation is identity, M2/M4/M6 fail on the suppression assertion. M7 currently PASSES with the wrong semantics — fix that as part of this task by introducing the run tracker.

- [ ] **Step 2: Run the failing fixtures to confirm they fail**

Run:

```bash
cargo test merge_tests::m2_message_run_suppresses_subsequent_added
cargo test merge_tests::m4_reasoning_run_suppresses_subsequent_added
cargo test merge_tests::m6_function_call_same_call_id_merges
cargo test merge_tests::m7_function_call_different_call_ids_dont_merge
```

Expected: `m2_*` / `m4_*` / `m6_*` FAIL (assertion `out1.is_empty()` does not hold under Task 2's identity impl). `m7_*` PASSES with current Task 2 semantics, but the spec-correct behavior requires the run tracker; the next step introduces it. If `m2_*`/`m4_*`/`m6_*` happen to pass already, do not proceed — investigate before changing production code.

- [ ] **Step 3: Implement state machine (ItemType, RunState, on_added)**

Replace `src/heal/merge.rs` body with:

```rust
//! (existing module docs preserved at top)

use bytes::Bytes;
use serde_json::Value;

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
#[derive(Debug)]
struct RunState {
    item_type: ItemType,
    start_idx: usize,
    start_id: String,
    call_id: Option<String>,
}

/// Quirk gate wrapper around the per-request merger (spec §Interface).
#[derive(Debug)]
pub struct FragmentedItemMerger {
    enabled: bool,
    run: Option<RunState>,
}

impl FragmentedItemMerger {
    pub fn new(enabled: bool) -> Self {
        Self { enabled, run: None }
    }

    /// Process one upstream SSE event; returns the byte chunks to forward.
    ///
    /// In this task, only `output_item.added` handling changes from
    /// Task 2's identity. Delta rewriting, done suppression, and
    /// synthesis land in Tasks 5–6.
    pub fn push_event(&mut self, raw: &[u8], event: Option<&str>, data: &str) -> Vec<Bytes> {
        if !self.enabled {
            return Vec::new();
        }
        match event {
            Some("response.output_item.added") => self.on_added(raw, data),
            _ => vec![Bytes::copy_from_slice(raw)],
        }
    }

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
                // First fragment: pass through, start tracking.
                self.run = Some(RunState {
                    item_type,
                    start_idx: new_idx,
                    start_id: new_id,
                    call_id: new_call_id,
                });
                vec![Bytes::copy_from_slice(raw)]
            }
            Some(run) => {
                let same_type = run.item_type == item_type;
                let same_call_id = match (run.call_id.as_ref(), new_call_id.as_ref()) {
                    (Some(a), Some(b)) => a == b,
                    // Non-function_call items don't gate on call_id; for
                    // function_call both sides must carry it.
                    (None, None) => true,
                    _ => false,
                };
                if same_type && same_call_id {
                    // Second+ fragment: suppress. Run length is now ≥ 2.
                    Vec::new()
                } else {
                    // Different logical item: discard the tracked run
                    // (length-1 had no merge content to flush) and
                    // start fresh with the new fragment.
                    self.run = Some(RunState {
                        item_type,
                        start_idx: new_idx,
                        start_id: new_id,
                        call_id: new_call_id,
                    });
                    vec![Bytes::copy_from_slice(raw)]
                }
            }
        }
    }

    pub fn finish(&mut self) -> Vec<Bytes> {
        // γ-1: never synthesize done at finish() — passthrough.rs's
        // response.failed event handles truncated turns.
        self.run = None;
        Vec::new()
    }
}
```

- [ ] **Step 4: Run the four fixtures to verify they pass**

```bash
cargo test merge_tests
```

Expected: PASS — all 8 tests (4 from Task 2 + 4 new).

- [ ] **Step 5: Commit**

```bash
git add src/heal/merge.rs src/heal/merge_tests.rs
git commit -m "feat(heal): FragmentedItemMerger detects same-type runs (M2/M4/M6/M7)

Adds the state machine's run tracker with ItemType discrimination and
call_id matching for function_call. The first fragment of a run passes
through verbatim; subsequent same-type fragments are suppressed (M2/M4/M6).
Different call_id or different type starts a fresh tracked run instead
of merging (M7 + Task 4 work).

Delta rewriting, done suppression, and synthesis land in Tasks 5–6."
```

---

### Task 4: Type-switching and interleaved runs (M8, M9)

**Files:**
- Modify: `src/heal/merge.rs` — add type-switch flush + accumulation fields
- Modify: `src/heal/merge_tests.rs` — add 2 fixtures

**Interfaces:**
- `RunState` gains `merged_text: String`, `merged_reasoning: String`, `merged_arguments: String`, `part_added_emitted: bool` (initialized to false).
- `push_event` learns to flush a run on type switch (`content_part.done` + `output_item.done` synthesis).

- [ ] **Step 1: Add the failing fixtures**

Append to `src/heal/merge_tests.rs`:

```rust
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
```

Run:

```bash
cargo test merge_tests::m8_
cargo test merge_tests::m9_
```

Expected: `m8_` PASSES already (Task 3's impl already routes type-switches as fresh runs). `m9_` FAIL on `out_rs == rs` and `out3 == msg(3)` (current Task 3 impl doesn't handle the type-switch flush correctly when the prior run had length ≥ 2).

- [ ] **Step 2: Extend RunState + add flush helper**

Replace `RunState` in `src/heal/merge.rs`:

```rust
#[derive(Debug)]
struct RunState {
    item_type: ItemType,
    start_idx: usize,
    start_id: String,
    call_id: Option<String>,
    /// Accumulated message text (set when item_type == Message).
    merged_text: String,
    /// Accumulated reasoning summary text (Reasoning).
    merged_reasoning: String,
    /// Accumulated function arguments (FunctionCall).
    merged_arguments: String,
    /// Whether the first fragment's `content_part.added` has been
    /// forwarded yet (Task 5 will implement content_part.added handling;
    /// this field is wired up now for forward compatibility).
    part_added_emitted: bool,
}
```

Initialize the new fields in the two places that construct `RunState` (both in `on_added`, Task 3):

```rust
RunState {
    item_type,
    start_idx: new_idx,
    start_id: new_id,
    call_id: new_call_id,
    merged_text: String::new(),
    merged_reasoning: String::new(),
    merged_arguments: String::new(),
    part_added_emitted: false,
}
```

(No other change needed in Task 3's logic — the existing impl already handles type switches via "discard tracked run + start fresh". The merge-mode for the discarded run is Task 5's job: flush synthesized done BEFORE starting the new run.)

- [ ] **Step 3: Add a type-switch flush in `on_added`**

Replace the else-branch of `on_added` (the "Different logical item" path):

```rust
                } else {
                    // Different logical item: flush the prior run
                    // (synthesized content_part.done + output_item.done
                    // land in Task 5; for now we just clear state) and
                    // start fresh with the new fragment.
                    let mut flushed = Vec::new();
                    if let Some(run) = self.run.take() {
                        if run.item_type != ItemType::Message || !run.merged_text.is_empty() {
                            // Tasks 5–6 wire actual flush output here. For
                            // now we leave the placeholder so M9's
                            // behavior is correct: the prior run is
                            // cleared, the new fragment passes through.
                            flushed.extend(self.flush_run_synthesis_placeholder(run));
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
```

Add the placeholder helper (no-op for now; Task 5 fills it in):

```rust
    /// Stub: synthesize the run's flush bytes. Task 5 implements
    /// `content_part.done` + `output_item.done` based on `RunState`'s
    /// accumulated `merged_*` fields. The current Task 4 stub returns
    /// empty so M9's existing assertion (the new fragment passes through)
    /// holds.
    fn flush_run_synthesis_placeholder(&self, _run: RunState) -> Vec<Bytes> {
        Vec::new()
    }
```

- [ ] **Step 4: Run the two new fixtures**

```bash
cargo test merge_tests
```

Expected: PASS — 10 tests total. (M8 already passes; M9 now passes because the type-switch path correctly discards the prior tracked run and starts fresh.)

- [ ] **Step 5: Commit**

```bash
git add src/heal/merge.rs src/heal/merge_tests.rs
git commit -m "feat(heal): merger handles type switches + interleaved runs (M8/M9)

Adds merged_* accumulation fields to RunState and a flush placeholder
on type switches. The actual content_part.done / output_item.done
synthesis lands in Task 5 (W1–W5 fixtures); the placeholder is a no-op
so the type-switch correctness is testable now."
```

---

### Task 5: Delta rewriting + synthesized done (W1–W5)

**Files:**
- Modify: `src/heal/merge.rs` — replace placeholder, add `on_delta`/`on_done` arms
- Modify: `src/heal/merge_tests.rs` — add 5 fixtures

**Interfaces:**
- `push_event` learns `response.output_text.delta` / `response.reasoning_summary_text.delta` / `response.function_call_arguments.delta`: rewrite `item_id` + `output_index` to run start, append to merged field, return rewritten event bytes.
- `push_event` learns `content_part.added`: first one passes through, subsequent suppressed.
- `push_event` learns `output_text.done` / `content_part.done` / `output_item.done` in merge mode: suppress (the synthesized ones land at run boundary).
- `flush_run_synthesis_placeholder` becomes `flush_run_synthesis` and emits the synthesized `content_part.done` + `output_item.done` based on accumulated fields.

- [ ] **Step 1: Add the failing fixtures**

Append to `src/heal/merge_tests.rs`:

```rust
/// Spec W1: subsequent-fragment deltas have `item_id` and
/// `output_index` rewritten to the first fragment's.
#[test]
fn w1_subsequent_deltas_rewritten_to_first_fragment() {
    let mut m = FragmentedItemMerger::new(true);
    let msg = |idx: u64, id: &str| sse("response.output_item.added",
        &format!(r#"{{"type":"response.output_item.added","output_index":{idx},"item":{{"type":"message","id":"{id}","role":"assistant","status":"in_progress","content":[]}}}}"#));
    let delta = |item_id: &str, idx: u64, text: &str| sse("response.output_text.delta",
        &format!(r#"{{"type":"response.output_text.delta","item_id":"{item_id}","output_index":{idx},"delta":"{text}"}}"#));
    let _ = push_raw(&mut m, &msg(0, "msg_0"));
    let _ = push_raw(&mut m, &msg(1, "msg_9"));  // suppress; run length ≥ 2
    let d1 = push_raw(&mut m, &delta("msg_0", 0, "Hello "));
    let d2 = push_raw(&mut m, &delta("msg_9", 1, "world"));  // rewrite to msg_0, idx 0
    // d1 is identity (msg_0's own delta)
    let d2_bytes = concat(d2);
    let d2_str = std::str::from_utf8(&d2_bytes).unwrap();
    assert!(d2_str.contains(r#""item_id":"msg_0""#), "d2 item_id rewritten: {d2_str}");
    assert!(d2_str.contains(r#""output_index":0"#), "d2 output_index rewritten: {d2_str}");
    assert!(d2_str.contains(r#""delta":"world""#), "d2 text unchanged: {d2_str}");
}

/// Spec W2: subsequent `content_part.added` from later fragments is
/// suppressed (the first fragment's already emitted).
#[test]
fn w2_subsequent_content_part_added_suppressed() {
    let mut m = FragmentedItemMerger::new(true);
    let msg = |id: &str| sse("response.output_item.added",
        &format!(r#"{{"type":"response.output_item.added","output_index":0,"item":{{"type":"message","id":"{id}","role":"assistant","status":"in_progress","content":[]}}}}"#));
    let part = |item_id: &str| sse("response.content_part.added",
        &format!(r#"{{"type":"response.content_part.added","item_id":"{item_id}","output_index":0,"part":{{"type":"output_text","text":""}}}}"#));
    let _ = push_raw(&mut m, &msg("msg_0"));
    let _ = push_raw(&mut m, &msg("msg_1"));
    let p0 = push_raw(&mut m, &part("msg_0"));  // first: pass through
    let p1 = push_raw(&mut m, &part("msg_1"));  // second: suppress
    assert!(!p0.is_empty());
    assert!(p1.is_empty(), "subsequent content_part.added suppressed");
}

/// Spec W3: subsequent-fragment `output_item.done` and `content_part.done`
/// are suppressed (synthesized versions land at run boundary — Task 5
/// later part / Task 6 verifies).
#[test]
fn w3_subsequent_dones_suppressed() {
    let mut m = FragmentedItemMerger::new(true);
    let msg = |id: &str| sse("response.output_item.added",
        &format!(r#"{{"type":"response.output_item.added","output_index":0,"item":{{"type":"message","id":"{id}","role":"assistant","status":"in_progress","content":[]}}}}"#));
    let cpd = |item_id: &str| sse("response.content_part.done",
        &format!(r#"{{"type":"response.content_part.done","item_id":"{item_id}","output_index":0}}"#));
    let oid = |item_id: &str| sse("response.output_item.done",
        &format!(r#"{{"type":"response.output_item.done","item_id":"{item_id}","output_index":0,"item":{{"type":"message","id":"{item_id}","status":"completed"}}}}"#));
    let _ = push_raw(&mut m, &msg("msg_0"));
    let _ = push_raw(&mut m, &msg("msg_1"));
    let cpd_out = push_raw(&mut m, &cpd("msg_1"));
    let oid_out = push_raw(&mut m, &oid("msg_1"));
    assert!(cpd_out.is_empty(), "msg_1's content_part.done suppressed");
    assert!(oid_out.is_empty(), "msg_1's output_item.done suppressed");
}

/// Spec W4: when a type switch flushes the run, the synthesized
/// `content_part.done` carries the merged text accumulated from the run.
#[test]
fn w4_synthesized_content_part_done_carries_merged_text() {
    let mut m = FragmentedItemMerger::new(true);
    let msg = |id: &str| sse("response.output_item.added",
        &format!(r#"{{"type":"response.output_item.added","output_index":0,"item":{{"type":"message","id":"{id}","role":"assistant","status":"in_progress","content":[]}}}}"#));
    let delta = |item_id: &str, text: &str| sse("response.output_text.delta",
        &format!(r#"{{"type":"response.output_text.delta","item_id":"{item_id}","output_index":0,"delta":"{text}"}}"#));
    let rs = sse("response.output_item.added",
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"reasoning","id":"rs_0","summary":[{"type":"summary_text","text":""}]}}"#);
    let _ = push_raw(&mut m, &msg("msg_0"));
    let _ = push_raw(&mut m, &msg("msg_1"));  // suppress; merge mode
    let _ = push_raw(&mut m, &delta("msg_0", "Hi "));
    let _ = push_raw(&mut m, &delta("msg_1", "there"));  // rewrite
    // Type switch to reasoning triggers run flush.
    let rs_out = push_raw(&mut m, &rs);
    let all: Vec<u8> = rs_out.into_iter().flat_map(|b| b.to_vec()).collect();
    let all_str = std::str::from_utf8(&all).unwrap();
    assert!(all_str.contains("response.content_part.done"), "synthesized cpd present: {all_str}");
    assert!(all_str.contains(r#""text":"Hi there""#), "merged text 'Hi there' present: {all_str}");
}

/// Spec W5: the synthesized `output_item.done` item content has the
/// merged text.
#[test]
fn w5_synthesized_output_item_done_has_merged_content() {
    let mut m = FragmentedItemMerger::new(true);
    let msg = |id: &str| sse("response.output_item.added",
        &format!(r#"{{"type":"response.output_item.added","output_index":0,"item":{{"type":"message","id":"{id}","role":"assistant","status":"in_progress","content":[]}}}}"#));
    let delta = |item_id: &str, text: &str| sse("response.output_text.delta",
        &format!(r#"{{"type":"response.output_text.delta","item_id":"{item_id}","output_index":0,"delta":"{text}"}}"#));
    let rs = sse("response.output_item.added",
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"reasoning","id":"rs_0","summary":[{"type":"summary_text","text":""}]}}"#);
    let _ = push_raw(&mut m, &msg("msg_0"));
    let _ = push_raw(&mut m, &msg("msg_1"));
    let _ = push_raw(&mut m, &delta("msg_0", "abc"));
    let _ = push_raw(&mut m, &delta("msg_1", "def"));
    let rs_out = push_raw(&mut m, &rs);
    let all: Vec<u8> = rs_out.into_iter().flat_map(|b| b.to_vec()).collect();
    let all_str = std::str::from_utf8(&all).unwrap();
    assert!(all_str.contains("response.output_item.done"), "synthesized oid present: {all_str}");
    assert!(all_str.contains(r#""text":"abcdef""#), "merged content 'abcdef' present: {all_str}");
}
```

Run:

```bash
cargo test merge_tests::w
```

Expected: `w1`/`w2`/`w3` FAIL (current impl is identity for deltas / part.added / done in merge mode); `w4`/`w5` FAIL (synthesis placeholder is empty).

- [ ] **Step 2: Replace the placeholder with the real flush implementation**

In `src/heal/merge.rs`, replace `flush_run_synthesis_placeholder` and remove the `run.item_type != ItemType::Message || !run.merged_text.is_empty()` gate so the flush always runs when a tracked run is being discarded:

```rust
    /// Synthesize the run's flush bytes: `content_part.done` and
    /// `output_item.done` based on accumulated merged_* fields. Called
    /// on type switches and at `response.completed` (Task 6).
    fn flush_run_synthesis(&self, run: RunState) -> Vec<Bytes> {
        let mut out = Vec::new();
        match run.item_type {
            ItemType::Message => {
                out.push(sse_block("response.content_part.done", &json!({
                    "type": "response.content_part.done",
                    "item_id": run.start_id,
                    "output_index": run.start_idx,
                    "part": { "type": "output_text", "text": run.merged_text }
                })));
                out.push(sse_block("response.output_item.done", &json!({
                    "type": "response.output_item.done",
                    "output_index": run.start_idx,
                    "item": {
                        "type": "message",
                        "id": run.start_id,
                        "role": "assistant",
                        "status": "completed",
                        "content": [{ "type": "output_text", "text": run.merged_text }]
                    }
                })));
            }
            ItemType::Reasoning => {
                out.push(sse_block("response.output_item.done", &json!({
                    "type": "response.output_item.done",
                    "output_index": run.start_idx,
                    "item": {
                        "type": "reasoning",
                        "id": run.start_id,
                        "summary": [{ "type": "summary_text", "text": run.merged_reasoning }]
                    }
                })));
            }
            ItemType::FunctionCall => {
                out.push(sse_block("response.output_item.done", &json!({
                    "type": "response.output_item.done",
                    "output_index": run.start_idx,
                    "item": {
                        "type": "function_call",
                        "id": run.start_id,
                        "call_id": run.call_id.unwrap_or_default(),
                        "name": "<merged>",  // function name is upstream's responsibility; placeholder if absent
                        "arguments": run.merged_arguments,
                        "status": "completed"
                    }
                })));
            }
        }
        out
    }
```

Add the helpers at module scope:

```rust
fn sse_block(event: &str, payload: &serde_json::Value) -> Bytes {
    Bytes::from(format!("event: {event}\ndata: {payload}\n\n"))
}

// Use serde_json::json in the function body via `use` at top.
```

Add `use serde_json::{json, Value};` to the imports at the top of `src/heal/merge.rs` (alongside the existing `use serde_json::Value;`).

Now simplify the type-switch branch in `on_added` (remove the gate, just flush):

```rust
                } else {
                    let mut flushed = Vec::new();
                    if let Some(run) = self.run.take() {
                        flushed.extend(self.flush_run_synthesis(run));
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
```

(Note: the old `if run.item_type != ItemType::Message || !run.merged_text.is_empty()` gate is gone — flushing always happens, even for a length-1 run, because the upstream's own `output_item.done` for that single fragment was suppressed by the merger when merge-mode engaged.)

Wait — re-check: a length-1 run is `run started` state, **not** merge mode. In Task 3 the implementation does NOT suppress `output_item.done` for a length-1 run because the run was never engaged (the suppression rules in W3 only apply when the second fragment arrived). So flushing a length-1 run is correct only if the upstream never sent its own done. If it did, we'd double-emit.

**Actually**, the gate `if !run.merged_text.is_empty()` was meant to handle exactly this. Drop it ONLY if we also implement done suppression for length-1 runs. **Decision:** keep the gate as `if !run.merged_text.is_empty() && run.item_type == ItemType::Message` for the early flush — Task 6 will revisit once we know the full semantics. For now, the simpler gate is sufficient because the upstream's own done always arrives after the delta stream ends, and the merger never sees it before the type switch (the type switch comes when a different item type appears, by which point the upstream's done for the prior item has already arrived and been processed by the merger — but Task 3's identity impl doesn't suppress it).

For correctness, **Task 6** will add proper done suppression for ANY tracked run (including length-1) and update this flush gate. **For now, restore the gate:**

```rust
                } else {
                    let mut flushed = Vec::new();
                    if let Some(run) = self.run.take() {
                        let should_flush = match run.item_type {
                            ItemType::Message => !run.merged_text.is_empty(),
                            ItemType::Reasoning => !run.merged_reasoning.is_empty(),
                            ItemType::FunctionCall => !run.merged_arguments.is_empty(),
                        };
                        if should_flush {
                            flushed.extend(self.flush_run_synthesis(run));
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
```

(If the upstream's own done arrived and was identity-passed, our synthesized done doubles the close. The W4/W5 fixtures don't hit this because they only assert synthesized-done presence. Task 6 tightens.)

- [ ] **Step 3: Add delta + done + content_part.added handling in `push_event`**

Replace the wildcard arm in `push_event`:

```rust
    pub fn push_event(&mut self, raw: &[u8], event: Option<&str>, data: &str) -> Vec<Bytes> {
        if !self.enabled {
            return Vec::new();
        }
        match event {
            Some("response.output_item.added") => self.on_added(raw, data),
            Some("response.output_text.delta")
            | Some("response.reasoning_summary_text.delta")
            | Some("response.function_call_arguments.delta") => self.on_delta(raw, data),
            Some("response.content_part.added") => self.on_content_part_added(raw, data),
            Some("response.output_text.done")
            | Some("response.content_part.done")
            | Some("response.output_item.done") => self.on_done(raw),
            Some("response.completed") => self.on_completed(raw),
            _ => vec![Bytes::copy_from_slice(raw)],
        }
    }
```

Add the new methods (insert before `finish`):

```rust
    fn on_delta(&mut self, raw: &[u8], data: &str) -> Vec<Bytes> {
        let Some(run) = self.run.as_ref() else {
            return vec![Bytes::copy_from_slice(raw)];
        };
        let Ok(mut v) = serde_json::from_str::<Value>(data) else {
            return vec![Bytes::copy_from_slice(raw)];
        };
        // Rewrite item_id + output_index to the run's first fragment.
        v["item_id"] = Value::String(run.start_id.clone());
        v["output_index"] = Value::Number(run.start_idx.into());
        // Accumulate the delta text into the run's merged_* field.
        if let Some(delta) = v.get("delta").and_then(Value::as_str) {
            match run.item_type {
                ItemType::Message => self.run.as_mut().unwrap().merged_text.push_str(delta),
                ItemType::Reasoning => self.run.as_mut().unwrap().merged_reasoning.push_str(delta),
                ItemType::FunctionCall => {
                    self.run.as_mut().unwrap().merged_arguments.push_str(delta)
                }
            }
        }
        let event_name = if v.get("delta").is_some() {
            // crude but sufficient: derive event from a marker field
            if data.contains("function_call_arguments") {
                "response.function_call_arguments.delta"
            } else if data.contains("reasoning_summary_text") {
                "response.reasoning_summary_text.delta"
            } else {
                "response.output_text.delta"
            }
        } else {
            "response.output_text.delta"
        };
        vec![sse_block(event_name, &v)]
    }

    fn on_content_part_added(&mut self, raw: &[u8], _data: &str) -> Vec<Bytes> {
        let Some(run) = self.run.as_mut() else {
            return vec![Bytes::copy_from_slice(raw)];
        };
        if run.part_added_emitted {
            return Vec::new();
        }
        run.part_added_emitted = true;
        vec![Bytes::copy_from_slice(raw)]
    }

    fn on_done(&mut self, _raw: &[u8]) -> Vec<Bytes> {
        // Suppress; the synthesized dones are flushed at run boundaries
        // (Task 4 type switch, Task 6 response.completed).
        Vec::new()
    }

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
```

- [ ] **Step 4: Run all fixtures**

```bash
cargo test merge_tests
```

Expected: PASS — 15 tests total.

- [ ] **Step 5: Commit**

```bash
git add src/heal/merge.rs src/heal/merge_tests.rs
git commit -m "feat(heal): merger rewrites deltas + synthesizes dones (W1–W5)

Adds response.output_text.delta / reasoning_summary_text.delta /
function_call_arguments.delta rewriting to the run's first fragment
(item_id, output_index), with merged text/reasoning/arguments
accumulation. content_part.added is forwarded once for the first
fragment and suppressed thereafter. output_text.done /
content_part.done / output_item.done are suppressed; synthesized
versions land at type switches and at response.completed."
```

---

### Task 6: Boundaries and failure modes (E1–E4)

**Files:**
- Modify: `src/heal/merge.rs` — tighten done suppression, handle large runs
- Modify: `src/heal/merge_tests.rs` — add 4 fixtures

**Interfaces:**
- `on_done` now also suppresses for length-1 runs (so the type-switch flush doesn't double-emit when the upstream's own done already passed through).
- `on_completed` handles the no-content case cleanly (no synthesis when merged_* is empty).

- [ ] **Step 1: Add the failing fixtures**

Append to `src/heal/merge_tests.rs`:

```rust
/// Spec E1: a 14-fragment run (the MiniMax M3 worst case from
/// NOTES-2026-08-28 §2) merges cleanly.
#[test]
fn e1_fourteen_fragment_run_merges() {
    let mut m = FragmentedItemMerger::new(true);
    let msg = |idx: u64| sse("response.output_item.added",
        &format!(r#"{{"type":"response.output_item.added","output_index":{idx},"item":{{"type":"message","id":"msg_{idx}","role":"assistant","status":"in_progress","content":[]}}}}"#));
    let mut emitted = 0;
    for i in 0..14 {
        let out = push_raw(&mut m, &msg(i));
        if !out.is_empty() {
            emitted += 1;
        }
    }
    assert_eq!(emitted, 1, "only the first fragment passes through, 13 suppressed");
}

/// Spec E2: a message run that switches to function_call flushes the
/// synthesized dones BEFORE the function_call item is emitted.
#[test]
fn e2_run_flushes_done_before_type_switch() {
    let mut m = FragmentedItemMerger::new(true);
    let msg = |id: &str| sse("response.output_item.added",
        &format!(r#"{{"type":"response.output_item.added","output_index":0,"item":{{"type":"message","id":"{id}","role":"assistant","status":"in_progress","content":[]}}}}"#));
    let fc = sse("response.output_item.added",
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_0","call_id":"c0","name":"shell","arguments":"","status":"in_progress"}}"#);
    let _ = push_raw(&mut m, &msg("msg_0"));
    let _ = push_raw(&mut m, &msg("msg_1"));
    let fc_out = push_raw(&mut m, &fc);
    let all: Vec<u8> = fc_out.into_iter().flat_map(|b| b.to_vec()).collect();
    let s = std::str::from_utf8(&all).unwrap();
    // Synthesized done should appear before the function_call's added.
    let done_pos = s.find("response.output_item.done").unwrap_or(usize::MAX);
    let fc_pos = s.find(r#""type":"function_call""#).unwrap_or(usize::MAX);
    assert!(done_pos < fc_pos, "synthesized done must precede function_call: {s}");
}

/// Spec E3: a truncated run (only first fragment + a few deltas, then
/// stream ends) does NOT synthesize done in finish() — γ-1.
#[test]
fn e3_truncated_run_no_synthesized_done_in_finish() {
    let mut m = FragmentedItemMerger::new(true);
    let msg = |id: &str| sse("response.output_item.added",
        &format!(r#"{{"type":"response.output_item.added","output_index":0,"item":{{"type":"message","id":"{id}","role":"assistant","status":"in_progress","content":[]}}}}"#));
    let delta = |text: &str| sse("response.output_text.delta",
        &format!(r#"{{"type":"response.output_text.delta","item_id":"msg_0","output_index":0,"delta":"{text}"}}"#));
    let _ = push_raw(&mut m, &msg("msg_0"));
    let _ = push_raw(&mut m, &msg("msg_1"));
    let _ = push_raw(&mut m, &delta("half"));
    // Stream ends without response.completed.
    let finish_out = m.finish();
    assert!(finish_out.is_empty(), "finish() must not synthesize done: γ-1");
}

/// Spec E4: a fragment with an empty `item.id` passes through (the
/// merger tolerates; downstream healer handles).
#[test]
fn e4_empty_item_id_tolerated() {
    let mut m = FragmentedItemMerger::new(true);
    let raw = sse("response.output_item.added",
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"","role":"assistant","status":"in_progress","content":[]}}"#);
    let out = push_raw(&mut m, &raw);
    assert_eq!(concat(out), raw);
}
```

Run:

```bash
cargo test merge_tests::e
```

Expected: `e1` PASSES (already covered). `e2` PASSES (already correct ordering). `e3` PASSES (finish() is empty by design). `e4` PASSES (Task 3 impl tolerates empty id).

- [ ] **Step 2: Re-run the full fixture suite as a regression baseline (before the tighten)**

Run:

```bash
cargo test merge_tests
```

Expected: PASS — 19 tests still green (the tighten below changes on_done semantics; this is the baseline). If any test fails here, fix it before proceeding.

- [ ] **Step 3: Tighten `on_done` to also suppress for length-1 runs**

Replace `on_done`:

```rust
    fn on_done(&mut self, raw: &[u8]) -> Vec<Bytes> {
        // If we're tracking a run AND that run's accumulated content is
        // non-empty, suppress this done — the merger will synthesize its
        // own at the run boundary (Task 4 type switch or Task 5's
        // response.completed arm). For length-1 runs with no merged
        // content, pass through: the upstream's own done is the close.
        let Some(run) = self.run.as_ref() else {
            return vec![Bytes::copy_from_slice(raw)];
        };
        let has_content = match run.item_type {
            ItemType::Message => !run.merged_text.is_empty(),
            ItemType::Reasoning => !run.merged_reasoning.is_empty(),
            ItemType::FunctionCall => !run.merged_arguments.is_empty(),
        };
        if has_content {
            Vec::new()
        } else {
            vec![Bytes::copy_from_slice(raw)]
        }
    }
```

- [ ] **Step 4: Run all fixtures**

```bash
cargo test merge_tests
```

Expected: PASS — 19 tests.

- [ ] **Step 5: Commit**

```bash
git add src/heal/merge.rs src/heal/merge_tests.rs
git commit -m "feat(heal): tighten done suppression for length-1 runs (E1–E4)

E1/E2/E3/E4 pass without further code changes (already correct).
Tightens on_done so a length-1 run's upstream done passes through
unchanged, while a merged run's upstream done is suppressed in favor
of the synthesized one at the run boundary."
```

---

### Task 7: Interaction with `ResponsesStreamHealer` (S1–S3)

**Files:**
- Modify: `src/heal/responses_healer_tests.rs` — add 3 fixtures that compose the merger upstream of the healer
- (No production code change expected — the merger and healer are orthogonal by design.)

**Interfaces:**
- The merger is fed raw SSE; its output is fed to `ResponsesStreamHealer::push_event` (same byte stream). This fixture proves the chain works end-to-end.

- [ ] **Step 1: Add the fixtures**

Append to `src/heal/responses_healer_tests.rs` (alongside the existing fixtures):

```rust
/// Spec S1: a merged run with leaked DSML markup — merger rewrites
/// item_id, healer strips the markup.
#[test]
fn s1_merged_run_with_dsml_heals() {
    use crate::heal::{FragmentedItemMerger, HealGates, ResponsesStreamHealer};
    let mut merger = FragmentedItemMerger::new(true);
    let mut healer = ResponsesStreamHealer::new(HealGates {
        dsml: true,
        think: true,
        merge_fragmented: true,
    });
    // Two message fragments (msg_0, msg_1); msg_1's delta carries leaked DSML.
    let feed = |m: &mut FragmentedItemMerger, h: &mut ResponsesStreamHealer, raw: &[u8], event: &str, data: &str| -> Vec<Bytes> {
        let mut all = Vec::new();
        for chunk in m.push_event(raw, Some(event), data) {
            all.push(chunk.clone());
            for h_chunk in h.push_event(&chunk, Some(event), data) {
                all.push(h_chunk);
            }
        }
        all
    };
    let added0 = br#"event: response.output_item.added
data: {"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_0","role":"assistant","status":"in_progress","content":[]}}

"#;
    let added1 = br#"event: response.output_item.added
data: {"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"msg_1","role":"assistant","status":"in_progress","content":[]}}

"#;
    let _ = feed(&mut merger, &mut healer, added0, "response.output_item.added", "");
    let _ = feed(&mut merger, &mut healer, added1, "response.output_item.added", "");
    // Delta carrying DSML markup (msg_1's id; merger rewrites to msg_0).
    let dsml_delta = br#"event: response.output_text.delta
data: {"type":"response.output_text.delta","item_id":"msg_1","output_index":1,"delta":"<｜DSML｜invoke name=\"shell\"><｜DSML｜parameter name=\"cmd\" string=\"true\">echo hi</｜DSML｜parameter></｜DSML｜invoke>"}

"#;
    let data_str = std::str::from_utf8(dsml_delta).unwrap().lines()
        .find_map(|l| l.strip_prefix("data: ").map(str::to_string)).unwrap_or_default();
    let out = feed(&mut merger, &mut healer, dsml_delta, "response.output_text.delta", &data_str);
    let s: String = out.iter().flat_map(|b| b.to_vec()).map(|b| b as u8 as char).collect();
    // The DSML markup must NOT appear in the wire by the time the chain is done.
    assert!(!s.contains("<｜DSML｜"), "DSML markup leaked through chain: {s}");
}

/// Spec S2: a merged run with leaked think markup → healer splits to reasoning.
#[test]
fn s2_merged_run_with_think_splits_to_reasoning() {
    use crate::heal::{FragmentedItemMerger, HealGates, ResponsesStreamHealer};
    let mut merger = FragmentedItemMerger::new(true);
    let mut healer = ResponsesStreamHealer::new(HealGates {
        dsml: true,
        think: true,
        merge_fragmented: true,
    });
    let feed = |m: &mut FragmentedItemMerger, h: &mut ResponsesStreamHealer, raw: &[u8], event: &str, data: &str| -> Vec<Bytes> {
        let mut all = Vec::new();
        for chunk in m.push_event(raw, Some(event), data) {
            all.push(chunk.clone());
            for h_chunk in h.push_event(&chunk, Some(event), data) {
                all.push(h_chunk);
            }
        }
        all
    };
    let added = |id: &str| format!(
        "event: response.output_item.added\ndata: {{\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{{\"type\":\"message\",\"id\":\"{id}\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}}}\n\n"
    ).into_bytes();
    let _ = feed(&mut merger, &mut healer, &added("msg_0"), "response.output_item.added", "");
    let _ = feed(&mut merger, &mut healer, &added("msg_1"), "response.output_item.added", "");
    let think_delta = br#"event: response.output_text.delta
data: {"type":"response.output_text.delta","item_id":"msg_1","output_index":1,"delta":"<think>musings</think>Hi"}

"#;
    let data_str = std::str::from_utf8(think_delta).unwrap().lines()
        .find_map(|l| l.strip_prefix("data: ").map(str::to_string)).unwrap_or_default();
    let out = feed(&mut merger, &mut healer, think_delta, "response.output_text.delta", &data_str);
    let s: String = out.iter().flat_map(|b| b.to_vec()).map(|b| b as u8 as char).collect();
    // The healer must have emitted a reasoning event with the think text.
    assert!(s.contains("response.reasoning_summary_text.delta") || s.contains("reasoning"),
        "reasoning channel not engaged: {s}");
    assert!(s.contains("musings"), "think text should appear on reasoning channel: {s}");
}

/// Spec S3: combined DSML + think leakage in the same merged run.
#[test]
fn s3_merged_run_with_both_quirks_heals() {
    use crate::heal::{FragmentedItemMerger, HealGates, ResponsesStreamHealer};
    let mut merger = FragmentedItemMerger::new(true);
    let mut healer = ResponsesStreamHealer::new(HealGates {
        dsml: true,
        think: true,
        merge_fragmented: true,
    });
    let feed = |m: &mut FragmentedItemMerger, h: &mut ResponsesStreamHealer, raw: &[u8], event: &str, data: &str| -> Vec<Bytes> {
        let mut all = Vec::new();
        for chunk in m.push_event(raw, Some(event), data) {
            all.push(chunk.clone());
            for h_chunk in h.push_event(&chunk, Some(event), data) {
                all.push(h_chunk);
            }
        }
        all
    };
    let added = |id: &str| format!(
        "event: response.output_item.added\ndata: {{\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{{\"type\":\"message\",\"id\":\"{id}\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}}}\n\n"
    ).into_bytes();
    let _ = feed(&mut merger, &mut healer, &added("msg_0"), "response.output_item.added", "");
    let _ = feed(&mut merger, &mut healer, &added("msg_1"), "response.output_item.added", "");
    // Delta with think first, then DSML — exercises both filter stages.
    let mixed = br#"event: response.output_text.delta
data: {"type":"response.output_text.delta","item_id":"msg_1","output_index":1,"delta":"<think>secret</think><｜DSML｜invoke name=\"x\"><｜DSML｜parameter name=\"y\" string=\"true\">z</｜DSML｜parameter></｜DSML｜invoke>visible"}

"#;
    let data_str = std::str::from_utf8(mixed).unwrap().lines()
        .find_map(|l| l.strip_prefix("data: ").map(str::to_string)).unwrap_or_default();
    let out = feed(&mut merger, &mut healer, mixed, "response.output_text.delta", &data_str);
    let s: String = out.iter().flat_map(|b| b.to_vec()).map(|b| b as u8 as char).collect();
    assert!(!s.contains("<｜DSML｜"), "DSML markup leaked: {s}");
    assert!(!s.contains("<think>"), "think markup leaked: {s}");
}
```

Run:

```bash
cargo test responses_healer_tests::s
```

Expected: PASS — 3 new tests; no changes to responses.rs needed.

- [ ] **Step 2: Run tests to verify they pass**

Run:

```bash
cargo test responses_healer_tests::s
```

Expected: PASS — 3 new tests. (No production code change is needed: the merger + healer composition works by spec; this is a verification fixture, not a TDD failing-test step.)

- [ ] **Step 3: Commit**

```bash
git add src/heal/responses_healer_tests.rs
git commit -m "test(heal): cover merger + ResponsesStreamHealer composition (S1–S3)

Proves the merger and healer are orthogonal: the merger's output is
fed directly to ResponsesStreamHealer::push_event, and both quirks
(DSML, think) still heal correctly within a merged run. No production
code change in this commit — the composition works by spec."
```

---

### Task 8: Wire `FragmentedItemMerger` into `passthrough.rs`

**Files:**
- Modify: `src/proxy/passthrough.rs:42-49` (replace `let heal = ... HealGates { dsml, think }` to add `merge_fragmented`)
- Modify: `src/proxy/passthrough.rs` (the streaming loop where `ResponsesStreamHealer` is constructed, add `FragmentedItemMerger` upstream)

**Interfaces:**
- `passthrough.rs` reads `config.quirk_enabled("merge_fragmented")` once per request (same pattern as `dsml` / `think`).
- The streaming loop becomes: chunk → `merger.push_event(...)` → `healer.push_event(...)` → tx.send + raw.extend.

- [ ] **Step 1: Verify the build is currently broken**

```bash
cargo build 2>&1 | head -10
```

Expected: errors about `missing field merge_fragmented` in `passthrough.rs:45` and `chat.rs:46`. (These were introduced in Task 1.)

- [ ] **Step 2: Update `passthrough.rs`'s `HealGates` construction**

In `src/proxy/passthrough.rs`, replace the `let heal = ...` block (around line 42):

```rust
    let heal = {
        let config = state.config.read().await;
        crate::heal::HealGates {
            dsml: config.quirk_enabled("dsml_heal"),
            think: config.quirk_enabled("think_tags"),
            merge_fragmented: config.quirk_enabled("merge_fragmented"),
        }
    };
```

- [ ] **Step 3: Insert the merger in the healed-relay loop**

Find the streaming section in `passthrough.rs` that constructs and uses `ResponsesStreamHealer` (around the comment "Healed relay: event-granular rewrite"). Replace the `let mut healer = crate::heal::ResponsesStreamHealer::new(heal);` line with:

```rust
                // Fragmented-items merger runs BEFORE the content healer:
                // it normalizes the stream shape (collapsing same-type item
                // runs into single Responses-conformant items), so the healer
                // sees one canonical item per logical content unit. The
                // two passes are orthogonal: the merger only touches
                // item_id / output_index and done suppression; the healer
                // does content-level DSML/think repair. See
                // docs/superpowers/specs/2026-08-30-fragmented-items-merger-design.md
                // §Design > Interaction with ResponsesStreamHealer.
                let mut merger = crate::heal::FragmentedItemMerger::new(heal.merge_fragmented);
                let mut healer = crate::heal::ResponsesStreamHealer::new(heal);
```

Then in the same `'relay: loop`, change the inner event processing so each upstream event passes through `merger.push_event` first, then `healer.push_event` on each chunk the merger emits:

```rust
                    if !ttft_recorded {
                        if let Some(ref event_type) = evt.event {
                            if is_first_content_event(event_type) {
                                ttft_recorded = true;
                                metrics.observe_ttft(
                                    &provider_name,
                                    &route_key,
                                    &upstream_log,
                                    upstream_start.elapsed().as_secs_f64(),
                                );
                            }
                        }
                    }
                    for chunk in merger.push_event(&evt.raw, evt.event.as_deref(), &evt.data) {
                        if tx.send(Ok(chunk.clone())).await.is_err() {
                            client_disconnected = true;
                            break 'relay;
                        }
                        let prev_len = raw.len();
                        raw.extend_from_slice(&chunk);
                        if !raw_trimmed && trim_completed_prefix(&mut raw, prev_len) {
                            raw_trimmed = true;
                        }
                        for h_chunk in healer.push_event(&chunk, evt.event.as_deref(), &evt.data) {
                            if tx.send(Ok(h_chunk.clone())).await.is_err() {
                                client_disconnected = true;
                                break 'relay;
                            }
                            let prev_len = raw.len();
                            raw.extend_from_slice(&h_chunk);
                            if !raw_trimmed && trim_completed_prefix(&mut raw, prev_len) {
                                raw_trimmed = true;
                            }
                        }
                    }
```

After the loop, also flush the merger (no-op by spec γ-1, but kept for symmetry):

```rust
                for chunk in merger.finish() {
                    if tx.send(Ok(chunk.clone())).await.is_err() {
                        client_disconnected = true;
                        break;
                    }
                    let prev_len = raw.len();
                    raw.extend_from_slice(&chunk);
                    if !raw_trimmed && trim_completed_prefix(&mut raw, prev_len) {
                        raw_trimmed = true;
                    }
                }
                for chunk in healer.finish() {
                    if tx.send(Ok(chunk.clone())).await.is_err() {
                        client_disconnected = true;
                        break;
                    }
                    let prev_len = raw.len();
                    raw.extend_from_slice(&chunk);
                    if !raw_trimmed && trim_completed_prefix(&mut raw, prev_len) {
                        raw_trimmed = true;
                    }
                }
```

- [ ] **Step 4: Update `chat.rs`'s `HealGates` construction**

In `src/proxy/chat.rs:42-53`, add `merge_fragmented` to the literal (the chat path always has the gate off; this restores the build):

```rust
    let (glm_thinking, heal, missing_done_quirk) = {
        let config = state.config.read().await;
        (
            config.quirk_enabled("glm_thinking"),
            crate::heal::HealGates {
                dsml: config.quirk_enabled("dsml_heal"),
                think: config.quirk_enabled("think_tags"),
                merge_fragmented: config.quirk_enabled("merge_fragmented"),
            },
            config.quirk_enabled("missing_done"),
        )
    };
```

(The chat path never invokes `FragmentedItemMerger` — it owns `StreamConverter` which is naturally unfragmented. The field is read here only to keep `HealGates::new` total.)

- [ ] **Step 5: Build and run all unit tests**

```bash
cargo build 2>&1 | tail -10
cargo test
```

Expected: `cargo build` PASS (no compile errors); `cargo test` PASS (no regressions in `HealGates`-using code, all 19 merge fixtures pass).

- [ ] **Step 6: Commit**

```bash
git add src/proxy/passthrough.rs src/proxy/chat.rs
git commit -m "feat(proxy): wire FragmentedItemMerger into passthrough healed branch

Inserts the merger upstream of ResponsesStreamHealer in the streaming
loop (passthrough.rs). Each upstream event now flows:
  merger.push_event → tx.send + raw.extend (per chunk)
  → healer.push_event → tx.send + raw.extend (per chunk)
merger.finish() is called after the relay loop (no-op by spec γ-1).
chat.rs HealGates construction is updated to include the new field
for compile-correctness; the chat path never invokes the merger."
```

---

### Task 9: Integration tests in `tests/passthrough.rs`

**Files:**
- Modify: `tests/common/mod.rs` — add a mock handler that emits fragmented SSE
- Modify: `tests/passthrough.rs` — add 2 cases

**Interfaces:**
- A new mock scenario (call it `responses-fragmented`) that the mock axum upstream serves, configured via the standard test config format (`[providers.X] base_url = ".../fragmented"`).

- [ ] **Step 1: Add the fragmented mock to `tests/common/mod.rs`**

Find the section that defines mock scenarios (search for `pub const STREAM_CHUNKS` and the handler builders around it; typically a `build_router` function). Add a new scenario builder. Pattern:

```rust
/// Build a mock axum router that emits a fragmented Responses SSE stream
/// (each chunk arrives as its own output_item.added event). Used by
/// `tests/passthrough.rs` cases for the `merge_fragmented` integration
/// tests. The route is `/fragmented`.
pub fn fragmented_mock_router() -> Router {
    use axum::response::sse::Event;
    use std::convert::Infallible;
    let stream = async_stream::stream! {
        // 5 message fragments, each with one delta.
        for i in 0..5 {
            let added = format!(
                "event: response.output_item.added\ndata: {{\"type\":\"response.output_item.added\",\"output_index\":{i},\"item\":{{\"type\":\"message\",\"id\":\"msg_{i}\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}}}\n\n",
                i = i
            );
            yield Ok::<_, Infallible>(Event::default().data(added));
            let delta = format!(
                "event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"item_id\":\"msg_{i}\",\"output_index\":{i},\"delta\":\"chunk{i} \"}}\n\n",
                i = i
            );
            yield Ok::<_, Infallible>(Event::default().data(delta));
        }
        yield Ok::<_, Infallible>(Event::default().data(
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r_0\",\"status\":\"completed\",\"output\":[]}}\n\n".to_string()
        ));
    };
    Router::new().route("/fragmented", get(move || {
        Sse::new(stream).into_response()
    }))
}
```

(If the existing mock setup uses a different pattern, mirror it; the requirement is a `/fragmented` endpoint that emits 5 message fragments + a `response.completed`.)

- [ ] **Step 2: Add the integration case to `tests/passthrough.rs`**

Append:

```rust
#[tokio::test]
/// Spec: when the upstream emits a fragmented message run (5 message
/// items, each carrying one delta), the client must receive exactly ONE
/// `output_item.added` event for the message item — the merger's job.
async fn passthrough_merges_fragmented_message_run() {
    let env = setup_with_config(|mock_base_url, port| {
        format!(
            r#"
[server]
host = "127.0.0.1"
port = {port}

[providers.merger]
base_url = "{mock_base_url}"
api_key = "test-key"
format = "responses"
timeout_ms = 5000

[routes]
"merger/M3" = {{ model = "upstream-M3" }}
"#
        )
    })
    .await;

    let resp = env
        .client
        .post(format!("{}/v1/responses", env.router_url))
        .json(&json!({
            "model": "merger/M3",
            "input": "hello",
            "stream": true,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.unwrap();

    // Count output_item.added events: must be 1 (merged), not 5 (fragmented).
    let added_count = body.matches("response.output_item.added").count();
    assert_eq!(
        added_count, 1,
        "expected exactly 1 output_item.added (merged), got {added_count}:\n{body}"
    );

    // Merged text should contain all 5 chunk fragments concatenated.
    let mut idx = 0;
    let mut expected = String::new();
    for i in 0..5 {
        expected.push_str(&format!("chunk{i} "));
    }
    // Find the delta events and verify the text is the full concatenation.
    let text_present = body.contains(&expected.trim_end());
    assert!(text_present, "merged text not found in body:\n{body}");
    let _ = idx;
}

#[tokio::test]
/// Spec: an interleaved stream (msg run × 3 + reasoning × 1 + msg run × 2)
/// must yield 3 client-visible items: merged_msg + reasoning + merged_msg.
async fn passthrough_merges_interleaved_runs() {
    let env = setup_with_config(|mock_base_url, port| {
        format!(
            r#"
[server]
host = "127.0.0.1"
port = {port}

[providers.merger]
base_url = "{mock_base_url}"
api_key = "test-key"
format = "responses"
timeout_ms = 5000

[routes]
"merger/M3" = {{ model = "upstream-M3" }}
"#
        )
    })
    .await;

    let resp = env
        .client
        .post(format!("{}/v1/responses", env.router_url))
        .json(&json!({
            "model": "merger/M3",
            "input": "hello",
            "stream": true,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.unwrap();

    // At least 3 items: 2 merged messages + 1 reasoning. The reasoning
    // item is its own item; the messages are merged.
    let added_count = body.matches("response.output_item.added").count();
    assert!(added_count >= 3, "expected ≥ 3 items, got {added_count}:\n{body}");
}
```

(The second test assumes the mock also supports a mixed scenario. If `tests/common/mod.rs` doesn't currently emit mixed scenarios, this test should be paired with extending the mock, or dropped to a single case. **If extending the mock is too costly, drop this test** and ship only the first — spec calls for "2-3 cases" so 1 is below target but acceptable; flag in commit message.)

- [ ] **Step 3: Run the integration test(s)**

```bash
cargo test --test passthrough passthrough_merges
```

Expected: PASS — 1 (or 2) tests.

- [ ] **Step 4: Commit**

```bash
git add tests/common/mod.rs tests/passthrough.rs
git commit -m "test(passthrough): integration test for fragmented-items merger

Adds a mock upstream scenario that emits 5 message fragments + a
response.completed, and a client-side test that asserts exactly ONE
output_item.added event reaches the client. Without the merger wired
in, this fails with 5 added events and 5 separate message items."
```

---

### Task 10: Update `docs/heal-description.md` per spec §Docs sync

**Files:**
- Modify: `docs/heal-description.md` (§3, §6, §7, §8, §12 — see spec §Docs sync for the precise list)

**Interfaces:**
- The doc becomes the post-implementation reference for the merger.

- [ ] **Step 1: §3 — add `merge_fragmented` row to the总览表**

Find the table in `docs/heal-description.md` (around §3) and add a row before the markdown separator (matching the table's column order: Quirk / 方向 / 子类 / 触发对象 / 默认 / 进 HealGates?):

```
| `merge_fragmented` | response | 流形状修复 | 上游 LLM 响应里连续同类型 `output_item.added` | ON | **是**（`HealGates.merge_fragmented`） |
```

- [ ] **Step 2: §6 — add §6.5 section for `merge_fragmented`**

Append after the §6.4 `missing_done` section:

```markdown
### 6.5 `merge_fragmented` —— response 侧流形状修复

| 字段 | 值 |
|---|---|
| 方向 | response |
| 子类 | 流形状修复（与内容修复正交）|
| 适用路径 | **responses only**（chat 是构造者姿态，天然不碎片）|
| 触发条件 | 连续同类型 `output_item.added`：`message` / `reasoning` 同 type 直接合并；`function_call` 额外要求 `call_id` 匹配（OpenAI 契约：同 call 必须同 item）|
| 修复做法 | 把 N 个连续同类型 item 折回 1 个：第 1 个原样透传、后续 N-1 个 added 抑制、deltas 重写 `item_id`/`output_index` 为首碎片、累积 merged_* 文本、run 末尾 flush 合成 `content_part.done` + `output_item.done` |
| 状态位 | `HealGates::merge_fragmented: bool`，默认 `true` |
| Hook 点 | `src/proxy/passthrough.rs` 的 healed 分支，**插在 `ResponsesStreamHealer` 前面** |
| 与 healer 的关系 | D-1：merger 只管形状，healer 只管内容。merger 改写后 healer 看到「真的只有一个 item」——**消除了 review #5/#7 的 multi-item 边界** |

模块：`src/heal/merge.rs` + `src/heal/merge_tests.rs`（19 个 fixture，按 M1–M9 / W1–W5 / E1–E4 / S1–S3 / K1 编号）。

设计 spec：`docs/superpowers/specs/2026-08-30-fragmented-items-merger-design.md`。
NOTES 调查：`NOTES-2026-08-28-minimax-m3-fragmentation.md`（git-exclude，仅本地）。
```

- [ ] **Step 3: §7 — add `merge.rs` + `merge_tests.rs` rows to the module directory table**

In the §7 directory listing, add two new rows in the `src/heal/` table:

```
├── merge.rs                         X   FragmentedItemMerger + ItemType + RunState
└── merge_tests.rs                   X   M1–M9 / W1–W5 / E1–E4 / S1–S3 / K1 fixtures
```

Fill in `X` with the actual line counts after `wc -l src/heal/merge.rs src/heal/merge_tests.rs` reports them post-implementation.

- [ ] **Step 4: §8 — add `FragmentedItemMerger` column to the state machine对照表**

In §8, append a new column to the对照表 for `FragmentedItemMerger（responses，stream-shape heal）`. The column mirrors `ResponsesStreamHealer`'s structure but emphasizes the run tracker:

```markdown
| 字段 | StreamConverter（chat） | ResponsesStreamHealer（responses） | FragmentedItemMerger（responses, stream-shape） |
|---|---|---|---|
| 持有 filter | `dsml_filter`, `think_filter` | `dsml`, `think` | — |
| 文本累积 | `acc.text`, `acc.reasoning` | `healed_text`, `reasoning_text` | `merged_text`, `merged_reasoning`, `merged_arguments` |
| 跟踪/状态 | `message_output_index`, `reasoning_output_index` | `message_item_id`, `message_output_index` | `run: Option<RunState { item_type, start_idx, start_id, call_id, merged_*, part_added_emitted }>` |
| 下一个分配 index | `next_output_index: usize` | `next_index: usize` (from `INJECT_INDEX_BASE`) | —（不分配新 index；改写现有 index） |
| tool call 累积 | `tool_calls: BTreeMap<...>` | `injected_calls: Vec<...>` | —（call_id 匹配，不累积） |
| finish 幂等 | `finish_emitted: bool` | `finished: bool` | γ-1: `finish()` 不合成 done |
| 「天然不碎片」机制 | turn-level `Option<usize>` 整 turn 至多 None→Some 一次 | 依赖上游合规流 | **修复**上游非合规流：run 跟踪 + 合成 done |
```

- [ ] **Step 5: §12 — update the summary from "stream-shape heal 仅 responses" to "stream-shape heal 在 responses 上由 merge_fragmented 提供"**

Find the §12 bullet that currently reads:

> - **stream-shape heal 仅 responses 有意义** —— chat 是「按规范构造」…

Replace with:

> - **stream-shape heal 在 responses 上由 `merge_fragmented` 提供**（新增类别）—— chat 仍是「按规范构造」，`message_output_index: Option<usize>` 整个 turn 至多 None→Some 一次是不变量；responses 由 `FragmentedItemMerger` 把上游非合规流（每个 chunk 一个 item）折回合规（一个逻辑 item 一个 add/done 周期）。

- [ ] **Step 6: §14 — flip the扩展点 placeholder to "shipped"**

Find §14's "Future扩展点" section. Replace the "未来在 HealGates 里加第三字段时" preamble with:

```markdown
## 14. 后续扩展点（已落地：`merge_fragmented`）

`merge_fragmented` quirk 已落地（2026-08-30，见 spec
`docs/superpowers/specs/2026-08-30-fragmented-items-merger-design.md`）。
模块 `src/heal/merge.rs`，接入 `src/proxy/passthrough.rs` 的 healed 分支。

未来若再加新响应侧修复：
- 设计 spec：`docs/superpowers/specs/<date>-<topic>-design.md`
- 本档案同步更新：§3 加行、§6 加小节、§7 加模块目录行、§8 加 state machine 对照列、§12 更新总结措辞
```

- [ ] **Step 7: Run full doc render + commit**

```bash
# No compile check for docs; just commit.
wc -l src/heal/merge.rs src/heal/merge_tests.rs  # for §7 row填充
git add docs/heal-description.md
git commit -m "docs(heal-description): document merge_fragmented quirk

Updates §3/§6/§7/§8/§12/§14 per spec §Docs sync to reflect the
shipped merge_fragmented heal pass. Module line counts filled in
from wc -l post-implementation."
```

---

### Task 11: Full regression + `CHANGELOG` draft

**Files:**
- Modify: `CHANGELOG.md` (add unreleased section)

**Interfaces:** none.

- [ ] **Step 1: Run the full test suite**

```bash
cargo test
```

Expected: PASS — all unit + integration tests green.

- [ ] **Step 2: Run clippy and format check**

```bash
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: no warnings (or fix any that surface; the existing project uses clippy clean per AGENTS.md §"Keep the top-level MD docs in sync").

- [ ] **Step 3: Draft the CHANGELOG entry**

Run (per AGENTS.md §13):

```bash
scripts/release.sh v0.1.4 --prep-changelog
```

Edit the generated `CHANGELOG.md` section under `## [v0.1.4] — Unreleased` to read (or augment the auto-generated draft with):

```markdown
## [v0.1.4] — Unreleased

**`merge_fragmented` heal pass for upstream Responses SSE fragmentation.**

### Features

- `[quirks] merge_fragmented` (default ON): when an upstream Responses
  gateway emits one logical output item as N consecutive
  `output_item.added` events (observed with MiniMax M3 in
  `NOTES-2026-08-28`, typical 5–14 fragments), the daemon merges them
  into a single Responses-conformant item. All deltas in the run are
  rewritten to the first fragment's `item_id` / `output_index`, and a
  synthesized `content_part.done` + `output_item.done` is emitted at
  the run boundary. Applies to consecutive same-type `message` /
  `reasoning` items, and to consecutive `function_call` items with
  matching `call_id` (OpenAI Responses contract: same call must live
  in the same item). Off via `[quirks] disabled = ["merge_fragmented"]`.
  Hot-reloadable. Responses-format path only — chat-format path is
  naturally unfragmented.

### Spec

`docs/superpowers/specs/2026-08-30-fragmented-items-merger-design.md`.
```

Commit:

```bash
git add CHANGELOG.md
git commit -m "docs(changelog): v0.1.4 unreleased entry for merge_fragmented"
```

- [ ] **Step 4: Final verification**

```bash
cargo test
cargo build --release
```

Expected: PASS / build success.

- [ ] **Step 5: Push (do NOT release yet)**

```bash
git push origin main
```

Per AGENTS.md §13: "CHANGELOG.md (per-release changes — the new version's section is drafted with `scripts/release.sh vX.Y.Z --prep-changelog`, curated, and committed to main BEFORE the release is cut, so both remotes receive it through the normal push flow)." — push the unreleased entry to main; cut the actual release tag in a separate step.
