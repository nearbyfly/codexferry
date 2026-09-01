//! `gen-catalog` subcommand: generate a Codex `model_catalog_json` file.
//!
//! Codex TUI shows a "Model metadata not found" warning for models that are
//! not in its bundled catalog. This offline tool reads the router config and
//! emits a catalog JSON (`{"models": [...]}`) that the user points Codex at
//! via `model_catalog_json` in `~/.codex/config.toml`, so the proxy's
//! `provider/alias` models appear with proper names and context windows
//! (spec §13).
//!
//! ## What gets generated
//!
//! One catalog entry per configured route:
//!
//! * `slug` / `display_name` = the route key (kept identical to `-m`).
//! * `context_window` / `max_context_window` = the route's `context_window`.
//! * `supported_in_api`, `visibility`, `prefer_websockets` are set so Codex
//!   lists the model and uses plain HTTP (no websockets).
//!
//! Entries are emitted in **sorted route-key order**, so the output file is
//! byte-reproducible across runs (a `HashMap`'s iteration order is not
//! deterministic).
//!
//! ## Template inheritance (deny-by-default)
//!
//! When a Codex `models.json` template is found (see [`load_template`]), the
//! route entry is seeded from a CLONE of the template's first "list"-visible
//! model FILTERED by [`INHERITED_FIELDS`]: only `base_instructions` and
//! `model_messages` survive (the prompt fields that track the installed
//! Codex version). Everything else the template carries — dialect switches
//! (`use_responses_lite`, `tool_mode`, `shell_type`, …), OpenAI-ecosystem
//! flags (`include_*`, `service_tiers`, `upgrade`, …), TUI decoration — is
//! dropped, and the dropped names are returned so `run_gen_catalog` can log
//! them. A future template field the router has not reviewed is therefore
//! inert by default, instead of leaking onto the wire (the 2026-08-17
//! `use_responses_lite` DSML leak was exactly this class).

use crate::config::{Config, ValidatedConfig};
use serde_json::{json, Map, Value};
use std::path::Path;
use std::time::Duration;

/// Generation output: the catalog JSON plus the template field names dropped
/// by the allowlist (logged by `run_gen_catalog`, ignored by callers that
/// only need the catalog).
pub struct GeneratedCatalog {
    pub catalog: Value,
    pub dropped_template_fields: Vec<String>,
}

/// Generate the model catalog in memory (no file writes).
///
/// Same loading path as [`run_gen_catalog`]: parse + validate config, locate
/// the template, build one entry per route in sorted key order (output is
/// byte-reproducible: serde_json's Map is a BTreeMap).
pub fn generate_catalog(
    config_path: &Path,
    codex_models: Option<&Path>,
) -> anyhow::Result<GeneratedCatalog> {
    let validated = Config::parse_file(config_path)?.validate()?;
    let template = load_template(codex_models)?.map(|(_path, val)| val);
    Ok(build_catalog_value(&validated, template.as_ref()))
}

/// Build one catalog entry from an empty seed (no template).
///
/// Shared by the generator's no-template path (`generate_catalog`) and
/// `doctor --live`, which synthesizes entries for its temporary probe
/// routes. Its only callers are those two production paths plus the unit
/// tests that pin the from-scratch shape.
pub fn build_catalog_entry(
    route_key: &str,
    provider_format: &str,
    context_window: u64,
    effort: Option<&str>,
) -> Value {
    let mut obj = Map::new();
    // From-scratch path: pin the neutral base_instructions placeholder
    // (codex >= 0.147 rejects entries carrying neither base_instructions nor
    // model_messages.instructions_template); template-seeded entries keep the
    // inherited value because set_catalog_fields never touches the field.
    obj.insert(
        "base_instructions".into(),
        json!(FALLBACK_BASE_INSTRUCTIONS),
    );
    set_catalog_fields(
        &mut obj,
        route_key,
        provider_format,
        context_window,
        effort,
        /* description */ None,
    );
    Value::Object(obj)
}

/// Build the `{"models": [...]}` catalog value from an already-validated
/// config and an optional parsed template. Shared by `gen-catalog` and the
/// `/models` endpoint (spec: single catalog-building code path).
pub fn build_catalog_value(
    validated: &ValidatedConfig,
    template: Option<&Value>,
) -> GeneratedCatalog {
    let mut dropped: Vec<String> = Vec::new();
    let mut models: Vec<Value> = Vec::new();

    let mut route_keys: Vec<&String> = validated.routes.keys().collect();
    route_keys.sort();
    for route_key in route_keys {
        let route = &validated.routes[route_key];
        // Seed: allowlist-filtered template clone, or empty without a
        // template. Both branches then get the same pinned fields, so the
        // two paths cannot drift apart.
        let mut seed: Map<String, Value> = match template {
            Some(t) => {
                let template_entry = t
                    .get("models")
                    .and_then(|m| m.as_array())
                    .and_then(|arr| {
                        arr.iter()
                            .find(|m| m.get("visibility").and_then(|v| v.as_str()) == Some("list"))
                            .or_else(|| arr.first())
                    })
                    .and_then(|e| e.as_object().cloned())
                    .unwrap_or_default();
                let mut entry = template_entry;
                dropped.extend(filter_template_entry(&mut entry));
                entry
            }
            None => Map::new(),
        };
        // From-scratch seeds (no template, or a template with no usable
        // entry) must pin the neutral base_instructions placeholder or the
        // entry is invalid for codex >= 0.147 (see
        // FALLBACK_BASE_INSTRUCTIONS). `entry().or_insert` never overwrites:
        // template-seeded entries keep their inherited value.
        seed.entry("base_instructions")
            .or_insert_with(|| json!(FALLBACK_BASE_INSTRUCTIONS));
        set_catalog_fields(
            &mut seed,
            route_key,
            &route.provider.format,
            route.context_window,
            route.default_reasoning_effort.as_deref(),
            route.description.as_deref(),
        );
        models.push(Value::Object(seed));
    }
    dropped.sort();
    dropped.dedup();
    GeneratedCatalog {
        catalog: json!({ "models": models }),
        dropped_template_fields: dropped,
    }
}

