//! Response healing for both paths (quirks `dsml_heal` + `think_tags`).
//!
//! - **Chat path** (blocking + streaming): [`heal_dsml_chat_message`] /
//!   [`heal_think_chat_message`] for blocking responses, and the
//!   `DsmlStreamFilter` / `ThinkStreamFilter` pipeline that `StreamConverter`
//!   drives for streamed responses.
//! - **Responses passthrough path** (Phase B): [`ResponsesStreamHealer`]
//!   rewrites leaked DSML / `<think>` markup out of an upstream's SSE stream
//!   event by event, injecting think reasoning and healed function calls as
//!   Responses items.
//!
//! Both are **class-B (response-healing)** quirks ported from codex-relay
//! (`src/dsml.rs`, `src/think.rs`): gated by detecting the anomaly itself,
//! no-ops on healthy responses, self-deactivating once the upstream fixes
//! the bug. Model-name gating is deliberately absent - the trigger
//! condition is precise and the same model is served under many names.
//! Kill switch: `[quirks] disabled = [...]` in config.toml (hot-reloaded);
//! the gates are pre-read by the proxy once per request
//! ([`HealGates`]) so this module never touches config.
//!
//! Order matters where both quirks meet: content passes DSML isolation
//! FIRST, then the think filter - a DSML parameter value may legitimately
//! contain `<think>` text as part of a tool argument.
//!
//! Module split (spec Phase 3): `think.rs`, `dsml.rs`, `responses.rs` hold the
//! three concerns; the public API is re-exported below so `crate::heal::*`
//! paths stay stable.

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

mod dsml;
mod merge;
mod responses;
mod think;

pub use dsml::{heal_dsml_chat_message, parse_leaked_tool_calls, DsmlStreamFilter, DsmlToolCall};
pub use responses::{heal_responses_body, ResponsesStreamHealer};
// FragmentedItemMerger is consumed by `passthrough.rs`'s streaming loop
// (wired in Task 8). Until then the re-export would warn as unused;
// suppress with a scoped attribute so Task 2's build stays clean.
#[allow(unused_imports)]
pub use merge::FragmentedItemMerger;
pub use think::{contains_think_markup, heal_think_chat_message, ThinkSplit, ThinkStreamFilter};

pub(crate) use dsml::synthesize_call_id;

#[cfg(test)]
mod dsml_tests;
#[cfg(test)]
mod merge_tests;
#[cfg(test)]
mod responses_healer_tests;
#[cfg(test)]
mod think_tests;
