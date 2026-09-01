//! Boundary normalization: translate Codex's private wire dialects into the
//! public API shape before requests reach third-party upstreams.
//!
//! Codex CLI occasionally delivers parts of a request in OpenAI-internal
//! dialect shapes the public Responses/Chat APIs never defined:
//!
//! - `additional_tools` INPUT items carrying namespace-wrapped function
//!   tools instead of a top-level `tools` array (Codex 0.147 under
//!   `use_responses_lite`; the 2026-08-17 DSML leak) — hoisted by
//!   [`collect_additional_tools_reporting`] / [`normalize_responses_request`].
//! - `namespace` entries inside the top-level `tools` array — flattened for
//!   Chat upstreams by [`normalize_chat_tools`] (Responses upstreams have
//!   proven tolerant, so passthrough leaves them verbatim).
//!
//! Standing principle: **known dialects are translated, unknown dialects are
//! made visible** — never silently dropped, never guessed at, never
//! rejected. Unknown input item types pass through (Responses) or are
//! skipped (Chat, where no mapping exists) and are logged with a
//! process-lifetime counter via [`warn_unknown_item_types`], so a future
//! Codex dialect shows up in the logs and in `doctor --live` (which asserts
//! [`KNOWN_INPUT_ITEM_TYPES`]) instead of degrading silently.

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};

/// Input item types a normalized request may carry. The contract shared
/// with `doctor --live`'s wire-shape probe — a Codex release introducing a
/// new item type trips the doctor until the type is reviewed here.
/// Consumed by the pipeline entrypoints (`warn_unknown_item_types`).
pub const KNOWN_INPUT_ITEM_TYPES: &[&str] = &[
    "message",
    "reasoning",
    "function_call",
    "function_call_output",
];

/// The original namespace + name of a tool whose Chat-Completions name was
/// encoded with [`chat_function_name_for_namespace_tool`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceToolName {
    pub namespace: String,
    pub name: String,
}

/// Chat-encoded tool name → original `{namespace, name}`. Built once per
/// request from the `namespace` entries the Codex client declared, so
/// response tool_calls can be decoded back to the Responses `namespace`
/// field Codex uses to dispatch namespaced tools (spec §7).
pub type NamespaceToolMap = HashMap<String, NamespaceToolName>;

/// Encode a namespace tool's Chat-Completions name as `{namespace}-{name}`
/// (e.g. `multi_agent_v1-spawn_agent`). The hyphen is a legal Chat tool-name
/// character (`^[a-zA-Z0-9_-]+$`) and matches codex-relay's encoding.
///
/// TODO: the encoding is NOT injective — `a`/`b-c` and `a-b`/`c` collide on
/// `a-b-c`. Collisions are handled first-wins by the decode map (tested), so
/// this is safe today; revisit (e.g. an escaping scheme) only if real
/// collisions surface in practice (spec §9).
pub fn chat_function_name_for_namespace_tool(namespace: &str, name: &str) -> String {
    format!("{namespace}-{name}")
}