/// Generate the model catalog and write it to `out`.
///
/// Steps:
/// 1. Load and validate the router config (same code path as the server).
/// 2. Locate an optional Codex `models.json` template
///    ([`load_template`]).
/// 3. Build one catalog entry per route (iterated in sorted key order),
///    seeding each entry from the template when available, else from
///    scratch.
/// 4. Write the pretty-printed `{"models": [...]}` JSON to `out`.
///
/// Any failure (config parse, template read, JSON serialization, file
/// write) is returned to `main` as an `anyhow::Error`, which aborts with a
/// non-zero exit code.
pub fn run_gen_catalog(
    config_path: &Path,
    out: &Path,
    codex_models: Option<&Path>,
) -> anyhow::Result<()> {
    // The gen-catalog CLI path normally runs WITHOUT a tracing subscriber
    // (logging is initialized only in server mode), so the dropped-field
    // tripwire below would otherwise print nothing. Initialize once: an
    // unconditional init() panics when a global subscriber already exists
    // (e.g. if a future caller invokes us from a running server).
    GEN_CATALOG_LOG_INIT.call_once(|| {
        if !tracing::dispatcher::has_been_set() {
            crate::logging::init();
        }
    });
    let generated = generate_catalog(config_path, codex_models)?;
    if generated.dropped_template_fields.is_empty() {
        tracing::info!("template carried no fields outside the allowlist");
    } else {
        tracing::info!(
            "dropped {} template field(s) outside the allowlist: {}",
            generated.dropped_template_fields.len(),
            generated.dropped_template_fields.join(", ")
        );
    }
    let json_str = serde_json::to_string_pretty(&generated.catalog)?;
    std::fs::write(out, json_str)?;
    tracing::info!("catalog written to {}", out.display());
    Ok(())
}

/// One-shot guard for the gen-catalog logging init inside
/// [`run_gen_catalog`]. `has_been_set()` is a check-then-act: two concurrent
/// callers (e.g. parallel unit tests invoking `run_gen_catalog`) could both
/// pass the guard and race into `logging::init()`, whose `.init()` panics
/// when a global subscriber is already registered. `Once` serializes them;
/// the inner `has_been_set()` check then only matters when a subscriber was
/// installed by something else (e.g. a future caller that already
/// initialized logging).
static GEN_CATALOG_LOG_INIT: std::sync::Once = std::sync::Once::new();

/// Template fields allowed through to generated entries (deny-by-default).
///
/// Only the two prompt fields that track the installed Codex version. A new
/// field deliberately added here needs a review against how Codex uses it on
/// the wire — see the 2026-08-17 DSML leak postmortem.
const INHERITED_FIELDS: &[&str] = &["base_instructions", "model_messages"];

/// Neutral `base_instructions` for from-scratch catalog entries.
///
/// Codex >= 0.147 rejects any model entry that carries neither
/// `base_instructions` nor `model_messages.instructions_template` ("model X
/// is missing both"). Template-seeded entries keep the inherited template
/// value (the allowlist's `base_instructions` inheritance is untouched), but
/// the no-template path must pin a neutral placeholder or the generated
/// catalog is invalid on a template-less host. `set_catalog_fields` must NOT
/// overwrite an inherited `base_instructions`.
const FALLBACK_BASE_INSTRUCTIONS: &str = "You are a helpful assistant.";

/// Filter a cloned template entry down to [`INHERITED_FIELDS`], returning the
/// names of every dropped field (for the generation log: when a Codex upgrade
/// adds template fields, they show up here).
fn filter_template_entry(entry: &mut Map<String, Value>) -> Vec<String> {
    let dropped: Vec<String> = entry
        .keys()
        .filter(|k| !INHERITED_FIELDS.contains(&k.as_str()))
        .cloned()
        .collect();
    entry.retain(|k, _| INHERITED_FIELDS.contains(&k.as_str()));
    dropped
}

