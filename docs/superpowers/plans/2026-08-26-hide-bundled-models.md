# Hide Bundled Models (Dynamic Mode) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Serve `visibility: "hide"` override entries on the live `/models` catalog endpoint so Codex's bundled GPT models disappear from the picker in dynamic mode, behind an opt-in `[server] hide_bundled_models` flag.

**Architecture:** Codex's dynamic-mode merge (`apply_remote_models`, codex 0.147) replaces bundled entries by slug with fetched ones, and only `visibility: "list"` entries are picker-visible. So `CatalogCache`'s rebuild path appends, after the route entries, clones of the bundled catalog's list-visible entries (discovered via `codex debug models --bundled`) with `visibility` flipped to `"hide"`. `gen-catalog` and the chat-list shape are untouched.

**Tech Stack:** Rust (axum, serde_json, tracing); no new dependencies.

**Spec:** `docs/superpowers/specs/2026-08-26-hide-bundled-models-design.md` — read it first; this plan argues from it.

## Global Constraints

- No new crate dependencies (spec: shell-out via `std::process::Command`, JSON via `serde_json`).
- Discovery source is `codex debug models --bundled` ONLY — never `load_template`'s file fallback tiers (spec §Decisions 3).
- `gen-catalog` output must never contain hide entries (spec §Decisions 2).
- Any discovery failure degrades to an empty hide list — never a failed or failed-fast `/models` request (spec §Decisions 4).
- Comments in English, `//!` module docs accurate, reference the spec doc path for non-obvious behavior (AGENTS.md #10).
- Unit tests must not depend on the host's real `codex` binary: use the injected discovery fn (`CatalogCache` field) or an explicit command path — never in-process `PATH` mutation (spec §Freshness).
- All test runs from the repo root: `cargo test …`. Commit after every task; work on the current branch `cxf-improvements-1`.
- Tracing strings use `{field}` placeholders, never positional `{}` (AGENTS.md #3).

---

### Task 1: Config flag `[server] hide_bundled_models`

**Files:**
- Modify: `src/config.rs:85-101` (the `ServerConfig` struct + manual `Default` impl)
- Test: `src/config.rs` (new `#[cfg(test)] mod server_config_tests` near the other named test modules, e.g. after `quirks_config_tests`)

**Interfaces:**
- Consumes: nothing new.
- Produces: `ServerConfig.hide_bundled_models: bool` (serde default `false`), carried into `ValidatedConfig` automatically (`ValidatedConfig` already holds `server: ServerConfig`, `src/config.rs:292`). Tasks 4–5 read `config.server.hide_bundled_models`.

- [ ] **Step 1: Write the failing tests**

Append to `src/config.rs` (inside the tests region at the bottom of the file):

```rust
#[cfg(test)]
mod server_config_tests {
    use super::*;

    #[test]
    fn hide_bundled_models_defaults_false() {
        let cfg = Config::parse_str(
            "[providers.ds]\nbase_url=\"http://x\"\napi_key=\"k\"\nformat=\"chat\"\n\
             [routes]\n\"ds/a\" = { model = \"m\", context_window = 1000 }\n",
        )
        .unwrap();
        let validated = cfg.validate().unwrap();
        assert!(!validated.server.hide_bundled_models);
    }

    #[test]
    fn hide_bundled_models_parses_from_server_section() {
        let cfg = Config::parse_str(
            "[server]\nhide_bundled_models = true\n\
             [providers.ds]\nbase_url=\"http://x\"\napi_key=\"k\"\nformat=\"chat\"\n\
             [routes]\n\"ds/a\" = { model = \"m\", context_window = 1000 }\n",
        )
        .unwrap();
        let validated = cfg.validate().unwrap();
        assert!(validated.server.hide_bundled_models);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test server_config_tests`
Expected: FAIL — `no field \`hide_bundled_models\` on type \`ServerConfig\`` (compile error).

- [ ] **Step 3: Implement the flag**

In `src/config.rs`, replace the `ServerConfig` struct, its `Default` impl (keep `default_host`/`default_port` as-is), and update the section doc comment:

```rust
/// `[server]` section: where the proxy listens and how the live model
/// catalog is served.
///
/// Address/port fields are optional; defaults are `127.0.0.1:8787`
/// (localhost only — this is a personal-use tool with no authentication).
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// Bind address. Defaults to `127.0.0.1`.
    #[serde(default = "default_host")]
    pub host: String,
    /// Bind port. Defaults to `8787`.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Emit `visibility: "hide"` overrides for the Codex-bundled models on
    /// the live `/models` catalog endpoint (dynamic-mode wiring only;
    /// `gen-catalog` output is never affected). Defaults to `false`. See
    /// docs/superpowers/specs/2026-08-26-hide-bundled-models-design.md.
    #[serde(default)]
    pub hide_bundled_models: bool,
}
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            hide_bundled_models: false,
        }
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test server_config_tests`
Expected: PASS (2 tests). Also run `cargo test config::` — Expected: PASS (no regressions).

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat(config): add [server].hide_bundled_models flag"
```

---

### Task 2: Bundled-catalog discovery in `catalog.rs`

**Execution note:** As built, `bundled_from_command` delegates to `bundled_from_command_with_timeout` (10s bound, commit `907ae3e`) which drains stdout on a reader thread to avoid a pipe-buffer deadlock with real codex output (commit `348b5df`). Do not revert to a plain `.output()`-based implementation without those two guards.

**Files:**
- Modify: `src/catalog.rs` (new functions after `reload_template`, ~line 454)
- Test: `src/catalog.rs` (append to the existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing new.
- Produces (used by Task 4):
  - `pub(crate) fn discover_bundled_models() -> Vec<Value>` — shells out `codex debug models --bundled`; any failure → empty vec.
  - `fn bundled_from_command(cmd: &str) -> Vec<Value>` (private; test seam).
  - `fn parse_bundled_output(stdout: &[u8]) -> Vec<Value>` (private; test seam).

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` in `src/catalog.rs`:

```rust
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
        let models = bundled_from_command(script.to_str().unwrap());
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["slug"], "gpt-x");
    }

    #[test]
    fn bundled_from_command_missing_binary_is_empty() {
        assert!(bundled_from_command("/nonexistent/codexferry-no-such-bin").is_empty());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test catalog::tests::bundled catalog::tests::parse_bundled`
Expected: FAIL — `cannot find function parse_bundled_output / bundled_from_command` (compile error).

- [ ] **Step 3: Implement the discovery functions**

Insert after `reload_template` in `src/catalog.rs`:

```rust
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
    let Ok(output) = std::process::Command::new(cmd)
        .args(["debug", "models", "--bundled"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_bundled_output(&output.stdout)
}

/// Parse the `{"models": [...]}` stdout of `codex debug models --bundled`.
/// Anything that is not that shape degrades to an empty vec.
fn parse_bundled_output(stdout: &[u8]) -> Vec<Value> {
    serde_json::from_slice::<Value>(stdout)
        .ok()
        .and_then(|v| v.get("models").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test catalog::`
Expected: PASS (existing catalog tests + 4 new).

- [ ] **Step 5: Commit**

```bash
git add src/catalog.rs
git commit -m "feat(catalog): discover bundled models via codex debug models --bundled"
```

---

### Task 3: `build_hide_entries` override builder

**Files:**
- Modify: `src/catalog.rs` (new function next to Task 2's functions)
- Test: `src/catalog.rs` (append to `mod tests`)

**Interfaces:**
- Consumes: `serde_json::Value` (already imported).
- Produces (used by Task 4): `pub(crate) fn build_hide_entries(bundled: &[Value], route_keys: &std::collections::HashSet<&str>) -> Vec<Value>` — clones of bundled entries whose `visibility == "list"` and whose slug is NOT a route key, with `visibility` set to `"hide"`, sorted by slug.

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` in `src/catalog.rs`:

```rust
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
        let route_keys: std::collections::HashSet<&str> =
            ["ds/claim"].into_iter().collect();
        let entries = build_hide_entries(&bundled, &route_keys);
        let slugs: Vec<&str> = entries.iter().filter_map(|m| m["slug"].as_str()).collect();
        assert_eq!(
            slugs,
            vec!["alpha", "zeta"],
            "route collision skipped, output sorted by slug"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test catalog::tests::build_hide_entries`
Expected: FAIL — `cannot find function build_hide_entries` (compile error).

- [ ] **Step 3: Implement the builder**

Insert in `src/catalog.rs` after `build_hide_entries`'s neighbors from Task 2:

```rust
/// Build override entries that hide the Codex-bundled models from the
/// dynamic-mode picker (hide-bundled spec §Mechanism).
///
/// Every bundled entry with `visibility: "list"` is CLONED with visibility
/// flipped to `"hide"`: codex's dynamic merge (`apply_remote_models`)
/// replaces bundled entries by slug, and `show_in_picker` is true only for
/// `"list"`, so the clone suppresses the picker entry while preserving every
/// other field — the clone round-trips codex's own serialization, so all
/// fields required by codex's strict `ModelInfo` deserialization stay
/// present. Entries already hidden need no override, and slugs that collide
/// with a configured route key are skipped — the route must stay
/// selectable. Output is sorted by slug so the response body is
/// byte-reproducible across rebuilds.
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
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test catalog::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/catalog.rs
git commit -m "feat(catalog): build_hide_entries clone-flip overrides"
```

---

### Task 4: `CatalogCache` wiring — flag gate, fingerprint, re-probe

**Files:**
- Modify: `src/models_cache.rs` (`CatalogCache` struct + `new()` at lines 55–58, `Cached` struct at line 33, fast-path `recheck_due` at lines 84–86, slow path at lines 101–118, `fingerprint_config` at line 133)
- Test: `src/models_cache.rs` (extend `mod tests`)

**Interfaces:**
- Consumes: `ServerConfig.hide_bundled_models` (Task 1), `crate::catalog::discover_bundled_models` (Task 2), `crate::catalog::build_hide_entries` (Task 3).
- Produces: no new public API — `/models` behavior changes internally (hide entries appended to the cached catalog body when the flag is on).

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` in `src/models_cache.rs` (add `use serde_json::Value;` to the test-module imports alongside the existing `use` lines):

```rust
    fn config_with_flag(key: &str, hide: bool) -> ValidatedConfig {
        let toml = format!(
            r#"
[server]
hide_bundled_models = {hide}
[providers.ds]
base_url = "http://x"
api_key = "k"
format = "chat"
[routes."{key}"]
model = "m"
context_window = 1000
"#
        );
        parse_config_toml(&toml)
    }

    fn marker_discovery() -> Vec<Value> {
        vec![serde_json::json!({"slug": "gpt-marker", "visibility": "list"})]
    }

    #[tokio::test]
    async fn hide_flag_off_serves_no_hide_entries_and_never_discovers() {
        static CALLS: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);
        fn counting() -> Vec<Value> {
            CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            marker_discovery()
        }
        let cfg = Arc::new(RwLock::new(config_with_flag("ds/a", false)));
        let cache = CatalogCache {
            inner: Mutex::new(None),
            discovery: counting,
        };
        let (_, body) = cache.get(&cfg.read().await);
        assert!(
            !body.windows(11).any(|w| w == b"gpt-marker"),
            "flag off must not append hide entries"
        );
        assert_eq!(
            CALLS.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "discovery must not run when the flag is off"
        );
    }

    #[tokio::test]
    async fn hide_flag_on_appends_hide_entries() {
        let cfg = Arc::new(RwLock::new(config_with_flag("ds/a", true)));
        let cache = CatalogCache {
            inner: Mutex::new(None),
            discovery: marker_discovery,
        };
        let (etag, body) = cache.get(&cfg.read().await);
        let v: Value = serde_json::from_slice(&body).unwrap();
        let models = v["models"].as_array().expect("models array");
        let marker = models
            .iter()
            .find(|m| m["slug"] == "gpt-marker")
            .expect("hide entry appended");
        assert_eq!(marker["visibility"], "hide");
        assert!(
            models.iter().any(|m| m["slug"] == "ds/a" && m["visibility"] == "list"),
            "route entries stay list-visible"
        );
        assert!(!etag.is_empty());
    }

    #[tokio::test]
    async fn hide_flag_toggle_invalidates_cache() {
        let cfg = Arc::new(RwLock::new(config_with_flag("ds/a", false)));
        let cache = CatalogCache {
            inner: Mutex::new(None),
            discovery: marker_discovery,
        };
        let (etag1, body1) = cache.get(&cfg.read().await);
        *cfg.write().await = config_with_flag("ds/a", true);
        let (etag2, body2) = cache.get(&cfg.read().await);
        assert_ne!(etag1, etag2, "flag toggle must invalidate the cache");
        assert!(!body1.windows(11).any(|w| w == b"gpt-marker"));
        assert!(body2.windows(11).any(|w| w == b"gpt-marker"));
    }

    #[tokio::test]
    async fn hide_with_file_template_reprobes_after_interval() {
        // Hide entries come from a shell-out source with no mtime, so the
        // 60s re-probe must fire even when the template is file-backed
        // (spec §Freshness).
        static CALLS: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);
        fn counting() -> Vec<Value> {
            CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            marker_discovery()
        }
        let cfg = Arc::new(RwLock::new(config_with_flag("ds/a", true)));
        let cache = CatalogCache {
            inner: Mutex::new(None),
            discovery: counting,
        };
        let _ = cache.get(&cfg.read().await);
        {
            let mut inner = cache.inner.lock().unwrap();
            let cached = inner.as_mut().unwrap();
            // Simulate a file-backed template + an aged entry.
            cached.template_path = Some(PathBuf::from("/nonexistent-template.json"));
            cached.checked_at = std::time::Instant::now()
                - (TEMPLATE_RECHECK_INTERVAL + Duration::from_secs(1));
        }
        let _ = cache.get(&cfg.read().await);
        assert_eq!(
            CALLS.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "aged entry with hide on must rebuild despite a file-backed template"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test models_cache::`
Expected: FAIL — `no field \`discovery\` on type \`CatalogCache\`` (compile error).

- [ ] **Step 3: Implement the wiring**

In `src/models_cache.rs`:

1. Add the `hide_bundled` field to `Cached` (after `checked_at`):

```rust
    /// Whether hide entries were appended at build time — drives the
    /// periodic re-probe even when the template is file-backed (the
    /// bundled catalog comes from a shell-out with no mtime).
    hide_bundled: bool,
```

2. Add the `discovery` field to `CatalogCache` and default it in `new()`:

```rust
pub struct CatalogCache {
    inner: Mutex<Option<Cached>>,
    /// Bundled-catalog discovery, injectable so unit tests never depend on
    /// the host's real codex binary (hide-bundled spec §Freshness).
    discovery: fn() -> Vec<serde_json::Value>,
}
```

```rust
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
            discovery: crate::catalog::discover_bundled_models,
        }
    }
```

3. Replace the fast-path `recheck_due` (lines 84–86) with:

```rust
                let recheck_due = cached.checked_at.elapsed()
                    >= TEMPLATE_RECHECK_INTERVAL
                    && (cached.template_path.is_none() || cached.hide_bundled);
```

4. Replace the slow-path body between `reload_template()` and the `Cached` store (lines 103–117) with:

```rust
        let (template_path, template) = crate::catalog::reload_template();
        let template_mtime = template_path.as_ref().and_then(file_mtime);
        let mut generated = crate::catalog::build_catalog_value(config, template.as_ref());
        // Hide overrides: dynamic-mode only, opt-in. gen-catalog never runs
        // this code (spec §Decisions 1-2).
        let hide_bundled = config.server.hide_bundled_models;
        if hide_bundled {
            let bundled = (self.discovery)();
            if bundled.is_empty() {
                tracing::warn!(
                    "hide_bundled_models is on but `codex debug models --bundled` \
                     returned no models; Codex's bundled models stay visible"
                );
            } else {
                let route_keys: std::collections::HashSet<&str> =
                    config.routes.keys().map(String::as_str).collect();
                let entries = crate::catalog::build_hide_entries(&bundled, &route_keys);
                if let Some(models) = generated.catalog["models"].as_array_mut() {
                    models.extend(entries);
                }
            }
        }
        let body = serde_json::to_vec(&generated.catalog)
            .map(Bytes::from)
            .unwrap_or_default();
        let etag = etag_for(fingerprint, &body);
        let cached = Cached {
            body: body.clone(),
            etag: etag.clone(),
            fingerprint,
            template_path,
            template_mtime,
            checked_at: Instant::now(),
            hide_bundled,
        };
```

5. Hash the flag into the fingerprint — add as the first write inside `fingerprint_config` (before the routes loop):

```rust
    // The hide flag changes the served body; hashing it (re)builds the
    // cache when a hot-reload toggles it (spec §Freshness).
    h.write_u8(u8::from(config.server.hide_bundled_models));
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test models_cache::`
Expected: PASS — 4 new tests plus the 3 existing ones (`stable_etag_and_body_when_nothing_changes`, `route_change_invalidates`, `non_file_template_is_rechecked_after_interval`).

- [ ] **Step 5: Commit**

```bash
git add src/models_cache.rs
git commit -m "feat(models-cache): serve hide overrides behind flag + shell-out re-probe"
```

---

### Task 5: End-to-end integration test (fake codex + hot-reload toggle)

**Files:**
- Modify: `tests/endpoints_metrics.rs` (new test after `models_endpoint_reflects_hot_reload`)
- Consumes from `tests/common/mod.rs` (already re-exported via `pub use common::*`): `free_port()`, `RouterGuard { child, stderr_path, _dir }`, `wait_for_healthz`.

**Interfaces:**
- Consumes: Tasks 1–4 behavior via the real binary (`CARGO_BIN_EXE_codexferry`).
- Produces: nothing (pure verification).

- [ ] **Step 1: Write the test**

Append to `tests/endpoints_metrics.rs`:

```rust
#[tokio::test]
/// hide_bundled_models (dynamic mode): with the flag on, the catalog-shape
/// /models response must carry `visibility: "hide"` overrides cloned from
/// the (faked) bundled catalog so codex's slug merge hides them; with the
/// flag off they must be absent. The chat-list shape must never contain
/// them. Toggling the flag via config hot-reload must rebuild the catalog.
async fn models_catalog_hides_bundled_models_when_enabled() {
    // Fake `codex` binary: `codex debug models --bundled` prints one
    // list-visible and one already-hidden model. Prepending its directory
    // to the child's PATH shadows any real codex install.
    let bin_dir = tempfile::tempdir().expect("bin tempdir");
    let fake_codex = bin_dir.path().join("codex");
    std::fs::write(
        &fake_codex,
        "#!/bin/sh\necho '{\"models\":[{\"slug\":\"gpt-fake-sol\",\"visibility\":\"list\",\"priority\":1},{\"slug\":\"gpt-fake-old\",\"visibility\":\"hide\",\"priority\":2}]}'",
    )
    .expect("write fake codex");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_codex, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake codex");
    }
    let child_path = format!(
        "{}:{}",
        bin_dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let base_config = |port: u16, hide: bool| {
        format!(
            r#"
[server]
host = "127.0.0.1"
port = {port}
hide_bundled_models = {hide}

[providers.ds]
base_url = "http://x"
api_key = "k"
format = "chat"

[routes]
"ds/a" = {{ model = "m", context_window = 1000 }}
"#
        )
    };

    // TOCTOU retry, mirroring models_endpoint_reflects_hot_reload.
    let mut started = None;
    for attempt in 0..2 {
        let port = free_port();
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, base_config(port, false)).expect("write config");
        let stderr_path = dir.path().join("router.stderr.log");
        let stderr = std::fs::File::create(&stderr_path).expect("create stderr log");
        let bin = env!("CARGO_BIN_EXE_codexferry");
        let child = std::process::Command::new(bin)
            .env("CODEXFERRY_CONFIG", &config_path)
            .env("PATH", &child_path)
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .expect("spawn codexferry");
        let guard = RouterGuard {
            child,
            stderr_path,
            _dir: dir,
        };
        let router_url = format!("http://127.0.0.1:{port}");
        match wait_for_healthz(&router_url, &guard).await {
            Ok(()) => {
                started = Some((guard, router_url, config_path, port));
                break;
            }
            Err(log) => {
                drop(guard);
                if attempt == 1 {
                    panic!("router did not become healthy within 10s; stderr:\n{log}");
                }
            }
        }
    }
    let (_guard, router_url, config_path, port) =
        started.expect("retry loop always returns or panics");

    let client = reqwest::Client::new();

    // 1. Flag off: no hide entries in the catalog shape.
    let resp = client
        .get(format!("{router_url}/v1/models?client_version=0.0.0"))
        .send()
        .await
        .expect("models get (flag off)");
    let body = resp.text().await.unwrap();
    assert!(
        !body.contains("gpt-fake-sol"),
        "flag off must not append hide entries:\n{body}"
    );

    // 2. Chat-list shape (no client_version): routes only, before AND after
    //    the flag is turned on (spec §Decisions 2).
    let resp = client
        .get(format!("{router_url}/v1/models"))
        .send()
        .await
        .expect("chat-list models get");
    let list: Value = resp.json().await.unwrap();
    let ids: Vec<&str> = list["data"]
        .as_array()
        .expect("data array")
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["ds/a"]);

    // 3. Toggle the flag via hot-reload and poll until the hide entry
    //    appears with visibility "hide".
    std::fs::write(&config_path, base_config(port, true))
        .expect("rewrite config with hide on");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let resp = client
            .get(format!("{router_url}/v1/models?client_version=0.0.0"))
            .send()
            .await
            .expect("poll models get");
        let body = resp.text().await.unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
        let models = v["models"].as_array().expect("models array");
        let sol = models.iter().find(|m| m["slug"] == "gpt-fake-sol");
        let done = match sol {
            Some(m) => {
                assert_eq!(m["visibility"], "hide", "override must hide:\n{body}");
                true
            }
            None => false,
        };
        if done {
            assert!(
                !models.iter().any(|m| m["slug"] == "gpt-fake-old"),
                "already-hidden bundled entries need no override:\n{body}"
            );
            assert!(
                models.iter().any(|m| m["slug"] == "ds/a" && m["visibility"] == "list"),
                "route stays list-visible:\n{body}"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "hide entry did not appear within 5s of hot-reload; last body:\n{body}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
```

- [ ] **Step 2: Run the test to verify it passes**

Run: `cargo test --test endpoints_metrics models_catalog_hides_bundled_models`
Expected: PASS. (It exercises already-implemented Tasks 1–4 through the real binary, so there is no red phase — its red phase was Task 4's.)

- [ ] **Step 3: Run the whole integration suite for regressions**

Run: `cargo test --test endpoints_metrics`
Expected: PASS (all tests).

- [ ] **Step 4: Commit**

```bash
git add tests/endpoints_metrics.rs
git commit -m "test(endpoints): e2e hide overrides via fake codex + hot-reload toggle"
```

---

### Task 6: Docs sync + full verification

**Files:**
- Modify: `README.md` (section `## Two config modes`, line ~80)
- Modify: `ARCHITECTURE.md` (§11 per-file line counts table; module notes for `catalog.rs` / `models_cache.rs` if they describe responsibilities)
- Modify: `AGENTS.md` (convention §12 `/models` note)

**Interfaces:**
- Consumes: final code from Tasks 1–5.
- Produces: documentation only.

- [ ] **Step 1: README — extend `## Two config modes`**

Add at the end of that section (verbatim):

````markdown
### Hiding Codex's bundled GPT models (dynamic mode)

In dynamic mode Codex merges its own bundled model catalog (the GPT family
compiled into the codex binary) underneath the models this proxy serves, so
those entries show up in the picker. Setting

```toml
[server]
hide_bundled_models = true
```

makes the live `/models` catalog additionally return `visibility: "hide"`
copies of every bundled model (discovered via `codex debug models
--bundled`), which suppresses them from the picker while your routes stay
selectable. If the `codex` binary is not on the proxy's `PATH`, hiding is
silently disabled and the bundled models reappear — check the daemon log
for the warning. `gen-catalog` output is never affected.
````

- [ ] **Step 2: AGENTS.md — extend convention §12**

Append one sentence to the end of the `### 12. /models is dual-shape` bullet list:

```markdown
When `[server] hide_bundled_models = true`, the catalog-shape branch also
appends `visibility: "hide"` overrides cloned from `codex debug models
--bundled` so Codex's dynamic-mode slug merge hides its bundled GPT models;
see `docs/superpowers/specs/2026-08-26-hide-bundled-models-design.md`.
```

- [ ] **Step 3: ARCHITECTURE.md — refresh §11 line counts**

Run: `wc -l src/config.rs src/catalog.rs src/models_cache.rs`
Update the §11 table rows for those three files with the new numbers. If the module-responsibility table or prose describes `models_cache.rs` invalidation, extend that sentence with: "and, when `hide_bundled_models` is on, re-probes `codex debug models --bundled` on the same 60s cadence."

- [ ] **Step 4: Full verification**

Run: `cargo fmt --check` then `cargo test`
Expected: fmt clean (run `cargo fmt` first if not); the entire test suite passes, including `cargo test upstream` (SSE parser untouched, but the convention asks for it when touching parsing-adjacent code — here it is a cheap sanity run).

- [ ] **Step 5: Commit**

```bash
git add README.md ARCHITECTURE.md AGENTS.md
git commit -m "docs: hide_bundled_models feature docs + line counts"
```

---

### Task 7: E2E scenario — real Codex CLI merge probe

**Files:**
- Modify: `scripts/e2e-lib.sh` (extend `write_router_config` with an optional 4th arg; add `run_codex_debug_models` after `run_codex_resume_fallback`)
- Modify: `scripts/e2e.sh` (new `scenario_hide_bundled`; usage line, case dispatch, `all` chain)

**Interfaces:**
- Consumes: Tasks 1–4 via the real `target/debug/codexferry` binary, plus the real `codex` CLI on PATH (verified against 0.147; the scenario is a manual tool like the rest of the e2e suite — never part of `cargo test`).
- Produces: `scripts/e2e.sh hide_bundled` scenario; `write_router_config` gains an optional 4th arg (backward compatible — existing callers pass 3 args).

**Observation point (why this works):** plain `codex debug models` (no `--bundled`) builds the real config, constructs the dynamic-mode models manager, and prints the MERGED catalog produced by `apply_remote_models` — the exact overlay merge the feature depends on (`codex-rs/cli/src/main.rs:2150-2177`). Assertions are version-independent: no gpt slug is hardcoded; the contract under test is "picker-visible ⊆ router routes".

- [ ] **Step 1: Extend `e2e-lib.sh`**

1. Replace `write_router_config` (lines 31–61) — add the optional extra `[server]` line; existing 3-arg callers are unaffected (`${4:-}` renders an empty TOML line):

```bash
# Temp router config: three routes over one mock instance. mocka/mockb are
# distinct providers so a mid-session switch stays cross-provider. $4 is an
# optional extra [server] line (e.g. "hide_bundled_models = true" for the
# hide_bundled scenario); empty renders a harmless blank line.
write_router_config() { # $1 path, $2 router port, $3 mock port, $4 optional extra [server] line
  cat > "$1" <<EOF
[server]
host = "127.0.0.1"
port = $2
${4:-}

[providers.mocka]
base_url = "http://127.0.0.1:$3/v1"
api_key = "test-key"
format = "chat"
timeout_ms = 30000

[providers.mockb]
base_url = "http://127.0.0.1:$3/v1"
api_key = "test-key"
format = "chat"
timeout_ms = 30000

[providers.mockr]
base_url = "http://127.0.0.1:$3/v1"
api_key = "test-key"
format = "responses"
timeout_ms = 30000

[routes]
"mocka/chat" = { model = "e2e-model" }
"mockb/chat" = { model = "e2e-model" }
"mockr/resp" = { model = "e2e-model" }
EOF
}
```

2. Add `run_codex_debug_models` after `run_codex_resume_fallback` (same dynamic wiring as `run_codex` — command auth, no pin — so the fetch+merge path actually runs):

```bash
# `codex debug models` under the canonical dynamic wiring (auth command, no
# pin): prints the MERGED catalog codex would use — bundled base + the
# router's /v1/models overlay applied by apply_remote_models. This is the
# observation point for hide_bundled_models: the merged JSON shows whether
# the router's visibility:"hide" overrides actually suppressed the bundled
# models INSIDE codex (the cargo integration test can only see the wire).
# No sandbox flags: no session is started. Stderr goes to a log so the
# caller can capture stdout as the catalog JSON.
run_codex_debug_models() {
  mkdir -p "$ARTIFACT_DIR/codex-home"
  CODEX_HOME="$ARTIFACT_DIR/codex-home" codex debug models \
    -c 'model_provider="e2e"' \
    -c 'model_providers.e2e.name="e2e"' \
    -c "model_providers.e2e.base_url=\"http://127.0.0.1:${ROUTER_PORT}/v1\"" \
    -c 'model_providers.e2e.wire_api="responses"' \
    -c 'model_providers.e2e.auth={command="echo",args=["dummy"]}' \
    2>"$ARTIFACT_DIR/codex-debug-models-$(date +%s%N).log"
}
```

- [ ] **Step 2: Add the scenario to `e2e.sh`**

1. New scenario function (place after `scenario_models`):

```bash
scenario_hide_bundled() {
  log "scenario: hide_bundled_models (codex-side merge probe)"
  start_mock basic "$ARTIFACT_DIR/record-hide.jsonl"   # /models is router-served; mock unused

  # Phase A (control, flag off): the merged catalog codex builds must still
  # show >=1 bundled model as picker-visible - proves the fetch+merge
  # happened, so phase B's hiding is the flag's doing, not a broken fetch.
  # start_router clears $CODEX_HOME/models_cache.json (300s TTL) per start,
  # so the two phases cannot read each other's cache.
  write_router_config "$ARTIFACT_DIR/config-hide-off.toml" "$(free_port)" "$MOCK_PORT"
  start_router "$ARTIFACT_DIR/config-hide-off.toml"
  run_codex_debug_models > "$ARTIFACT_DIR/merged-hide-off.json"
  python3 - "$ARTIFACT_DIR/merged-hide-off.json" <<'PY' || fail "control (flag off): no bundled model picker-visible - fetch/merge broken?"
import json, sys
models = json.load(open(sys.argv[1])).get("models", [])
routes = {"mocka/chat", "mockb/chat", "mockr/resp"}
visible = [m.get("slug") for m in models if m.get("visibility") == "list"]
nonroute = [s for s in visible if s not in routes]
assert nonroute, f"expected >=1 bundled list-visible model, got {visible}"
PY
  cleanup_procs

  # Phase B (flag on): every picker-visible model must be a router route,
  # and at least one bundled model must appear hidden - the overlay merge
  # applied the router's visibility:"hide" overrides inside codex itself.
  # The hidden-model assertion also trips if a future codex stops merging
  # (remote-only would leave no non-route models at all).
  write_router_config "$ARTIFACT_DIR/config-hide-on.toml" "$(free_port)" "$MOCK_PORT" "hide_bundled_models = true"
  start_router "$ARTIFACT_DIR/config-hide-on.toml"
  run_codex_debug_models > "$ARTIFACT_DIR/merged-hide-on.json"
  python3 - "$ARTIFACT_DIR/merged-hide-on.json" mocka/chat mockb/chat mockr/resp <<'PY' || fail "hide (flag on): assertions above failed"
import json, sys
models = json.load(open(sys.argv[1])).get("models", [])
routes = set(sys.argv[2:])
visible = [m.get("slug") for m in models if m.get("visibility") == "list"]
nonroute = [s for s in visible if s not in routes]
assert not nonroute, f"bundled models still picker-visible: {nonroute}"
missing = [r for r in routes if r not in visible]
assert not missing, f"routes missing or not list-visible: {missing}"
hidden = [m.get("slug") for m in models
          if m.get("visibility") != "list" and m.get("slug") not in routes]
assert hidden, "no hidden bundled model in merged catalog - overrides never merged"
PY
  cleanup_procs
  pass "hide_bundled"
}
```

2. Usage line — add `hide_bundled` to the scenario list:

```bash
# Usage: scripts/e2e.sh [basic|models|static|tools|multiturn|cross_format_switch|stale_catalog|doctor_dynamic|doctor_pinned|doctor_fallback|hide_bundled|all]   (default: all)
```

3. Case dispatch — add before the `all)` arm:

```bash
  hide_bundled) scenario_hide_bundled ;;
```

4. `all)` chain — append `scenario_hide_bundled;` after `scenario_doctor_fallback;`.

- [ ] **Step 3: Build and run the new scenario**

Run: `cargo build --bins && scripts/e2e.sh hide_bundled`
Expected: `[PASS] hide_bundled`. (There is no red phase — like Task 5, this verifies already-implemented behavior; it requires the real codex CLI on PATH.)

- [ ] **Step 4: Regression-run the models scenario**

`write_router_config` was touched, so re-run its consumer:
Run: `scripts/e2e.sh models`
Expected: `[PASS] models`.

- [ ] **Step 5: Commit**

```bash
git add scripts/e2e.sh scripts/e2e-lib.sh
git commit -m "test(e2e): hide_bundled_models scenario via codex debug models merge probe"
```