/// Hoist tools out of Codex CLI's `additional_tools` input items into the
/// request's top-level `tools` array.
///
/// Codex ≥0.147 delivers its toolset as a non-standard input item -
/// `{"type": "additional_tools", "role": "developer", "tools": [...]}` -
/// instead of the top-level `tools` field, leaving `tools` empty. The
/// wrappers are `namespace` entries (`functions`, `collaboration`) whose
/// inner entries are ordinary Responses tool objects. OpenAI's backend
/// understands the item; third-party Responses upstreams do not: they bind
/// no tools, the model still emits DSML/agentic tool-call markup from its
/// prompt context, and the markup leaks into the visible text (the
/// `dsml_heal` quirk then has to rescue the chat path, and the responses
/// passthrough cannot rescue anything).
///
/// This flattens every `function`-shaped tool (directly or inside a
/// `namespace` wrapper) into `tools`, keeping the inner tool's own `name`
/// (the name the model's DSML calls use, e.g. `exec_command`). `custom` /
/// `freeform` inner tools are kept too - the chat conversion re-wraps them,
/// and Responses passthrough forwards them verbatim. Entries are deduplicated
/// by `name`, so a session replay that repeats the item cannot stack
/// duplicates. The caller decides separately whether to strip the consumed
/// items from the input it forwards ([`strip_additional_tools_items`]).
///
/// Additionally reports the tool types the hoist drops so the caller can
/// surface them (the visibility principle, spec §1):
///
/// - `unmappable`: type names of entries (top-level or namespace-inner)
///   with no Chat/Responses mapping — they are dropped by the hoist and
///   must not be silently swallowed just because they sat inside an
///   `additional_tools` item;
/// - `schema_conflicts`: names of same-named tool definitions skipped
///   because an earlier definition of the same name with a DIFFERENT schema
///   was already present.
///
/// Each caller merges the reports across its calls and emits ONE warn per
/// request (spec §4: at most one line per type per request).
///
/// `ns_map` is threaded through to [`push_namespace_inner_tools`] so the
/// chat path can encode namespace-inner function tools and build the decode
/// map. The chat call sites (`to_chat_request_with_ns_map`'s two hoist
/// calls — history and new-input) pass `Some(&mut ns_map)` (encode + record
/// the map); the responses path (`normalize_responses_request`) passes
/// `None` and leaves inner tools unencoded (spec §9 TODO).
///
/// This is one of TWO decode-map sources: the caller also merges the map
/// returned by [`normalize_chat_tools`] (top-level `namespace` entries)
/// with `or_insert`, so entries recorded here (the hoist) win on a
/// collision — in practice both sources encode the same `{namespace}-{name}`
/// string, so either order decodes identically (spec §7).
pub(crate) fn collect_additional_tools_reporting(
    items: &[Value],
    tools: &mut Vec<Value>,
    mut ns_map: Option<&mut NamespaceToolMap>,
) -> (Vec<String>, Vec<String>) {
    let mut unmappable = Vec::new();
    let mut schema_conflicts = Vec::new();
    for item in items {
        if item.get("type").and_then(Value::as_str) != Some("additional_tools") {
            continue;
        }
        let Some(entries) = item.get("tools").and_then(Value::as_array) else {
            continue;
        };
        for entry in entries {
            match entry.get("type").and_then(Value::as_str) {
                Some("namespace") => {
                    push_namespace_inner_tools(
                        tools,
                        entry,
                        &mut unmappable,
                        &mut schema_conflicts,
                        ns_map.as_deref_mut(),
                    );
                }
                _ => {
                    push_tool_classified(tools, entry, &mut unmappable, &mut schema_conflicts);
                }
            }
        }
    }
    (unmappable, schema_conflicts)
}

/// Append `tool` to `out` unless an entry with the same `name` is already
/// present. Returns `true` when appended, `false` when a same-named entry
/// won (the earlier definition is kept).
fn push_tool_if_new(out: &mut Vec<Value>, tool: &Value) -> bool {
    let name = tool.get("name").and_then(Value::as_str).unwrap_or("");
    if out
        .iter()
        .any(|existing| existing.get("name").and_then(Value::as_str) == Some(name))
    {
        return false;
    }
    out.push(tool.clone());
    true
}