/// Overwrite the route-specific fields of a catalog entry.
///
/// Mutates `obj` in place, setting the fields that identify the model and
/// its context window. Any other fields the entry inherited from a template
/// (e.g. `base_instructions`) are left untouched. Shared by both the
/// template-seeded and the from-scratch code paths.
///
/// `default_reasoning_effort`, when `Some`, becomes the entry's
/// `default_reasoning_level` and the picker's initial choice. When `None`,
/// the field is removed so the template's own (GPT-specific) default cannot
/// leak into router routes; `supported_reasoning_levels` is ALWAYS emitted
/// (codex requires it, see below) so every level stays selectable.
fn set_catalog_fields(
    obj: &mut Map<String, Value>,
    route_key: &str,
    provider_format: &str,
    context_window: u64,
    default_reasoning_effort: Option<&str>,
    description: Option<&str>,
) {
    // slug/display_name are the route key itself, so they always match what
    // the user passes to `codex -m`.
    obj.insert("slug".into(), json!(route_key));
    obj.insert("display_name".into(), json!(route_key));
    obj.insert("context_window".into(), json!(context_window));
    obj.insert("max_context_window".into(), json!(context_window));
    // The model is served through the proxy's Responses endpoint and is not
    // websocket-capable, so advertise exactly that.
    obj.insert("supported_in_api".into(), json!(true));
    obj.insert("visibility".into(), json!("list"));
    obj.insert("prefer_websockets".into(), json!(false));
    // Force the full Responses wire format. The GPT-5.6 template entries
    // carry `use_responses_lite: true`, and under that "lite" mode Codex
    // (>=0.147) delivers its tools as `additional_tools` INPUT items
    // (namespace-wrapped, top-level `tools` empty) - an OpenAI-internal
    // extension third-party Responses upstreams cannot bind. With no tools
    // bound, the model's tool-call markup leaks into visible text (observed
    // as raw DSML from DeepSeek V4). Explicit `false` rather than removal:
    // the template value would otherwise be inherited.
    obj.insert("use_responses_lite".into(), json!(false));
    // Pinned tool-delivery fields (aligned with the proven-working hand
    // catalog): the classic shell toolset, parallel calls allowed, and a
    // neutral synthesized description instead of the template's GPT
    // marketing copy. `tool_mode` can no longer leak either: the allowlist
    // drops it from the template seed before this function ever runs (the
    // 2026-08-17 DSML leak was exactly this class of inheritance).
    obj.insert("shell_type".into(), json!("default"));
    obj.insert("supports_parallel_tool_calls".into(), json!(true));
    // Description: name the upstream wire format ("chat" / "responses")
    // so the picker line shows which interface the route sits on — the
    // passthrough and the converting path behave differently, and the
    // distinction is otherwise invisible (user request 2026-09-01).
    let description_value = match description {
        Some(desc) => format!("CodexFerry {provider_format} to {route_key} - {desc}"),
        None => format!("CodexFerry {provider_format} to {route_key}"),
    };
    obj.insert("description".into(), json!(description_value));
    // Required-by-Codex structural fields. Codex >= 0.147 deserializes the
    // catalog into a strict struct whose `priority`, `support_verbosity`,
    // `truncation_policy` and `experimental_supported_tools` have NO serde
    // default — a catalog missing any of them is rejected outright ("missing
    // field X") and no router route would load. They are pinned to neutral
    // values (the allowlist drops them from the template on purpose).
    // priority sorts the model picker (lower = earlier); 99 is the same
    // "lowest priority" the codex fallback uses for unknown models, so
    // router routes never displace bundled OpenAI models.
    obj.insert("priority".into(), json!(99));
    // The router does not implement verbosity controls, so don't advertise
    // them.
    obj.insert("support_verbosity".into(), json!(false));
    obj.insert(
        "truncation_policy".into(),
        json!({"mode": "tokens", "limit": 10000}),
    );
    obj.insert("experimental_supported_tools".into(), json!([]));
    // Reasoning fields: `supported_reasoning_levels` is REQUIRED by codex's
    // ModelInfo (no serde default), so the full standard ladder is always
    // emitted. `default_reasoning_level` is emitted only when the route
    // explicitly configured an effort — codex's picker snaps effort to it on
    // selection, so an inherited GPT default would silently override the
    // user's `model_reasoning_effort`. With none configured the field stays
    // absent and codex falls back to the config-level effort, while the
    // ladder keeps every level selectable.
    obj.insert("supported_reasoning_levels".into(), full_effort_ladder());
    match default_reasoning_effort {
        Some(effort) => {
            let normalized = if effort == "ultra" { "max" } else { effort };
            obj.insert("default_reasoning_level".into(), json!(normalized));
        }
        None => {
            obj.remove("default_reasoning_level");
        }
    }
}

/// Human-readable description for a reasoning effort, matching the wording
/// Codex uses in its own bundled catalog.
fn effort_description(effort: &str) -> &'static str {
    match effort {
        "none" => "No reasoning; direct responses",
        "minimal" => "Fastest responses with the lightest reasoning",
        "low" => "Fast responses with lighter reasoning",
        "medium" => "Balances speed and reasoning depth for everyday tasks",
        "xhigh" => "Extra high reasoning depth for complex problems",
        "max" | "ultra" => "Maximum reasoning depth for the hardest problems",
        _ => "Greater reasoning depth for complex problems",
    }
}

/// The full standard reasoning-effort ladder, selectable in the Codex picker
/// regardless of the configured default. All efforts Codex accepts as
/// `default_reasoning_effort` are present here (`ultra` is codex-managed
/// alias remapped to `max` on the wire, included for completeness).
fn full_effort_ladder() -> Value {
    json!([
        { "effort": "none", "description": effort_description("none") },
        { "effort": "minimal", "description": effort_description("minimal") },
        { "effort": "low", "description": effort_description("low") },
        { "effort": "medium", "description": effort_description("medium") },
        { "effort": "high", "description": effort_description("high") },
        { "effort": "xhigh", "description": effort_description("xhigh") },
        { "effort": "max", "description": effort_description("max") },
        { "effort": "ultra", "description": effort_description("ultra") },
    ])
}

/// Locate a Codex `models.json` template to inherit fields from.
///
/// Searches three tiers, in order, returning the first valid catalog found:
///
/// 1. **Explicit path** — `--codex-models <path>` if provided. A missing
///    path is only a warning (search continues); an existing path that
///    fails to read/parse is a hard error.
/// 2. **`codex debug models --bundled`** — ask the installed Codex CLI for
///    its bundled models (the codex-relay approach, spec §13). Failures are
///    silently ignored and the search continues.
/// 3. **Common file paths** — `~/.codex/models.json`,
///    `$XDG_DATA_HOME/codex/models.json`, then a few well-known system
///    locations (including the macOS app bundle). Individual candidates
///    that are unreadable or not valid JSON are logged and skipped, so one
///    bad file never breaks generation.
///
/// Returns `Ok(None)` when nothing is found — the caller then generates
/// minimal entries without version-sensitive fields.
fn load_template(
    codex_models: Option<&Path>,
) -> anyhow::Result<Option<(std::path::PathBuf, Value)>> {
    // 1. Try explicit path
    if let Some(path) = codex_models {
        if path.exists() {
            let text = std::fs::read_to_string(path)?;
            let val: Value = serde_json::from_str(&text)?;
            return Ok(Some((path.to_path_buf(), val)));
        }
        tracing::warn!(
            "explicit --codex-models path {} does not exist; falling back to other sources",
            path.display()
        );
    }

    // 2. Try `codex debug models --bundled` — bounded by the same deadline
    // as every other shell-out: this runs on the /models single-flight
    // refresh path, so a hung `codex` here would wedge every cold-start
    // catalog request behind it.
    if let Some(stdout) = bundled_command_stdout("codex", BUNDLED_DISCOVERY_TIMEOUT) {
        if let Ok(val) = serde_json::from_slice(&stdout) {
            return Ok(Some((std::path::PathBuf::new(), val)));
        }
    }

    // 3. Search common paths
    let home = std::env::var("HOME").unwrap_or_default();
    let xdg = std::env::var("XDG_DATA_HOME").unwrap_or_default();
    // Well-known locations, in order of likelihood. Empty `HOME`/`XDG_DATA_HOME`
    // produce paths like `/.codex/models.json`, which simply won't exist.
    let search_paths = [
        std::path::PathBuf::from(&home).join(".codex/models.json"),
        std::path::PathBuf::from(&xdg).join("codex/models.json"),
        "/usr/local/share/codex/models.json".into(),
        "/usr/share/codex/models.json".into(),
        "/Applications/Codex.app/Contents/Resources/models.json".into(),
    ];
    for path in &search_paths {
        if path.exists() {
            let text = match std::fs::read_to_string(path) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("failed to read candidate template {}: {e}", path.display());
                    continue;
                }
            };
            if let Ok(val) = serde_json::from_str(&text) {
                return Ok(Some((path.clone(), val)));
            }
            tracing::warn!("candidate template {} is not valid JSON", path.display());
        }
    }

    tracing::info!("no Codex model template found, generating catalog with neutral placeholder");
    Ok(None)
}