/// Classify one tool entry into `out` and the two report lists, shared by
/// every normalization path (spec §2: namespace-inner and top-level entries
/// are isomorphic, so routing both through one classifier keeps the
/// responses hoist and the chat flatten from drifting).
///
/// Returns `true` when `tool` was appended to `out`, `false` when a
/// same-named entry won (or the type has no Chat mapping). Callers that
/// build derived state from the appended tool (e.g. the namespace decode
/// map) must only record it on a `true` return — a deduped tool never
/// reaches the upstream, so recording it would wrongly decode model calls
/// against a tool that is not bound (spec §7).
///
/// TODO: the return value is semantically meaningful for future callers —
/// ignoring it while building derived state silently drifts the decode map.
///
/// - function-shaped (`function` / `custom` / `freeform`) entries are
///   appended name-deduplicated. When a same-named entry is already present
///   with a different schema, the skipped definition's name is recorded in
///   `schema_conflicts` so the caller can warn — silently keeping a stale
///   schema upstream would be invisible.
/// - anything else has no Chat mapping: its type name is recorded in
///   `unmappable` (deduplicated) for the drop warn.
fn push_tool_classified(
    out: &mut Vec<Value>,
    tool: &Value,
    unmappable: &mut Vec<String>,
    schema_conflicts: &mut Vec<String>,
) -> bool {
    match tool.get("type").and_then(Value::as_str) {
        Some("function") | Some("custom") | Some("freeform") => {
            if push_tool_if_new(out, tool) {
                return true;
            }
            let name = tool.get("name").and_then(Value::as_str).unwrap_or("");
            let kept = out
                .iter()
                .find(|e| e.get("name").and_then(Value::as_str) == Some(name));
            if kept.is_some_and(|kept| kept != tool)
                && !schema_conflicts.contains(&name.to_string())
            {
                schema_conflicts.push(name.to_string());
            }
            false
        }
        other => {
            let t = other.unwrap_or("?").to_string();
            if !unmappable.contains(&t) {
                unmappable.push(t);
            }
            false
        }
    }
}

/// Flatten one `namespace` entry's inner `tools` through
/// [`push_tool_classified`]. Namespace inner entries are isomorphic to
/// top-level entries; this shared expansion is what keeps the responses
/// hoist and the chat flatten from drifting (spec §2).
///
/// When `ns_map` is `Some` (the chat path), inner `function` tools are
/// encoded to `{namespace}-{name}` and their decode mapping recorded in
/// `ns_map` — but ONLY when the encoded tool wins the name-dedup, so a
/// flat-bound same-named tool never gets a stale mapping. With `None` (the
/// responses hoist, spec §9 TODO) inner tools pass through under their own
/// name (no encoded copy is created; the classifier still clones the
/// original into `out` when it wins the name-dedup).
fn push_namespace_inner_tools(
    out: &mut Vec<Value>,
    entry: &Value,
    unmappable: &mut Vec<String>,
    schema_conflicts: &mut Vec<String>,
    mut ns_map: Option<&mut NamespaceToolMap>,
) {
    let namespace = entry.get("name").and_then(Value::as_str);
    if let Some(inner) = entry.get("tools").and_then(Value::as_array) {
        for tool in inner {
            if tool.get("type").and_then(Value::as_str) == Some("function") {
                if let (Some(map), Some(ns)) = (ns_map.as_deref_mut(), namespace) {
                    // Chat path: encode the inner function's name as
                    // `{namespace}-{name}` (spec §7).
                    if let Some(name) = tool.get("name").and_then(Value::as_str) {
                        let chat_name = chat_function_name_for_namespace_tool(ns, name);
                        let mut encoded = tool.clone();
                        encoded["name"] = Value::String(chat_name.clone());
                        // Record the decode mapping ONLY when the encoded
                        // tool wins the name-dedup: a flat-bound same-named
                        // tool keeps the upstream binding, and a stale map
                        // entry would wrongly decode model calls as
                        // namespaced (spec §7).
                        if push_tool_classified(out, &encoded, unmappable, schema_conflicts) {
                            map.entry(chat_name).or_insert(NamespaceToolName {
                                namespace: ns.to_string(),
                                name: name.to_string(),
                            });
                        }
                        continue;
                    }
                    // No name: fall through and classify by reference.
                }
            }
            // Responses hoist (spec §9 TODO) and non-function / unnamed
            // shapes: pass through unchanged, by reference (no clone).
            push_tool_classified(out, tool, unmappable, schema_conflicts);
        }
    }
}

/// Remove consumed `additional_tools` items from an input array.
///
/// Paired with [`collect_additional_tools`]: after hoisting the tools, the
/// item itself carries no information a third-party upstream understands
/// (and strict upstreams may reject the unknown item type), so the
/// responses-passthrough path strips it before forwarding.
pub fn strip_additional_tools_items(items: &mut Vec<Value>) {
    items.retain(|item| item.get("type").and_then(Value::as_str) != Some("additional_tools"));
}