/// Reload the models.json template for the /models endpoint. Returns the
/// resolved path (for mtime tracking) and the parsed template. Returns
/// (None, None) when no template is found; logs a warning on read failure.
pub(crate) fn reload_template() -> (Option<std::path::PathBuf>, Option<Value>) {
    match load_template(None) {
        Ok(Some((path, val))) => {
            // Use the path directly from load_template (empty PathBuf = no file,
            // e.g. shell-out source). Avoids duplicating path-discovery logic.
            let real_path = if path.as_os_str().is_empty() {
                None
            } else {
                Some(path)
            };
            (real_path, Some(val))
        }
        Ok(None) => (None, None),
        Err(e) => {
            tracing::warn!("template reload failed: {e}");
            (None, None)
        }
    }
}

/// Bound on a single `codex debug models --bundled` discovery run. A hung
/// `codex` must not wedge the `/models` request path or the config
/// hot-reload writer indefinitely (Task 4 review; stdout is drained on a
/// reader thread while waiting, so a large catalog cannot deadlock on the
/// pipe buffer).
const BUNDLED_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// Discover the installed Codex CLI's bundled model catalog by shelling out
/// to `codex debug models --bundled` (hide-bundled spec §Decisions 3: the
/// ONLY source for hide overrides — `load_template`'s file fallbacks may be
/// user-managed and are not guaranteed to equal the binary's bundled list).
/// Any failure (no codex on PATH, non-zero exit, unparseable output)
/// degrades to an empty vec: hiding is best-effort and never breaks the
/// catalog (spec §Decisions 4).
pub(crate) fn discover_bundled_models() -> Vec<Value> {
    bundled_from_command("codex")
}

/// Shell out to `cmd debug models --bundled` and return its `models` array.
/// Split from [`discover_bundled_models`] so unit tests can point at a fake
/// binary without mutating process-global `PATH` (which races parallel
/// tests — spec §Freshness).
fn bundled_from_command(cmd: &str) -> Vec<Value> {
    bundled_from_command_with_timeout(cmd, BUNDLED_DISCOVERY_TIMEOUT)
}

/// Like [`bundled_from_command`] but bounded by `timeout`: on expiry the
/// child is killed and the result degrades to empty (spec §Decisions 4).
/// Split out so the timeout path is testable without waiting out the real
/// 10s constant.
fn bundled_from_command_with_timeout(cmd: &str, timeout: Duration) -> Vec<Value> {
    bundled_command_stdout(cmd, timeout)
        .map(|stdout| parse_bundled_output(&stdout))
        .unwrap_or_default()
}