/// Normalize one whole upstream request body (responses passthrough path).
///
/// Called by the proxy AFTER the history merge so replayed items are covered:
/// 1. hoists `additional_tools` input items into the top-level `tools`
///    (name-deduplicated) and strips the non-standard items;
/// 2. logs unknown input item types, which are passed through verbatim.
///
/// Ordering note: stripping runs BEFORE the unknown-type warning, so the
/// consumed `additional_tools` item is never spuriously counted as an
/// unknown dialect (it is deliberately NOT in [`KNOWN_INPUT_ITEM_TYPES`]).
pub fn normalize_responses_request(obj: &mut serde_json::Map<String, Value>) {
    // Seed the hoist target with the request's existing top-level tools so
    // hoisting dedups against them (parity with the chat path, which dedups
    // across sources): a tool bound both top-level and inside
    // `additional_tools` must not be double-bound upstream.
    let mut hoisted: Vec<Value> = obj
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let preexisting = hoisted.len();
    if let Some(input) = obj.get_mut("input").and_then(Value::as_array_mut) {
        let (unmappable, schema_conflicts) =
            collect_additional_tools_reporting(input, &mut hoisted, None);
        strip_additional_tools_items(input);
        warn_unknown_item_types(input.iter(), "passed through");
        // The hoist drops unmappable tools (top-level or namespace-inner)
        // and skips same-named definitions with a different schema; surface
        // both once per request so `additional_tools` never degrades
        // silently (spec §1 visibility principle).
        if !unmappable.is_empty() {
            bump_and_warn(
                "tool type(s) not mappable to chat dropped from additional_tools",
                &unmappable,
            );
        }
        if !schema_conflicts.is_empty() {
            bump_and_warn(
                "same-named tool definition with a different schema dropped",
                &schema_conflicts,
            );
        }
    }
    if hoisted.len() == preexisting {
        return;
    }
    let count = hoisted.len() - preexisting;
    obj.insert("tools".to_string(), Value::Array(hoisted));
    tracing::debug!("hoisted {count} additional_tools entr(y/es) into the top-level tools array");
}

/// Normalize the tool list for a Chat-Completions upstream.
///
/// Returns the flattened Chat tool list plus a [`NamespaceToolMap`] mapping
/// every encoded `{namespace}-{name}` back to its original
/// `{namespace, name}`.
///
/// - `function` / `custom` / `freeform` entries pass through unchanged
///   (name-deduplicated);
/// - `namespace` entries are flattened and their inner `function` tools are
///   encoded to `{namespace}-{name}` (e.g. `multi_agent_v1-spawn_agent`),
///   with each mapping recorded in the returned map — the single source of
///   truth the response side decodes tool_call names against (spec §7).
///   Without this, Chat upstreams silently lose Codex's namespaced toolsets
///   (e.g. the multi-agent tools);
/// - anything else (`web_search`, `local_shell`, …) has no Chat mapping and
///   is dropped with a logged counter.
pub fn normalize_chat_tools(tools: &[Value]) -> (Vec<Value>, NamespaceToolMap) {
    let mut ns_map = NamespaceToolMap::new();
    let mut out: Vec<Value> = Vec::new();
    let mut dropped: Vec<String> = Vec::new();
    let mut schema_conflicts: Vec<String> = Vec::new();
    for tool in tools {
        match tool.get("type").and_then(Value::as_str) {
            Some("namespace") => {
                push_namespace_inner_tools(
                    &mut out,
                    tool,
                    &mut dropped,
                    &mut schema_conflicts,
                    Some(&mut ns_map),
                );
            }
            _ => {
                push_tool_classified(&mut out, tool, &mut dropped, &mut schema_conflicts);
            }
        }
    }
    if !dropped.is_empty() {
        bump_and_warn("tool type(s) not mappable to chat dropped", &dropped);
    }
    if !schema_conflicts.is_empty() {
        bump_and_warn(
            "same-named tool definition with a different schema dropped",
            &schema_conflicts,
        );
    }
    (out, ns_map)
}