/// Run `cmd debug models --bundled` bounded by `timeout`; return the raw
/// stdout on a zero-exit run, `None` on spawn failure, non-zero exit, or
/// timeout (child killed, reaped). The single bounded runner shared by BOTH
/// shell-out call sites — [`bundled_from_command_with_timeout`] (hide
/// overrides) and [`load_template`] tier 2 (template discovery) — so neither
/// can wedge its caller on a hung `codex` (load_template's tier-2
/// previously used bare `Command::output()`, which has no deadline, while
/// running inside the /models single-flight refresh).
///
/// stdout is drained on a reader thread while the deadline is polled, so a
/// large catalog cannot deadlock the child on the pipe buffer.
fn bundled_command_stdout(cmd: &str, timeout: Duration) -> Option<Vec<u8>> {
    let Ok(mut child) = std::process::Command::new(cmd)
        .args(["debug", "models", "--bundled"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return None;
    };
    // Read stdout on a separate thread so a large catalog (real codex emits
    // ~315 KB, far beyond the 64 KB pipe buffer) cannot deadlock the child on
    // write(2) while we only poll try_wait (same approach Command::output()
    // uses internally).
    let stdout_pipe = child.stdout.take();
    // Named for /proc/<pid>/task visibility (SWR spec §Thread naming);
    // spawn failure degrades like every other discovery failure.
    let reader = match std::thread::Builder::new()
        .name("bundled-reader".to_string())
        .spawn(move || {
            use std::io::Read;
            let mut buf = Vec::new();
            if let Some(mut out) = stdout_pipe {
                let _ = out.read_to_end(&mut buf);
            }
            buf
        }) {
        Ok(handle) => handle,
        // The child would keep running with no reader; reap it before
        // degrading.
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().ok().flatten() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = reader.join().unwrap_or_default();
    if !status.success() {
        return None;
    }
    Some(stdout)
}

/// Parse the `{"models": [...]}` stdout of `codex debug models --bundled`.
/// Anything that is not that shape degrades to an empty vec.
fn parse_bundled_output(stdout: &[u8]) -> Vec<Value> {
    serde_json::from_slice::<Value>(stdout)
        .ok()
        .and_then(|v| v.get("models").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

/// Build override entries that hide the Codex-bundled models from the
/// dynamic-mode picker (hide-bundled spec §Mechanism).
///
/// Every bundled entry with `visibility: "list"` is CLONED with visibility
/// flipped to `"hide"`: codex's dynamic merge (`apply_remote_models`)
/// replaces bundled entries by slug, and `show_in_picker` is true only for
/// `"list"`, so the clone suppresses the picker entry while preserving every
/// other field — the clone round-trips codex's own serialization, so all
/// fields required by codex's strict `ModelInfo` deserialization stay
/// present. Slug-less entries are dropped (codex replaces by slug, so a
/// slug-less clone is dead weight). Entries already hidden need no override,
/// and slugs that collide with a configured route key are skipped — the
/// route must stay selectable. Output is sorted by slug so the response body
/// is byte-reproducible across rebuilds.
pub(crate) fn build_hide_entries(
    bundled: &[Value],
    route_keys: &std::collections::HashSet<&str>,
) -> Vec<Value> {
    let mut out: Vec<Value> = bundled
        .iter()
        .filter(|m| m.get("visibility").and_then(Value::as_str) == Some("list"))
        .filter(|m| {
            m.get("slug")
                .and_then(Value::as_str)
                .is_some_and(|slug| !route_keys.contains(slug))
        })
        .map(|m| {
            let mut entry = m.clone();
            // Filters above guarantee this is an object (visibility is a string).
            if let Some(obj) = entry.as_object_mut() {
                obj.insert("visibility".into(), json!("hide"));
            }
            entry
        })
        .collect();
    out.sort_by(|a, b| {
        let slug_a = a.get("slug").and_then(Value::as_str).unwrap_or("");
        let slug_b = b.get("slug").and_then(Value::as_str).unwrap_or("");
        slug_a.cmp(slug_b)
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_reasoning_effort_overrides_template_defaults() {
        // A route with `default_reasoning_effort` set must both advertise that
        // effort as the catalog default AND restrict the picker to it; a
        // route without one must have no reasoning fields at all (nothing
        // inherited from the GPT template's `low` default).
        let template_text = r#"
{
  "models": [
    {
      "slug": "gpt-5.6-sol",
      "visibility": "list",
      "tool_mode": "code_mode_only",
      "default_reasoning_level": "low",
      "supported_reasoning_levels": [
        { "effort": "low" }, { "effort": "medium" }, { "effort": "high" }
      ]
    }
  ]
}
"#;
        let template_dir = tempfile::tempdir().unwrap();
        let template_path = template_dir.path().join("models.json");
        std::fs::write(&template_path, template_text).unwrap();

        let config_text = r#"
[providers.x]
base_url = "https://x.com/v1"
api_key = "k"
format = "responses"
[routes]
"x/model-a" = { model = "a", context_window = 131072, default_reasoning_effort = "high" }
"x/model-b" = { model = "b", context_window = 131072 }
"#;
        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("config.toml");
        std::fs::write(&config_path, config_text).unwrap();

        let out_dir = tempfile::tempdir().unwrap();
        let out_path = out_dir.path().join("catalog.json");

        run_gen_catalog(&config_path, &out_path, Some(&template_path)).unwrap();

        let catalog: Value =
            serde_json::from_str(&std::fs::read_to_string(&out_path).unwrap()).unwrap();
        let models = catalog["models"].as_array().unwrap();
        let a = models
            .iter()
            .find(|m| m["slug"] == "x/model-a")
            .expect("route x/model-a in catalog");
        assert_eq!(a["default_reasoning_level"], "high");
        // The full standard ladder stays selectable; the configured effort
        // is only the default, not the sole choice.
        assert_eq!(
            a["supported_reasoning_levels"],
            json!([
                { "effort": "none", "description": "No reasoning; direct responses" },
                { "effort": "minimal", "description": "Fastest responses with the lightest reasoning" },
                { "effort": "low", "description": "Fast responses with lighter reasoning" },
                { "effort": "medium", "description": "Balances speed and reasoning depth for everyday tasks" },
                { "effort": "high", "description": "Greater reasoning depth for complex problems" },
                { "effort": "xhigh", "description": "Extra high reasoning depth for complex problems" },
                { "effort": "max", "description": "Maximum reasoning depth for the hardest problems" },
                { "effort": "ultra", "description": "Maximum reasoning depth for the hardest problems" },
            ])
        );
        let b = models
            .iter()
            .find(|m| m["slug"] == "x/model-b")
            .expect("route x/model-b in catalog");
        // No configured effort: no forced default (codex falls back to the
        // config-level effort) — but the ladder must still be present, since
        // codex's ModelInfo requires `supported_reasoning_levels`.
        assert!(b.get("default_reasoning_level").is_none());
        assert!(b.get("supported_reasoning_levels").is_some());
        assert_eq!(b["supported_reasoning_levels"][0]["effort"], "none");
    }

    #[test]
    fn template_models_are_not_seeded_into_output() {
        // The template exists only for field inheritance. Seeding its own
        // models into the output would list models the router cannot serve
        // (e.g. official GPT models with no matching route → "no route for
        // model" errors when selected in the Codex TUI).
        let template_text = r#"
{
  "models": [
    { "slug": "gpt-5.6-sol", "visibility": "list", "tool_mode": "code_mode_only" },
    { "slug": "gpt-5.6-luna", "visibility": "list", "tool_mode": "code_mode_only" }
  ]
}
"#;
        let template_dir = tempfile::tempdir().unwrap();
        let template_path = template_dir.path().join("models.json");
        std::fs::write(&template_path, template_text).unwrap();

        let config_text = r#"
[providers.x]
base_url = "https://x.com/v1"
api_key = "k"
format = "chat"
[routes]
"x/model-a" = { model = "a", context_window = 131072 }
"#;
        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("config.toml");
        std::fs::write(&config_path, config_text).unwrap();

        let out_dir = tempfile::tempdir().unwrap();
        let out_path = out_dir.path().join("catalog.json");

        run_gen_catalog(&config_path, &out_path, Some(&template_path)).unwrap();

        let catalog: Value =
            serde_json::from_str(&std::fs::read_to_string(&out_path).unwrap()).unwrap();
        let models = catalog["models"].as_array().unwrap();
        let slugs: Vec<&str> = models.iter().map(|m| m["slug"].as_str().unwrap()).collect();
        assert_eq!(slugs, vec!["x/model-a"]);
    }

    #[test]
    fn template_tool_mode_does_not_leak_into_routes() {
        // A Codex models.json whose visible entry is code-mode-only (like the
        // bundled GPT-5.x family). Router routes must not inherit it: they are
        // served through the Responses endpoint and have no code-mode host.
        let template_text = r#"
{
  "models": [
    {
      "slug": "gpt-5.6-sol",
      "visibility": "list",
      "tool_mode": "code_mode_only"
    }
  ]
}
"#;
        let template_dir = tempfile::tempdir().unwrap();
        let template_path = template_dir.path().join("models.json");
        std::fs::write(&template_path, template_text).unwrap();

        let config_text = r#"
[providers.x]
base_url = "https://x.com/v1"
api_key = "k"
format = "chat"
[routes]
"x/model-a" = { model = "a", context_window = 131072 }
"#;
        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("config.toml");
        std::fs::write(&config_path, config_text).unwrap();

        let out_dir = tempfile::tempdir().unwrap();
        let out_path = out_dir.path().join("catalog.json");

        run_gen_catalog(&config_path, &out_path, Some(&template_path)).unwrap();

        let catalog: Value =
            serde_json::from_str(&std::fs::read_to_string(&out_path).unwrap()).unwrap();
        let models = catalog["models"].as_array().unwrap();
        let a = models
            .iter()
            .find(|m| m["slug"] == "x/model-a")
            .expect("route x/model-a in catalog");
        assert!(
            a.get("tool_mode").is_none(),
            "route entries must not carry a tool_mode: absent selects the classic \
             top-level `tools` delivery that third-party upstreams understand, while \
             `direct` makes Codex send its unified-exec toolset via a non-standard \
             `additional_tools` input item"
        );
        assert_eq!(
            a["use_responses_lite"], false,
            "the template's responses-lite mode makes Codex deliver tools via \
             `additional_tools` input items, which third-party upstreams cannot bind"
        );
    }

    #[test]
    fn routes_always_present_in_catalog() {
        let config_text = r#"
[providers.x]
base_url = "https://x.com/v1"
api_key = "k"
format = "chat"
[routes]
"x/model-a" = { model = "a", context_window = 131072 }
"x/model-b" = { model = "b" }
"#;
        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("config.toml");
        std::fs::write(&config_path, config_text).unwrap();

        let out_dir = tempfile::tempdir().unwrap();
        let out_path = out_dir.path().join("catalog.json");

        run_gen_catalog(&config_path, &out_path, None).unwrap();

        let catalog: Value =
            serde_json::from_str(&std::fs::read_to_string(&out_path).unwrap()).unwrap();
        let models = catalog["models"].as_array().unwrap();
        // The route entries must be present regardless of whether a Codex template
        // was found on the host (e.g. via `codex debug models`); iteration order
        // over the routes map is not deterministic, so match by slug.
        assert!(models.len() >= 2);
        let a = models
            .iter()
            .find(|m| m["slug"] == "x/model-a")
            .expect("route x/model-a in catalog");
        assert_eq!(a["context_window"], 131072);
        let b = models
            .iter()
            .find(|m| m["slug"] == "x/model-b")
            .expect("route x/model-b in catalog");
        assert_eq!(b["context_window"], 1048576);
    }

    /// A template entry carrying every field family we know leaks from the
    /// bundled GPT catalog: dialect switches, OpenAI-ecosystem flags, TUI
    /// decoration. Only base_instructions/model_messages may survive.
    fn dialect_template() -> String {
        r#"{
  "models": [
    {
      "slug": "gpt-5.6-sol",
      "visibility": "list",
      "tool_mode": "code_mode_only",
      "use_responses_lite": true,
      "shell_type": "shell_command",
      "apply_patch_tool_type": "freeform",
      "web_search_tool_type": "text_and_image",
      "supports_search_tool": true,
      "include_apps_usage_instructions": true,
      "include_plugin_usage_instructions": true,
      "service_tiers": [{"id": "priority"}],
      "upgrade": null,
      "availability_nux": {"message": "nudge"},
      "multi_agent_version": "v2",
      "input_modalities": ["text", "image"],
      "comp_hash": "3000",
      "effective_context_window_percent": 95,
      "truncation_policy": {"mode": "tokens", "limit": 10000},
      "base_instructions": "You are Codex.",
      "model_messages": {"instructions_template": "You are Codex."}
    }
  ]
}"#
        .to_string()
    }

    fn write_template(dir: &std::path::Path, text: &str) -> std::path::PathBuf {
        let p = dir.join("models.json");
        std::fs::write(&p, text).unwrap();
        p
    }

    const ROUTE_CONFIG: &str = r#"
[providers.x]
base_url = "https://x.com/v1"
api_key = "k"
format = "chat"
[routes]
"x/model-a" = { model = "a", context_window = 131072 }
"#;

    const ROUTE_CONFIG_WITH_DESC: &str = r#"
[providers.x]
base_url = "https://x.com/v1"
api_key = "k"
format = "chat"
[routes]
"x/model-a" = { model = "a", context_window = 131072, description = "fast coding model" }
"#;

    fn gen_with(dir: &std::path::Path, template: Option<&std::path::Path>) -> GeneratedCatalog {
        let config_path = dir.join("config.toml");
        std::fs::write(&config_path, ROUTE_CONFIG).unwrap();
        generate_catalog(&config_path, template).unwrap()
    }

    fn gen_with_desc(
        dir: &std::path::Path,
        template: Option<&std::path::Path>,
    ) -> GeneratedCatalog {
        let config_path = dir.join("config.toml");
        std::fs::write(&config_path, ROUTE_CONFIG_WITH_DESC).unwrap();
        generate_catalog(&config_path, template).unwrap()
    }

    #[test]
    fn description_fallback_names_the_wire_format() {
        let dir = tempfile::tempdir().unwrap();
        let generated = gen_with(dir.path(), None);
        let entry = &generated.catalog["models"][0];
        assert_eq!(
            entry["description"], "CodexFerry chat to x/model-a",
            "fallback description must name the wire format and the route"
        );
    }

    #[test]
    fn configurable_description_appends_after_route_key() {
        let dir = tempfile::tempdir().unwrap();
        let generated = gen_with_desc(dir.path(), None);
        let entry = &generated.catalog["models"][0];
        assert_eq!(
            entry["description"], "CodexFerry chat to x/model-a - fast coding model",
            "custom description must be appended after the wire format + route with a dash separator"
        );
    }

    #[test]
    fn allowlist_keeps_only_prompt_fields_and_reports_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let template = write_template(dir.path(), &dialect_template());
        let generated = gen_with(dir.path(), Some(&template));

        let entry = &generated.catalog["models"][0];
        let keys: Vec<&str> = entry
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        // Inherited (sorted, BTreeMap): only the two prompt fields.
        assert!(keys.contains(&"base_instructions"));
        assert!(keys.contains(&"model_messages"));
        // Dialect / ecosystem fields all gone. `use_responses_lite`,
        // `shell_type` and `truncation_policy` are deliberately NOT in this
        // list: they are pinned to neutral values (false / "default" /
        // tokens-10000) by set_catalog_fields, so the template's dialect
        // VALUES must be gone but the fields themselves remain — see
        // pinned_fields_are_written_regardless_of_template.
        for banned in [
            "tool_mode",
            "apply_patch_tool_type",
            "web_search_tool_type",
            "supports_search_tool",
            "include_apps_usage_instructions",
            "include_plugin_usage_instructions",
            "service_tiers",
            "upgrade",
            "availability_nux",
            "multi_agent_version",
            "input_modalities",
            "comp_hash",
            "effective_context_window_percent",
        ] {
            assert!(
                !keys.contains(&banned),
                "{banned} must not survive the allowlist"
            );
        }
        // Dropped-field report covers the dialect switches.
        assert!(generated
            .dropped_template_fields
            .contains(&"use_responses_lite".to_string()));
        assert!(generated
            .dropped_template_fields
            .contains(&"tool_mode".to_string()));
    }

    #[test]
    fn pinned_fields_are_written_regardless_of_template() {
        let dir = tempfile::tempdir().unwrap();
        let template = write_template(dir.path(), &dialect_template());
        let generated = gen_with(dir.path(), Some(&template));
        let entry = &generated.catalog["models"][0];
        // Dialect switches pinned to the full public Responses wire format.
        assert_eq!(entry["use_responses_lite"], false);
        assert_eq!(entry["shell_type"], "default");
        assert_eq!(entry["supports_parallel_tool_calls"], true);
        assert_eq!(entry["prefer_websockets"], false);
        assert_eq!(entry["description"], "CodexFerry chat to x/model-a");
        assert_eq!(entry["slug"], "x/model-a");
        assert_eq!(entry["context_window"], 131072);
        // Required-by-codex structural fields are pinned to neutral values
        // (they have no serde default in codex's ModelInfo, so a catalog
        // without them is rejected outright).
        assert_eq!(entry["priority"], 99);
        assert_eq!(entry["support_verbosity"], false);
        assert_eq!(
            entry["truncation_policy"],
            json!({"mode": "tokens", "limit": 10000})
        );
        assert_eq!(entry["experimental_supported_tools"], json!([]));
        // The reasoning ladder is always present (also required by codex).
        assert_eq!(entry["supported_reasoning_levels"][0]["effort"], "none");
    }

    #[test]
    fn template_and_scratch_entries_emit_identical_field_sets() {
        // Both branches are explicit (no host Codex template lookup), so this
        // is deterministic on any machine: template inheritance must add
        // EXACTLY one reviewed prompt field on top of the from-scratch shape —
        // `model_messages`. `base_instructions` is present in BOTH branches
        // (inherited template value vs the pinned neutral placeholder), so it
        // is not part of the symmetric difference.
        let dir = tempfile::tempdir().unwrap();
        let template = write_template(dir.path(), &dialect_template());
        let with = gen_with(dir.path(), Some(&template));
        let dir2 = tempfile::tempdir().unwrap();
        let without = gen_with(
            dir2.path(),
            Some(&write_template(dir2.path(), r#"{"models": []}"#)),
        );
        let a = with.catalog["models"][0].as_object().unwrap();
        let b = without.catalog["models"][0].as_object().unwrap();
        let ka: std::collections::BTreeSet<&str> = a.keys().map(|k| k.as_str()).collect();
        let kb: std::collections::BTreeSet<&str> = b.keys().map(|k| k.as_str()).collect();
        // Inherited-only field: the one reviewed prompt field the scratch
        // entry cannot synthesize (model_messages is version-coupled).
        let extra: Vec<&str> = ka.difference(&kb).copied().collect();
        assert_eq!(extra, vec!["model_messages"]);
        // The from-scratch shape is fully contained in the inherited shape.
        assert!(kb.is_subset(&ka));
        // The from-scratch branch carries the pinned neutral placeholder.
        assert_eq!(
            without.catalog["models"][0]["base_instructions"],
            "You are a helpful assistant."
        );
        // Core invariant: the placeholder must NOT overwrite an inherited
        // template value — `entry().or_insert_with` never clobbers an
        // existing base_instructions. Pinned against future refactors.
        assert_eq!(
            with.catalog["models"][0]["base_instructions"],
            "You are Codex."
        );
    }

    #[test]
    fn build_catalog_value_matches_generate_catalog() {
        // Use an explicit empty template to avoid auto-discovery of the host
        // Codex template (which would create a mismatch between the two paths).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[providers.ds]\nbase_url=\"http://x\"\napi_key=\"k\"\nformat=\"chat\"\n[routes.\"ds/chat\"]\nmodel=\"m\"\ncontext_window=1000\n",
        )
        .unwrap();
        let template_path = dir.path().join("models.json");
        std::fs::write(&template_path, r#"{"models": []}"#).unwrap();
        let raw = Config::parse_file(&path).unwrap();
        let validated = raw.validate().unwrap();
        let a = build_catalog_value(&validated, None);
        let b = generate_catalog(&path, Some(&template_path)).unwrap();
        assert_eq!(a.catalog, b.catalog);
        assert!(a.dropped_template_fields.is_empty());
    }

    #[test]
    fn parse_bundled_output_extracts_models_array() {
        let out = br#"{"models": [{"slug": "gpt-x", "visibility": "list"}]}"#;
        let models = parse_bundled_output(out);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["slug"], "gpt-x");
    }

    #[test]
    fn parse_bundled_output_degrades_on_garbage() {
        assert!(parse_bundled_output(b"not json").is_empty());
        assert!(parse_bundled_output(br#"{"no_models": []}"#).is_empty());
        assert!(parse_bundled_output(br#"{"models": "not-an-array"}"#).is_empty());
    }

    /// Run a script via [`bundled_from_command`], retrying transient empty
    /// results. On this host kernel, `execve` of a freshly-written script can
    /// transiently fail with `ETXTBSY` ("Text file busy") when other test
    /// threads spawn processes concurrently; the file itself is intact and a
    /// retry succeeds immediately. This is test-harness noise, not a logic
    /// issue — retrying keeps the suite deterministic. A genuinely broken
    /// script still surfaces: the retry window is short and the final
    /// assertion below still fails on a persistent empty result.
    fn bundled_from_command_retry(cmd: &str) -> Vec<Value> {
        for _ in 0..5 {
            let models = bundled_from_command(cmd);
            if !models.is_empty() {
                return models;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        Vec::new()
    }

    #[cfg(unix)]
    #[test]
    fn bundled_from_command_runs_the_binary() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-codex");
        std::fs::write(
            &script,
            "#!/bin/sh\necho '{\"models\": [{\"slug\": \"gpt-x\", \"visibility\": \"list\"}]}'",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let models = bundled_from_command_retry(script.to_str().unwrap());
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["slug"], "gpt-x");
    }

    #[test]
    fn bundled_from_command_missing_binary_is_empty() {
        assert!(bundled_from_command("/nonexistent/codexferry-no-such-bin").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn bundled_from_command_nonzero_exit_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("failing-codex");
        std::fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(bundled_from_command(script.to_str().unwrap()).is_empty());
    }

    #[test]
    fn build_hide_entries_clones_and_flips_only_list_entries() {
        let bundled = vec![
            json!({
                "slug": "gpt-a", "visibility": "list", "priority": 1,
                "base_instructions": "You are Codex."
            }),
            json!({"slug": "gpt-b", "visibility": "hide", "priority": 2}),
            json!({"slug": "gpt-c", "visibility": "none", "priority": 3}),
        ];
        let entries = build_hide_entries(&bundled, &Default::default());
        assert_eq!(entries.len(), 1, "only list-visible entries get overrides");
        assert_eq!(entries[0]["slug"], "gpt-a");
        assert_eq!(entries[0]["visibility"], "hide");
        // Clone-flip must preserve every other field.
        assert_eq!(entries[0]["priority"], 1);
        assert_eq!(entries[0]["base_instructions"], "You are Codex.");
    }

    #[test]
    fn build_hide_entries_skips_route_collisions_and_sorts_by_slug() {
        let bundled = vec![
            json!({"slug": "zeta", "visibility": "list"}),
            json!({"slug": "alpha", "visibility": "list"}),
            json!({"slug": "ds/claim", "visibility": "list"}),
        ];
        let route_keys: std::collections::HashSet<&str> = ["ds/claim"].into_iter().collect();
        let entries = build_hide_entries(&bundled, &route_keys);
        let slugs: Vec<&str> = entries.iter().filter_map(|m| m["slug"].as_str()).collect();
        assert_eq!(
            slugs,
            vec!["alpha", "zeta"],
            "route collision skipped, output sorted by slug"
        );
    }

    #[cfg(unix)]
    #[test]
    fn bundled_from_command_times_out_and_kills() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("hung-codex");
        std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let start = std::time::Instant::now();
        let models = bundled_from_command_with_timeout(
            script.to_str().unwrap(),
            std::time::Duration::from_millis(300),
        );
        assert!(models.is_empty(), "hung discovery must degrade to empty");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "timeout must bound the wait"
        );
    }

    #[cfg(unix)]
    #[test]
    fn bundled_from_command_handles_large_output() {
        // Real `codex debug models --bundled` emits ~315 KB — far beyond the
        // 64 KB pipe buffer. A read-after-exit implementation deadlocks the
        // child on write(2) and hits the timeout; this regression feeds a
        // catalog bigger than the pipe buffer and expects it parsed fully.
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("big-codex");
        let mut models = String::from(r#"{"models":["#);
        for i in 0..4000 {
            if i > 0 {
                models.push(',');
            }
            models.push_str(&format!(r#"{{"slug":"gpt-{i}","visibility":"list"}}"#));
        }
        models.push_str("]}");
        // ~4000 entries x ~40 bytes ≈ 160 KB > 64 KB pipe buffer.
        let body = format!("#!/bin/sh\ncat <<'EOF'\n{models}\nEOF\n");
        std::fs::write(&script, body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let start = std::time::Instant::now();
        // Use the retry seam so a transient ETXTBSY on exec cannot flake the
        // test (a genuinely broken script still yields empty every attempt).
        let parsed = bundled_from_command_retry(script.to_str().unwrap());
        assert_eq!(parsed.len(), 4000, "large catalog must be parsed fully");
        assert_eq!(parsed[3999]["slug"], "gpt-3999");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "large output must not hit the 10s timeout"
        );
    }
}