/// Surface dropped tool types from the chat-path hoist in one warn line.
///
/// `to_chat_request` hoists from history AND new input; it merges the two
/// reports and calls this once per request so a type is not double-warned
/// (spec §4: at most one line per type per request).
pub(crate) fn warn_dropped_tool_types(label: &str, types: &[String]) {
    bump_and_warn(label, types);
}

// ---- unknown-dialect visibility ----

/// Process-lifetime counts of unknown dialect types, keyed by type name.
/// Input item types and tool types share the map; labels disambiguate the two warn
/// families (a name appearing in both is counted once, which is acceptable for a
/// tripwire).
static UNKNOWN_TYPE_COUNTS: LazyLock<Mutex<HashMap<String, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Upper bound on distinct keys in [`UNKNOWN_TYPE_COUNTS`]. The key space is
/// client-controlled (unknown item/tool type strings), so an unbounded map
/// is an unbounded-memory vector; past this cap new keys are still warned
/// about but not remembered (their displayed total reflects only the current
/// call). 256 is far beyond every real dialect's type vocabulary.
const MAX_UNKNOWN_TYPE_KEYS: usize = 256;

/// Log one warn line for unknown input item types in `items`, bumping the
/// process-lifetime counters. `disposition` names what happens to the
/// unknown items on this path — `"passed through"` for the Responses
/// passthrough (spec §4), `"dropped"` for Chat conversion — so the log line
/// distinguishes data loss from passthrough. Known types are ignored; each
/// unknown type counts once per call (per request). One line per call.
pub fn warn_unknown_item_types<'a>(items: impl Iterator<Item = &'a Value>, disposition: &str) {
    let mut unknown: Vec<String> = Vec::new();
    for item in items {
        let Some(item_type) = item.get("type").and_then(Value::as_str) else {
            continue;
        };
        if !KNOWN_INPUT_ITEM_TYPES.contains(&item_type) && !unknown.iter().any(|u| u == item_type) {
            unknown.push(item_type.to_string());
        }
    }
    if !unknown.is_empty() {
        bump_and_warn(
            &format!("unknown input item type(s) {disposition}"),
            &unknown,
        );
    }
}

/// Bump the counters for each type in `types` (once each) and log one warn
/// line: `<label>: a (total N), b (total M)`.
///
/// Duplicate types in `types` are deduplicated before bumping/logging (spec
/// §4: at most one line per type per request), so repeated unmappable tools
/// of the same type
/// — top-level repeats, or a top-level tool arriving after a namespace-inner
/// one of the same type — never double-bump the counter or repeat in the
/// same warn line.
fn bump_and_warn(label: &str, types: &[String]) {
    let mut counts = UNKNOWN_TYPE_COUNTS.lock().unwrap();
    let mut seen: HashSet<&str> = HashSet::new();
    let parts: Vec<String> = types
        .iter()
        .filter(|t| seen.insert(t.as_str()))
        .map(|t| {
            // Computed before the match: the `get_mut` borrow must not be
            // alive while `counts` is read again.
            let at_cap = counts.len() >= MAX_UNKNOWN_TYPE_KEYS;
            match counts.get_mut(t.as_str()) {
                Some(n) => {
                    *n += 1;
                    format!("{t} (total {n})")
                }
                None if at_cap => {
                    // Counter map full: warn without persisting so the map
                    // (and the process heap) stays bounded on client-
                    // controlled key spam.
                    format!("{t} (untracked: counter map at cap)")
                }
                None => {
                    let n = counts.entry(t.clone()).or_insert(0);
                    *n += 1;
                    format!("{t} (total {n})")
                }
            }
        })
        .collect();
    tracing::warn!("{label}: {}", parts.join(", "));
}

/// Snapshot of the unknown-type counters (test helper).
#[cfg(test)]
pub(crate) fn unknown_type_counts() -> HashMap<String, u64> {
    UNKNOWN_TYPE_COUNTS.lock().unwrap().clone()
}

#[cfg(test)]
mod tests;
