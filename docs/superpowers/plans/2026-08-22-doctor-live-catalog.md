# doctor Live-Catalog Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove doctor's dependency on the dead static-catalog file, verify the live `/models` catalog contract with a split live probe, and add a codex-version tripwire to the daemon.

**Architecture:** Three layers per the spec: (1) `doctor` defaults to live probes — probe 1 (responses route) omits `model_catalog_json` so codex must resolve the route via the live `/models` endpoint, probe 2 (chat route) keeps the pinned temp catalog; (2) the `/models` handler observes the `client_version` value through a `CodexVersionTracker` in `AppState`, logging/metricing once per first-seen version and warning while the version is not green in a doctor state file; (3) offline checks shrink to catalog-free quick-checks. A new module `src/version.rs` owns version normalization, the tracker, and the state file.

**Tech Stack:** Rust (axum 0.8, tokio, prometheus-client 0.25, toml 0.8, clap 4). **No new dependencies.**

**Spec:** `docs/superpowers/specs/2026-08-22-doctor-live-catalog-design.md` — the plan argues from the spec; read §2 (probe split), §3 (tripwire + state file), §4 (quick-checks), §5 (remediation), §6 (quirk seam), §7 (terminology) before starting.

## Global Constraints

- No new crates: `toml` 0.8, `tempfile`, `uuid`, `prometheus-client` 0.25 are already dependencies.
- Exit codes: 0 all pass, 1 any FAIL, 2 environment unusable (`doctor --live` only). Never introduce another exit code.
- AGENTS.md #11 (exactly one per-request log line) — the version-change `info!`/`warn!` are once-per-version **event** logs, not per-request lines; AGENTS.md must be updated to document the exception (Task 12).
- Tracing format strings use `{field}` placeholders capturing locals (AGENTS.md #3); for `Option` values assign a plain local first.
- Code comments in English, `//!` module docs, `///` on public items (AGENTS.md #10).
- Terminology (migration spec Layer 3): neutral "router" vocabulary stays; user-visible product-facing strings say "codexferry".
- State file path: `$XDG_STATE_HOME/codexferry/doctor.json`, falling back to `~/.local/state/codexferry/doctor.json` when `XDG_STATE_HOME` is unset. NEVER under `~/.codex/`.
- All unit tests must pass on machines WITHOUT codex installed and WITHOUT `~/.codex/` — environment-dependent logic takes injected inputs (text/paths), spawning `codex` happens only in production paths.
- Conventional Commits (`feat:`, `fix:`, `test:`, `docs:`).
- Run `cargo test` (full suite) at minimum before each commit; targeted `cargo test <filter>` inside TDD loops.

---

### Task 1: `src/version.rs` — `normalize_version`

**Files:**
- Create: `src/version.rs`
- Modify: `src/main.rs:33` (module list) — add `mod version;` after `mod upstream;` (alphabetical: `mod upstream;` then `mod version;` then `mod wire;`)

**Interfaces:**
- Produces: `pub fn normalize_version(s: &str) -> Option<String>` — trims, returns the LAST whitespace-separated token containing an ASCII digit, `None` when no token qualifies. Later tasks (5, 6, 10) compare `codex --version` output with the `client_version` parameter through this function.

- [ ] **Step 1: Create the module with a failing test**

Create `src/version.rs` with ONLY the module doc and the test module (the function does not exist yet, so the test cannot compile — that is the failing state):

```rust
//! Codex client-version observation and doctor state.
//!
//! Owns three small concerns shared by the daemon and the `doctor`
//! subcommand (spec 2026-08-22 §3):
//! - [`normalize_version`]: make `codex --version` output and the
//!   `client_version` query parameter comparable;
//! - [`CodexVersionTracker`]: per-process first-sighting detection;
//! - [`DoctorState`]: the `last_green` state file under XDG state.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_extracts_last_digit_token() {
        assert_eq!(
            normalize_version("codex-cli 0.158.0"),
            Some("0.158.0".to_string())
        );
        assert_eq!(normalize_version("0.158.0"), Some("0.158.0".to_string()));
        // The LAST token carrying a digit wins, per spec §3.2.
        assert_eq!(normalize_version("codex 1.2 beta 3"), Some("3".to_string()));
    }

    #[test]
    fn normalize_returns_none_without_digits() {
        assert_eq!(normalize_version(""), None);
        assert_eq!(normalize_version("no digits here"), None);
        assert_eq!(normalize_version("   "), None);
    }
}
```

And in `src/main.rs` add `mod version;` to the module list (keep alphabetical order).

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test version`
Expected: compile error (`cannot find function normalize_version`) — this is the failing state.

- [ ] **Step 3: Write the minimal implementation**

Add above the tests module:

```rust
/// Make version-ish strings from different sources comparable.
///
/// `codex --version` prints e.g. `codex-cli 0.158.0` while the
/// `client_version` query parameter is usually already bare (`0.158.0`).
/// Rule (spec §3.2): the LAST whitespace-separated token containing an
/// ASCII digit; versions are otherwise opaque — no semver parsing.
pub fn normalize_version(s: &str) -> Option<String> {
    s.split_whitespace()
        .rev()
        .find(|t| t.bytes().any(|b| b.is_ascii_digit()))
        .map(str::to_string)
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test version`
Expected: 2 passed.

- [ ] **Step 5: Commit**

```bash
git add src/version.rs src/main.rs
git commit -m "feat: add normalize_version for codex client-version comparison (spec §3.2)"
```

---

### Task 2: `src/version.rs` — `CodexVersionTracker`

**Files:**
- Modify: `src/version.rs`

**Interfaces:**
- Produces: `pub struct Transition { pub from: Option<String>, pub to: String }` and `pub struct CodexVersionTracker` with `pub fn new() -> Self` and `pub fn observe(&self, version: &str) -> Option<Transition>`. Semantics: `current` updates on EVERY call; `Some(Transition)` returns only when the version is newly added to `seen` (first sighting this process); `from` is the previous `current` (`None` at process start). Task 5 calls this from `handle_models`.

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` in `src/version.rs`:

```rust
    use super::super::CodexVersionTracker;

    #[test]
    fn first_sighting_returns_transition_from_none() {
        let t = CodexVersionTracker::new();
        let tr = t.observe("0.1.0").expect("first sighting fires");
        assert_eq!(tr.from, None);
        assert_eq!(tr.to, "0.1.0");
    }

    #[test]
    fn repeat_sighting_is_silent() {
        let t = CodexVersionTracker::new();
        t.observe("0.1.0");
        assert!(t.observe("0.1.0").is_none());
        // Flapping back and forth: each DISTINCT version logs once only.
        t.observe("0.2.0");
        assert!(t.observe("0.1.0").is_none());
        assert!(t.observe("0.2.0").is_none());
    }

    #[test]
    fn second_version_transitions_from_previous_current() {
        let t = CodexVersionTracker::new();
        t.observe("0.1.0");
        let tr = t.observe("0.2.0").expect("second distinct version fires");
        assert_eq!(tr.from.as_deref(), Some("0.1.0"));
        assert_eq!(tr.to, "0.2.0");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test version`
Expected: compile error (`cannot find` `CodexVersionTracker` / `Transition`).

- [ ] **Step 3: Write the implementation**

Add to `src/version.rs` (below `normalize_version`, above tests):

```rust
use std::collections::HashSet;
use std::sync::Mutex;

/// A first-sighting event: `from` is the previous `current` (`None` at
/// process start), `to` the newly seen version.
pub struct Transition {
    pub from: Option<String>,
    pub to: String,
}

/// Per-process codex client-version tracker (spec §3).
///
/// Logging/metrics fire ONCE per distinct version per process (the `seen`
/// set), so two codex versions alternating requests cannot spam the log.
/// `current` tracks the most recent version so a transition can name both
/// sides. All methods are sync and lock-only — safe from the async
/// handlers.
#[derive(Default)]
pub struct CodexVersionTracker {
    seen: Mutex<HashSet<String>>,
    current: Mutex<Option<String>>,
}

impl CodexVersionTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Update `current` on every call; return `Some(Transition)` only when
    /// `version` is newly added to `seen` (its first sighting this process).
    pub fn observe(&self, version: &str) -> Option<Transition> {
        let mut current = self.current.lock().unwrap();
        let from = current.clone();
        *current = Some(version.to_string());
        if self.seen.lock().unwrap().insert(version.to_string()) {
            Some(Transition {
                from,
                to: version.to_string(),
            })
        } else {
            None
        }
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test version`
Expected: 5 passed (2 from Task 1 + 3 new).

- [ ] **Step 5: Commit**

```bash
git add src/version.rs
git commit -m "feat: add CodexVersionTracker first-sighting detection (spec §3)"
```

---

### Task 3: `src/version.rs` — `DoctorState` (state file)

**Files:**
- Modify: `src/version.rs`

**Interfaces:**
- Produces:
  - `pub fn state_path() -> PathBuf` — `$XDG_STATE_HOME/codexferry/doctor.json`, fallback `~/.local/state/codexferry/doctor.json`.
  - `#[derive(Serialize, Deserialize, Default, Clone)] pub struct DoctorState { pub last_green: Option<String>, pub last_run: Option<LastRun> }`
  - `#[derive(Serialize, Deserialize, Clone)] pub struct LastRun { pub green: bool, pub codex_version: Option<String>, pub at_unix: u64, pub summary: String }`
  - `impl DoctorState { pub fn read_from(path: &Path) -> DoctorState; pub fn write_to(&self, path: &Path) -> std::io::Result<()>; pub fn read() -> DoctorState; pub fn write(&self) -> std::io::Result<()> }`
  - `read*` never fail: missing/malformed file → `DoctorState::default()` (spec §3.1 "treated as never-green"). `write*` create parent dirs and write atomically (temp file + rename).
  - Spec note: the spec's `"at"` field is implemented as `at_unix` (epoch seconds) because the repo has no chrono dependency.
- Task 5 consumes `DoctorState::read()`; Task 6 consumes `DoctorState::read()`; Task 10 consumes `read_from`/`write_to` for tests and `write()` for production.

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests`:

```rust
    use super::super::{state_path, DoctorState, LastRun};

    #[test]
    fn read_missing_or_malformed_defaults_to_never_green() {
        let dir = tempfile::tempdir().unwrap();
        // Missing file.
        assert_eq!(DoctorState::read_from(&dir.path().join("nope.json")).last_green, None);
        // Malformed file.
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "not json").unwrap();
        assert_eq!(DoctorState::read_from(&bad).last_green, None);
    }

    #[test]
    fn write_then_read_round_trips_and_creates_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/dir/doctor.json");
        let state = DoctorState {
            last_green: Some("0.158.0".into()),
            last_run: Some(LastRun {
                green: true,
                codex_version: Some("codex-cli 0.158.0".into()),
                at_unix: 12345,
                summary: "all checks passed".into(),
            }),
        };
        state.write_to(&path).unwrap();
        let back = DoctorState::read_from(&path);
        assert_eq!(back.last_green.as_deref(), Some("0.158.0"));
        assert!(back.last_run.unwrap().green);
    }

    #[test]
    fn state_path_is_never_under_codex_home() {
        let p = state_path().to_string_lossy().to_string();
        assert!(!p.contains(".codex/"), "state must live on codexferry's side: {p}");
        assert!(p.ends_with("codexferry/doctor.json"), "unexpected path: {p}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test version`
Expected: compile error (`cannot find` `DoctorState` / `state_path`).

- [ ] **Step 3: Write the implementation**

Add to `src/version.rs`:

```rust
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Result of the most recent doctor run (spec §3.1). Timestamped as epoch
/// seconds — the repo deliberately has no chrono dependency.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LastRun {
    pub green: bool,
    pub codex_version: Option<String>,
    pub at_unix: u64,
    pub summary: String,
}

/// Doctor's persisted state (spec §3.1). Doctor is the ONLY writer; the
/// daemon only reads. A missing or malformed file means "never green".
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct DoctorState {
    pub last_green: Option<String>,
    pub last_run: Option<LastRun>,
}

/// Where the state file lives: `$XDG_STATE_HOME/codexferry/doctor.json`,
/// defaulting to `~/.local/state/codexferry/doctor.json` (spec §3.1 —
/// daemon-owned state stays OUT of `~/.codex/`).
pub fn state_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_default();
            PathBuf::from(home).join(".local/state")
        });
    base.join("codexferry").join("doctor.json")
}

impl DoctorState {
    /// Read from `path`; missing/malformed → default (never green).
    pub fn read_from(path: &Path) -> DoctorState {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Write atomically (tmp + rename), creating parent dirs. Write
    /// failures are the caller's to log — they never change a verdict.
    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, path)
    }

    /// Read the production state file (spec §3.1 path rule).
    pub fn read() -> DoctorState {
        Self::read_from(&state_path())
    }

    /// Write the production state file.
    pub fn write(&self) -> std::io::Result<()> {
        self.write_to(&state_path())
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test version`
Expected: 8 passed.

- [ ] **Step 5: Commit**

```bash
git add src/version.rs
git commit -m "feat: add DoctorState last_green file under XDG state (spec §3.1)"
```

---

### Task 4: `src/metrics.rs` — codex client metrics

**Files:**
- Modify: `src/metrics.rs` (labels near `GaugeLabels` ~line 114-119; `Metrics` struct fields ~line 126-134; `Metrics::new` ~line 136-191; a recording method near `record_tokens`)

**Interfaces:**
- Produces: `pub(crate) fn record_codex_client(&self, version: &str)` on `Metrics` — sets `codex_client_info{version}` gauge to 1 and increments the `codex_client_changes` counter. Registered names: `"codex_client_info"` and `"codex_client_changes"` (the text encoder appends `_total` for counters; tests match on the prefix). Task 5 calls this.

- [ ] **Step 1: Write the failing test**

In `src/metrics.rs` `#[cfg(test)] mod tests`, append:

```rust
    #[test]
    fn codex_client_metrics_record_versions_and_changes() {
        let m = Metrics::new();
        m.record_codex_client("0.1.0");
        m.record_codex_client("0.2.0");
        // A re-sighting still counts as an observed client but the caller
        // (the tracker) only invokes this on first sightings; a second call
        // with the same version must not panic or duplicate labels.
        m.record_codex_client("0.1.0");
        let mut buf = String::new();
        m.encode(&mut buf).unwrap();
        assert!(buf.contains("codex_client_info"), "family present: {buf}");
        assert!(buf.contains(r#"version="0.1.0""#));
        assert!(buf.contains(r#"version="0.2.0""#));
        assert!(buf.contains("codex_client_changes"), "counter present");
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test codex_client_metrics`
Expected: compile error (`no method named record_codex_client`).

- [ ] **Step 3: Write the implementation**

3a. Labels struct (next to `GaugeLabels`, ~line 119):

```rust
/// Labels for the codex client info gauge: the client version string.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct CodexVersionLabels {
    version: String,
}
```

3b. `Metrics` struct: add two fields after `duration`:

```rust
    codex_client_info: Family<CodexVersionLabels, Gauge>,
    codex_client_changes: Counter,
```

3c. In `Metrics::new()`, construct + register (after the `duration` registration, ~line 180) and add both fields to the returned `Self { .. }`:

```rust
        let codex_client_info = Family::<CodexVersionLabels, Gauge>::default();
        let codex_client_changes = Counter::default();
        registry.register(
            "codex_client_info",
            "Codex client versions observed on /models, value 1",
            codex_client_info.clone(),
        );
        registry.register(
            "codex_client_changes",
            "Codex client version first-sightings detected",
            codex_client_changes.clone(),
        );
```

3d. Recording method (next to `record_tokens`):

```rust
    /// Record a first-sighting of a codex client version (spec §3): set the
    /// per-version info gauge to 1 and bump the change counter.
    pub(crate) fn record_codex_client(&self, version: &str) {
        self.codex_client_info
            .get_or_create(&CodexVersionLabels {
                version: version.to_string(),
            })
            .set(1);
        self.codex_client_changes.inc();
    }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test codex_client_metrics`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/metrics.rs
git commit -m "feat: add codex client version info gauge + change counter (spec §3)"
```

---

### Task 5: `src/proxy/mod.rs` — wire the tripwire into `handle_models`

**Files:**
- Modify: `src/proxy/mod.rs:209-215` (`AppState` fields), `src/proxy/mod.rs:347-352` (construction), `src/proxy/mod.rs:387-399` (`handle_models` `Some(_)` branch)
- Test: `tests/endpoints_metrics.rs` (append a test using the existing `setup()` harness)

**Interfaces:**
- Consumes: `crate::version::CodexVersionTracker::observe`, `crate::version::{normalize_version, DoctorState}`, `crate::metrics::Metrics::record_codex_client`.
- Produces: `AppState.codex_version: crate::version::CodexVersionTracker`. Behavior: on a first-sighted `client_version` — one `info!` line, metrics recorded, one `warn!` when the version is not green in the state file. Re-sightings and 304s are silent. The catalog response itself is unchanged.

- [ ] **Step 1: Write the failing integration test**

Append to `tests/endpoints_metrics.rs` (uses the shared `common::setup()` harness like the neighboring `/models` tests):

```rust
/// First-sighting of codex client versions must surface in /metrics: one
/// info-gauge label per distinct version plus the change counter, while
/// repeat sightings add nothing (spec §3).
#[tokio::test]
async fn models_client_version_observation_feeds_metrics() {
    let env = common::setup().await;

    // Two distinct versions, then a repeat of the first.
    for v in ["0.1.0", "0.2.0", "0.1.0"] {
        let resp = env
            .client
            .get(format!("{}/v1/models?client_version={v}", env.router_url))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    let metrics = env
        .client
        .get(format!("{}/metrics", env.router_url))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(metrics.contains("codex_client_info"), "{metrics}");
    assert!(metrics.contains(r#"version="0.1.0""#));
    assert!(metrics.contains(r#"version="0.2.0""#));
    assert!(metrics.contains("codex_client_changes"), "{metrics}");
    // The repeat sighting logged/metriced nothing new; the counter text
    // line's exact value format varies by encoder, so assert the family
    // exists only (already done above).
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test endpoints_metrics models_client_version`
Expected: FAIL — `codex_client_info` missing from the scraped `/metrics` (nothing observes the version yet).

- [ ] **Step 3: Implement**

3a. `AppState` (`src/proxy/mod.rs:209-215`) — add after `metrics`:

```rust
    pub codex_version: crate::version::CodexVersionTracker,
```

3b. Construction (`src/proxy/mod.rs:347-352`) — add to the `AppState { .. }` literal:

```rust
        codex_version: crate::version::CodexVersionTracker::new(),
```

3c. `handle_models` — change the `Some(_)` arm to `Some(v)` and insert the observation block as its first statements (before `state.models.get(&config)`):

```rust
        Some(v) => {
            // First-sighting tripwire (spec §3): one info line per distinct
            // codex version per process, plus metrics and an unverified
            // warning until doctor goes green on this version. Event log,
            // NOT a per-request line (AGENTS.md #11 exception).
            if let Some(t) = state.codex_version.observe(&v) {
                let from = t.from.as_deref().unwrap_or("(none)");
                tracing::info!("codex client {from} → {v} detected — rerun `codexferry doctor`");
                state.metrics.record_codex_client(&v);
                let st = crate::version::DoctorState::read();
                let green = st
                    .last_green
                    .as_deref()
                    .and_then(crate::version::normalize_version);
                if crate::version::normalize_version(&v) != green {
                    let last = st.last_green.as_deref().unwrap_or("none");
                    tracing::warn!("codex {v} not verified by doctor (last green: {last})");
                }
            }
            // Codex ModelsResponse catalog shape (live model catalog).
            let (etag, body) = state.models.get(&config);
```

(Keep the rest of the arm — ETag/304 handling and the response — unchanged.)

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --test endpoints_metrics && cargo test version`
Expected: PASS (new test + no regressions; the pre-existing ETag tests at `client_version=0.0.0` still pass — observation does not alter responses).

- [ ] **Step 5: Commit**

```bash
git add src/proxy/mod.rs tests/endpoints_metrics.rs
git commit -m "feat: observe client_version on /models — version tripwire (spec §3)"
```

---

### Task 6: `src/doctor.rs` — offline quick-checks rewrite

**Files:**
- Modify: `src/doctor.rs` — rewrite `offline_checks` (lines 97-167), delete `default_catalog_path` (87-95) and `codex_wiring_info` (169-209), replace the test module (234-315). Update the module `//!` doc (lines 1-17) to describe quick-checks + live default instead of regenerate-and-compare.

**Interfaces:**
- Consumes: `crate::catalog::generate_catalog`, `crate::config::Config::parse_file`, `crate::version::{normalize_version, DoctorState}`.
- Produces:
  - `pub fn offline_checks(config_path: &Path, codex_models: Option<&Path>) -> Vec<Check>` (signature DROPS the `catalog_path` param).
  - `pub(crate) fn wiring_check(codex_toml: Option<&str>, expected_base: &str) -> Check` — PASS when ≥1 `[model_providers.X].base_url` equals `expected_base`; INFO with the exact snippet otherwise; INFO-skip when the file/TOML/providers are absent. Never FAIL. (Refines spec §4 check 3: at-least-one-match semantics, because a user's `~/.codex/config.toml` legitimately holds other providers.)
  - `pub(crate) fn shadow_check(codex_toml: Option<&str>) -> Option<Check>` — `Some(INFO)` when `model_catalog_json` is set, `None` otherwise.
  - `pub(crate) fn version_status(codex_version_output: Option<String>, state: &DoctorState) -> Check` — PASS/INFO only, never FAIL.
- Task 9 consumes the new `offline_checks` signature.

- [ ] **Step 1: Write the failing tests**

Replace the whole `#[cfg(test)] mod tests` block in `src/doctor.rs` with:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
[providers.x]
base_url = "https://x.com/v1"
api_key = "k"
format = "chat"
[routes]
"x/model-a" = { model = "a", context_window = 131072 }
"#;

    fn write_config(dir: &std::path::Path) -> std::path::PathBuf {
        let p = dir.join("config.toml");
        std::fs::write(&p, CONFIG).unwrap();
        p
    }

    #[test]
    fn quick_checks_never_fail_on_a_good_config() {
        // Runs on machines without ~/.codex and without codex on PATH:
        // every environment-dependent check is INFO-skip, never FAIL.
        let dir = tempfile::tempdir().unwrap();
        let checks = offline_checks(&write_config(dir.path()), None);
        assert!(!report_has_fail(&checks), "{checks:?}");
        let cfg = checks.iter().find(|c| c.name == "config loads").unwrap();
        assert!(matches!(cfg.status, CheckStatus::Pass));
    }

    #[test]
    fn broken_config_fails_config_load_check() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "not toml [").unwrap();
        let checks = offline_checks(&p, None);
        assert!(report_has_fail(&checks));
        let cfg = checks.iter().find(|c| c.name == "config loads").unwrap();
        assert!(matches!(cfg.status, CheckStatus::Fail));
    }

    #[test]
    fn wiring_check_passes_on_matching_provider() {
        let toml = r#"
[model_providers.codexferry]
base_url = "http://127.0.0.1:8787/v1"
"#;
        let c = wiring_check(Some(toml), "http://127.0.0.1:8787/v1");
        assert!(matches!(c.status, CheckStatus::Pass), "{c:?}");
    }

    #[test]
    fn wiring_check_infos_snippet_when_no_provider_matches() {
        let toml = r#"
[model_providers.other]
base_url = "https://api.openai.com/v1"
"#;
        let c = wiring_check(Some(toml), "http://127.0.0.1:8787/v1");
        assert!(matches!(c.status, CheckStatus::Info), "{c:?}");
        assert!(c.detail.contains("[model_providers.codexferry]"), "{c:?}");
        assert!(c.detail.contains("http://127.0.0.1:8787/v1"), "{c:?}");
    }

    #[test]
    fn wiring_check_skips_when_file_or_table_absent() {
        assert!(matches!(wiring_check(None, "x").status, CheckStatus::Info));
        let c = wiring_check(Some("model = \"gpt-5.6\"\n"), "x");
        assert!(matches!(c.status, CheckStatus::Info));
    }

    #[test]
    fn shadow_check_flags_model_catalog_json() {
        assert!(shadow_check(Some("model_catalog_json = \"/tmp/x.json\"\n")).is_some());
        assert!(shadow_check(Some("model = \"gpt-5.6\"\n")).is_none());
        assert!(shadow_check(None).is_none());
    }

    #[test]
    fn version_status_passes_only_on_green_match() {
        let green = DoctorState {
            last_green: Some("0.158.0".into()),
            last_run: None,
        };
        let pass = version_status(Some("codex-cli 0.158.0".into()), &green);
        assert!(matches!(pass.status, CheckStatus::Pass), "{pass:?}");
        let unverified = version_status(Some("codex-cli 0.160.0".into()), &green);
        assert!(matches!(unverified.status, CheckStatus::Info), "{unverified:?}");
        let never = DoctorState::default();
        assert!(matches!(
            version_status(Some("0.160.0".into()), &never).status,
            CheckStatus::Info
        ));
        assert!(matches!(version_status(None, &never).status, CheckStatus::Info));
        assert!(matches!(
            version_status(Some("no digits".into()), &never).status,
            CheckStatus::Info
        ));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test doctor`
Expected: compile errors — `offline_checks` still takes 3 params, helpers don't exist.

- [ ] **Step 3: Implement**

3a. Delete `default_catalog_path` (87-95) and `codex_wiring_info` (169-209). Rewrite `offline_checks` (97-167) as:

```rust
/// Offline quick-checks (spec §4): fast, catalog-file-free checks that run
/// before (or instead of) the live probes. Every environment-dependent
/// check degrades to INFO — quick-checks never fail on a machine without
/// codex or `~/.codex/`.
pub fn offline_checks(config_path: &Path, codex_models: Option<&Path>) -> Vec<Check> {
    let mut checks = Vec::new();

    let generated = match catalog::generate_catalog(config_path, codex_models) {
        Ok(g) => g,
        Err(e) => return vec![Check::fail("config loads", format!("{e:#}"))],
    };
    checks.push(Check::pass(
        "config loads",
        format!(
            "{} route(s)",
            generated.catalog["models"]
                .as_array()
                .map(Vec::len)
                .unwrap_or(0)
        ),
    ));

    // Template tripwire (INFO only): fields the allowlist dropped — a new
    // Codex version's template fields surface here (spec §4 check 2).
    if generated.dropped_template_fields.is_empty() {
        checks.push(Check::info("template fields dropped", "none"));
    } else {
        checks.push(Check::info(
            "template fields dropped",
            generated.dropped_template_fields.join(", "),
        ));
    }

    // Codex-side wiring + shadow + version status read ~/.codex/config.toml
    // (HOME → USERPROFILE fallback, same rule as before) and the doctor
    // state file. All best-effort INFO; never FAIL.
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    let codex_toml = std::fs::read_to_string(
        std::path::PathBuf::from(&home).join(".codex").join("config.toml"),
    )
    .ok();
    let expected = crate::config::Config::parse_file(config_path)
        .ok()
        .map(|c| format!("http://{}:{}/v1", c.server.host, c.server.port))
        .unwrap_or_else(|| "http://127.0.0.1:8787/v1".to_string());
    checks.push(wiring_check(codex_toml.as_deref(), &expected));
    if let Some(c) = shadow_check(codex_toml.as_deref()) {
        checks.push(c);
    }
    let codex_version = std::process::Command::new("codex")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string());
    checks.push(version_status(codex_version, &crate::version::DoctorState::read()));
    checks
}

/// Best-effort codex wiring hint (spec §4 check 3): PASS when at least one
/// `[model_providers.X].base_url` points at this codexferry instance,
/// INFO with the exact snippet otherwise. At-least-one semantics: the
/// user's config legitimately holds providers for other services.
pub(crate) fn wiring_check(codex_toml: Option<&str>, expected_base: &str) -> Check {
    let Some(text) = codex_toml else {
        return Check::info("codex wiring", "~/.codex/config.toml not found (skipped)");
    };
    let Ok(val) = text.parse::<toml::Value>() else {
        return Check::info("codex wiring", "~/.codex/config.toml not valid TOML (skipped)");
    };
    let empty = toml::value::Table::new();
    let providers = val
        .get("model_providers")
        .and_then(|p| p.as_table())
        .unwrap_or(&empty);
    let matches: Vec<&String> = providers
        .iter()
        .filter(|(_, p)| {
            p.get("base_url").and_then(|b| b.as_str()) == Some(expected_base)
        })
        .map(|(k, _)| k)
        .collect();
    if matches.is_empty() {
        Check::info(
            "codex wiring",
            format!(
                "no model_providers entry points at {expected_base} — add:\n\
                 [model_providers.codexferry]\n\
                 base_url = \"{expected_base}\"\n\
                 wire_api = \"responses\""
            ),
        )
    } else {
        Check::pass(
            "codex wiring",
            format!("provider(s) {} point at {expected_base}", matches.join(", ")),
        )
    }
}

/// `model_catalog_json` shadows the live /models catalog (spec §4 check 4):
/// surface it as INFO so a forgotten stopgap pin is visible.
pub(crate) fn shadow_check(codex_toml: Option<&str>) -> Option<Check> {
    let text = codex_toml?;
    let val = text.parse::<toml::Value>().ok()?;
    val.get("model_catalog_json")?;
    Some(Check::info(
        "static catalog shadow",
        "model_catalog_json is set — it shadows the live /models catalog; \
         remove it unless it is a deliberate stopgap",
    ))
}

/// Version status vs the doctor state file (spec §4 check 5): visibility
/// only, never a FAIL — an unverified codex is a reminder to run doctor.
pub(crate) fn version_status(
    codex_version_output: Option<String>,
    state: &crate::version::DoctorState,
) -> Check {
    let Some(out) = codex_version_output else {
        return Check::info("codex version status", "codex not found — skipped");
    };
    let cur = crate::version::normalize_version(&out);
    let green = state.last_green.clone();
    match (cur, green) {
        (Some(c), Some(g)) if c == g => {
            Check::pass("codex version status", format!("{c} (verified by doctor)"))
        }
        (Some(c), g) => Check::info(
            "codex version status",
            format!(
                "{c} not verified (last green: {}) — rerun `codexferry doctor`",
                g.unwrap_or_else(|| "none".to_string())
            ),
        ),
        (None, _) => Check::info(
            "codex version status",
            "could not parse `codex --version` output",
        ),
    }
}
```

3b. Update `run_doctor` and its call site NOW so the crate compiles with the two-param `offline_checks` (Task 9 replaces this intermediate shape with the mode-based signature):

- `run_doctor` signature becomes `pub fn run_doctor(config_path: &Path, codex_models: Option<&Path>, live: bool) -> anyhow::Result<()>` — drop the `catalog: Option<&Path>` parameter and the `catalog_path` resolution lines; the offline branch becomes `let checks = offline_checks(config_path, codex_models);`.
- In `src/main.rs`: delete the `catalog` field from the `Doctor` clap variant AND the `catalog.as_deref()` argument at the call site in the same edit — the build must stay green after this task (the `live`/`offline` flags arrive in Task 9).

3c. Rewrite the module `//!` doc (lines 1-17) first paragraph to:

```rust
//! `doctor` subcommand: codex-upgrade tripwire for the codexferry ↔ Codex
//! contract.
//!
//! Default mode runs the offline quick-checks (config validation, template
//! dropped-field tripwire, wiring hints, version status — spec §4) and then,
//! when codex is present, the live probes. `--live` runs probes only;
//! `--offline` runs quick-checks only. The static-catalog
//! regenerate-and-compare audit is gone with the static-catalog flow
//! (spec 2026-08-22).
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test doctor && cargo build`
Expected: doctor tests pass; build green (including `main.rs` with the flag removed early).

- [ ] **Step 5: Commit**

```bash
git add src/doctor.rs src/main.rs
git commit -m "feat!: rewrite offline_checks as catalog-free quick-checks (spec §4)"
```

---

### Task 7: `src/doctor_live.rs` — `run` returns checks (no printing/exiting)

**Files:**
- Modify: `src/doctor_live.rs:30-72` (`run`), `src/doctor.rs:64-85` (`run_doctor` live branch)

**Interfaces:**
- Produces: `pub fn run(_config_path: &Path) -> anyhow::Result<Vec<Check>>` — env gate (exit 2) and the probe-thread plumbing stay INSIDE; `print_report`/`exit(1)` move to the caller. Task 9's orchestrator consumes the returned checks.
- No unit tests possible without real codex; verified by compile + Task 12's manual matrix.

- [ ] **Step 1: Change the signature and move the printing**

In `doctor_live.rs`, replace the tail of `run` (lines 62-71):

```rust
// BEFORE:
    let checks = match probe.join() {
        Ok(Ok(checks)) => checks,
        Ok(Err(e)) => return Err(e),
        Err(_) => anyhow::bail!("doctor --live probe thread panicked"),
    };
    print_report(&checks);
    if report_has_fail(&checks) {
        std::process::exit(1);
    }
    Ok(())

// AFTER:
    match probe.join() {
        Ok(Ok(checks)) => Ok(checks),
        Ok(Err(e)) => Err(e),
        Err(_) => anyhow::bail!("doctor --live probe thread panicked"),
    }
```

Update the `use` at line 19 (`use crate::doctor::{print_report, report_has_fail, Check};`) to `use crate::doctor::Check;` and update `run`'s doc comment to state that the CALLER prints the report and decides the exit code (the env-gate exit 2 stays here).

- [ ] **Step 2: Update the caller**

In `src/doctor.rs` `run_doctor`, the live branch becomes:

```rust
    if live {
        let checks = crate::doctor_live::run(config_path)?;
        print_report(&checks);
        if report_has_fail(&checks) {
            std::process::exit(1);
        }
        return Ok(());
    }
```

- [ ] **Step 3: Build and run the existing suites**

Run: `cargo build && cargo test doctor_live && cargo test doctor`
Expected: green (`doctor_live` unit tests for `pick_tool` etc. are untouched).

- [ ] **Step 4: Commit**

```bash
git add src/doctor_live.rs src/doctor.rs
git commit -m "refactor: doctor_live::run returns checks; caller prints and exits"
```

---

### Task 8: `src/doctor_live.rs` — probe 1 omits `model_catalog_json`

**Files:**
- Modify: `src/doctor_live.rs:157-169` (probe loop), `423-442` (`run_codex_probe_with_deadline`), `444-499` (`run_codex_probe`), module doc lines 1-17 (add the probe-split paragraph)

**Interfaces:**
- Produces: `run_codex_probe_with_deadline(route: &str, catalog: Option<&Path>, router_port: u16, prompt: &'static str) -> ProbeOutcome` and the same `Option<&Path>` change on `run_codex_probe`. `None` → the `-c model_catalog_json=…` argument is omitted entirely.
- Probe split (spec §2): responses probe → `None` (codex must resolve `doctor/resp` via the temporary codexferry instance's live `/models`); chat probe → `Some(&catalog_path)` (pinned, unchanged).

- [ ] **Step 1: Implement the split**

1a. In the probe loop (lines 157-169), build a per-route catalog argument:

```rust
    for (idx, (route, endpoint)) in [("doctor/resp", "responses"), ("doctorchat/chat", "chat")]
        .into_iter()
        .enumerate()
    {
        println!("[{}/6] probing {route} ({endpoint} upstream)", idx + 4);
        let sentinel = sentinel_dir.join(format!("sentinel-{endpoint}.touched"));
        let _ = std::fs::remove_file(&sentinel);
        // Probe split (spec §2): the responses probe runs WITHOUT any
        // model_catalog_json — the fresh CODEX_HOME holds no static catalog
        // and `doctor/resp` exists in no bundled catalog, so codex can only
        // resolve the route through the live /v1/models?client_version=
        // fetch. Probe success proves the whole live-catalog contract. The
        // chat probe keeps the pinned temp catalog, isolating dialect
        // conversion from catalog concerns (bisection basis).
        let catalog_arg = if endpoint == "responses" {
            None
        } else {
            Some(catalog_path.as_path())
        };
        let outcome =
            run_codex_probe_with_deadline(route, catalog_arg, router_port, CODEX_PROMPT);
        checks.extend(shape_checks(&mock, endpoint, "doctor-upstream"));
        checks.push(sentinel_check(&sentinel));
        checks.push(exit_check(&outcome));
    }
```

1b. `run_codex_probe_with_deadline` (423-442): change the `catalog_path: &Path` parameter to `catalog: Option<&Path>` and pass it through to `run_codex_probe`.

1c. `run_codex_probe` (444-499): same parameter change; make the command builder conditional:

```rust
    let mut cmd = std::process::Command::new("codex");
    cmd.arg("exec")
        .arg("--skip-git-repo-check")
        .arg("-c")
        .arg("model_provider=doctor")
        .arg("-c")
        .arg("model_providers.doctor.name=doctor")
        .arg("-c")
        .arg(format!(
            "model_providers.doctor.base_url=http://127.0.0.1:{router_port}/v1"
        ))
        .arg("-c")
        .arg("model_providers.doctor.env_key=DOCTOR_CODEX_KEY")
        .arg("-c")
        .arg("model_providers.doctor.wire_api=responses");
    // Omitted for the live-catalog probe: absent `model_catalog_json`
    // forces codex to resolve the route via /v1/models (spec §2).
    if let Some(catalog) = catalog {
        cmd.arg("-c")
            .arg(format!("model_catalog_json={}", catalog.display()));
    }
    cmd.arg("-c")
        .arg("approval_policy=never")
        .arg("-c")
        .arg("sandbox_mode=danger-full-access")
        .arg("-c")
        .arg("model_reasoning_effort=low")
        .arg("-m")
        .arg(route)
        .arg(prompt)
        .env("CODEX_HOME", &home)
        .env("DOCTOR_CODEX_KEY", "dummy")
        .stdin(std::process::Stdio::null())
        .output()
```

(Adapt the surrounding lines: the original chained `.arg(...)` on `Command::new("codex")` directly; introduce the `let mut cmd` binding as shown, keeping the rest of the function — CODEX_HOME handling, output matching — unchanged.)

1d. Add to the module doc (after the intro paragraph, ~line 17):

```rust
//! Probe split (spec 2026-08-22 §2): the responses probe runs without any
//! `model_catalog_json`, so codex MUST resolve its route through the live
//! `/v1/models?client_version=` catalog — proving the live-catalog contract
//! end-to-end. The chat probe pins the temp catalog, isolating dialect
//! conversion; which probe fails bisects catalog-contract vs dialect drift.
```

- [ ] **Step 2: Build and run the suite**

Run: `cargo build && cargo test`
Expected: green (`pick_tool` unit tests unaffected; no test spawns codex).

- [ ] **Step 3: Manual spot-check (only if codex is installed on this machine)**

Run: `cargo build && ./target/debug/codexferry doctor --live --config config.toml`
Expected: 6/6 progress steps; the responses probe's checks pass only via the live catalog fetch. (If codex is absent, skip — Task 12 covers the full manual matrix.)

- [ ] **Step 4: Commit**

```bash
git add src/doctor_live.rs
git commit -m "feat: responses probe resolves routes via live /models (spec §2)"
```

---

### Task 9: CLI reshape — modes, flags, dispatch

**Files:**
- Modify: `src/main.rs:100-125` (`Doctor` clap variant; the `catalog` field was already removed in Task 6) and `151-168` (dispatch), `src/doctor.rs` (`run_doctor`)

**Interfaces:**
- Produces: `pub enum DoctorMode { Default, LiveOnly, OfflineOnly }` (in `doctor.rs`) and `pub fn run_doctor(config_path: &Path, codex_models: Option<&Path>, mode: DoctorMode) -> anyhow::Result<()>`.
- CLI surface (spec §1): `codexferry doctor` (default = quick-checks + live probes when codex is runnable; codex absent → INFO "live probe skipped", exit by quick-checks), `--live` (live only; exit 2 on missing codex — handled inside `doctor_live::run`), `--offline` (quick-checks only, never exit 2). `--live` and `--offline` conflict.
- Consumes: Task 6 `offline_checks`, Task 7 `doctor_live::run`.

- [ ] **Step 1: Update clap**

In `src/main.rs`, the `Doctor` variant becomes:

```rust
    /// Check codexferry ↔ Codex contract health (upgrade tripwire).
    ///
    /// Default: offline quick-checks + live wire-shape probes (when codex
    /// is runnable). `--live`: probes only. `--offline`: quick-checks only.
    Doctor {
        /// Path to the codexferry TOML config (defaults to CODEXFERRY_CONFIG
        /// or ./config.toml, same rule as the server).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Optional explicit path to a Codex `models.json` template to
        /// inherit version-sensitive fields from (same meaning as in
        /// `gen-catalog`); when omitted, the template is located
        /// automatically (see `catalog::load_template`).
        #[arg(long)]
        codex_models: Option<PathBuf>,
        /// Run only the live wire-shape + tool round-trip probes.
        #[arg(long, conflicts_with = "offline")]
        live: bool,
        /// Run only the offline quick-checks (no codex needed).
        #[arg(long)]
        offline: bool,
    },
```

- [ ] **Step 2: Rewrite `run_doctor` dispatch**

In `src/doctor.rs`:

```rust
/// Execution mode selected by the CLI flags (spec §1).
pub enum DoctorMode {
    /// Quick-checks + live probes (codex absent → INFO skip).
    Default,
    /// Live probes only; exit 2 on unusable environment.
    LiveOnly,
    /// Quick-checks only; never spawns codex, never exits 2.
    OfflineOnly,
}

/// Whether `codex --version` runs successfully (spec §1 default-mode gate).
fn codex_available() -> bool {
    std::process::Command::new("codex")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Entry point from `main`: run checks, print the report, exit 1 on any
/// FAIL (2 stays reserved for environment failures on the live path).
pub fn run_doctor(
    config_path: &Path,
    codex_models: Option<&Path>,
    mode: DoctorMode,
) -> anyhow::Result<()> {
    let mut checks = match mode {
        DoctorMode::OfflineOnly => offline_checks(config_path, codex_models),
        DoctorMode::LiveOnly => {
            // doctor_live::run exits 2 itself on an unusable environment.
            let checks = crate::doctor_live::run(config_path)?;
            print_report(&checks);
            if report_has_fail(&checks) {
                std::process::exit(1);
            }
            return Ok(());
        }
        DoctorMode::Default => {
            let mut checks = offline_checks(config_path, codex_models);
            if codex_available() {
                checks.extend(crate::doctor_live::run(config_path)?);
            } else {
                checks.push(Check::info(
                    "live probe",
                    "skipped — codex not found or not runnable",
                ));
            }
            checks
        }
    };
    print_report(&checks);
    if report_has_fail(&checks) {
        std::process::exit(1);
    }
    Ok(())
}
```

- [ ] **Step 3: Update the `main.rs` dispatch**

```rust
        Some(Commands::Doctor {
            config,
            codex_models,
            live,
            offline,
        }) => {
            let config_path = config.unwrap_or_else(|| {
                std::env::var("CODEXFERRY_CONFIG")
                    .unwrap_or_else(|_| "config.toml".into())
                    .into()
            });
            let mode = if live {
                doctor::DoctorMode::LiveOnly
            } else if offline {
                doctor::DoctorMode::OfflineOnly
            } else {
                doctor::DoctorMode::Default
            };
            doctor::run_doctor(&config_path, codex_models.as_deref(), mode)?;
        }
```

Also update `main.rs`'s module doc (lines 15-19, the `doctor` bullet) to describe the three modes.

- [ ] **Step 4: Verify**

Run: `cargo build && cargo test && ./target/debug/codexferry doctor --offline --config config.toml; echo "exit=$?"`
Expected: build + tests green; the manual `--offline` run prints quick-checks and `exit=0` (or `exit=1` if the repo's `config.toml` fails validation — inspect the report; on this repo it should pass). `./target/debug/codexferry doctor --live --offline` must fail with a clap conflict error.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs src/doctor.rs
git commit -m "feat!: doctor CLI modes — default live, --offline quick-checks (spec §1)"
```

---

### Task 10: doctor verdict side-effects — state file, version header, remediation

**Files:**
- Modify: `src/doctor.rs` — add `remediation_lines` + verdict plumbing to `run_doctor`

**Interfaces:**
- Consumes: `crate::version::{DoctorState, LastRun, normalize_version}`.
- Produces: `pub(crate) fn remediation_lines(checks: &[Check]) -> Vec<String>` — maps check-failure patterns to diagnosis/fix/stopgap lines (spec §5); wired into `run_doctor` for any mode that ran live checks.
- State rules (spec §3.1): green run with live checks → `last_green = normalize_version(codex --version)`, `last_run` green; red run → `last_run` red only (previous `last_green` preserved).

- [ ] **Step 1: Write the failing tests**

Append to `src/doctor.rs` tests module:

```rust
    fn fail(name: &str) -> Check {
        Check::fail(name, "x")
    }
    fn pass(name: &str) -> Check {
        Check::pass(name, "x")
    }

    #[test]
    fn remediation_bisects_probe_failures() {
        // Class 3: responses-only failure (live catalog contract).
        let class3 = vec![
            fail("responses: upstream received a request"),
            pass("chat: upstream received a request"),
        ];
        let lines = remediation_lines(&class3);
        assert!(lines.iter().any(|l| l.contains("live catalog")), "{lines:?}");

        // Class 1: shape failures on both endpoints (dialect drift).
        let class1 = vec![
            fail("responses: top-level tools non-empty"),
            fail("chat: chat function tools non-empty"),
        ];
        let lines = remediation_lines(&class1);
        assert!(lines.iter().any(|l| l.contains("dialect")), "{lines:?}");

        // Class 2: shapes pass, sentinel/exit fail (response contract).
        let class2 = vec![
            pass("responses: top-level tools non-empty"),
            fail("tool round-trip executed"),
            fail("codex exited cleanly"),
        ];
        let lines = remediation_lines(&class2);
        assert!(lines.iter().any(|l| l.contains("response")), "{lines:?}");

        // Green: no remediation.
        assert!(remediation_lines(&[pass("anything")]).is_empty());
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test remediation`
Expected: compile error (`cannot find function remediation_lines`).

- [ ] **Step 3: Implement `remediation_lines`**

```rust
/// Remediation guidance derived from the bisection rules (spec §5):
/// - class 1 (dialect): any shape-check failure anywhere;
/// - class 3 (catalog): responses-endpoint failures only, no shape fails;
/// - class 2 (response): sentinel/exit failures with shapes passing;
/// - generic fallback otherwise.
/// Printed after the report whenever live checks exist and any check
/// failed.
pub(crate) fn remediation_lines(checks: &[Check]) -> Vec<String> {
    let is_fail = |c: &Check| c.status == CheckStatus::Fail;
    let is_shape = |c: &Check| {
        c.name.contains("tools non-empty")
            || c.name.contains("additional_tools input items")
            || c.name.contains("item types known")
    };
    let resp_fail = checks
        .iter()
        .any(|c| is_fail(c) && c.name.starts_with("responses:"));
    let chat_fail = checks
        .iter()
        .any(|c| is_fail(c) && c.name.starts_with("chat:"));
    let shape_fail = checks.iter().any(|c| is_fail(c) && is_shape(c));
    let tail_fail = checks.iter().any(|c| {
        is_fail(c) && (c.name == "tool round-trip executed" || c.name == "codex exited cleanly")
    });

    let mut out = Vec::new();
    if shape_fail {
        out.push(
            "Diagnosis: request dialect drift — codex changed how it delivers tools/items. \
             Fix: update normalize.rs and release. \
             Stopgap: pin/lock the codex version until the release."
                .to_string(),
        );
    } else if resp_fail && !chat_fail {
        out.push(
            "Diagnosis: live catalog contract — codex rejected or could not fetch /v1/models \
             (the probe with a pinned static catalog passed). Fix: catalog.rs, add the newly \
             required field(s). Stopgap: codexferry gen-catalog, hand-add the missing fields, \
             point model_catalog_json at the file."
                .to_string(),
        );
    } else if tail_fail {
        out.push(
            "Diagnosis: response-side contract — codex failed to consume the SSE or execute \
             the tool (see the stderr tail above). Fix: usually convert/response.rs. \
             Stopgap: lock the codex version."
                .to_string(),
        );
    } else if resp_fail || chat_fail {
        out.push(
            "Unclassified probe failure — inspect the failing checks above and the codex \
             stderr tail."
                .to_string(),
        );
    }
    out
}
```

- [ ] **Step 4: Wire into `run_doctor`**

Change `run_doctor` to track whether live checks ran, print a version header, write state, and print remediation. Replace the `Default` arm and the tail of the function:

```rust
        DoctorMode::Default => {
            let mut checks = offline_checks(config_path, codex_models);
            let codex_version = if codex_available() {
                checks.extend(crate::doctor_live::run(config_path)?);
                std::process::Command::new("codex")
                    .arg("--version")
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                checks.push(Check::info(
                    "live probe",
                    "skipped — codex not found or not runnable",
                ));
                None
            };
            finish(checks, codex_version)
        }
```

And the shared tail (replacing the final `print_report(&checks); ...` block; `LiveOnly` now also routes through it):

```rust
/// Print the verdict side-effects: version header lines, the report,
/// remediation on failure, and the state-file update (spec §3.1/§5).
/// Green updates `last_green`; red preserves it (the unverified warning
/// persists until a green run).
fn finish(checks: Vec<Check>, codex_version: Option<String>) -> anyhow::Result<()> {
    if let Some(v) = &codex_version {
        println!("codex:      {v}");
    }
    println!("codexferry: {}", env!("CARGO_PKG_VERSION"));
    print_report(&checks);
    let green = !report_has_fail(&checks);
    if !green {
        let lines = remediation_lines(&checks);
        if !lines.is_empty() {
            println!();
            println!("Remediation:");
            for l in lines {
                println!("- {l}");
            }
        }
    }
    if let Some(v) = codex_version {
        let mut state = crate::version::DoctorState::read();
        state.last_run = Some(crate::version::LastRun {
            green,
            codex_version: Some(v.clone()),
            at_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            summary: if green {
                "all checks passed".to_string()
            } else {
                checks
                    .iter()
                    .filter(|c| c.status == CheckStatus::Fail)
                    .map(|c| c.name.clone())
                    .collect::<Vec<_>>()
                    .join("; ")
            },
        });
        if green {
            state.last_green = crate::version::normalize_version(&v);
        }
        if let Err(e) = state.write() {
            // State write failures never change the verdict (spec §3.1).
            tracing::warn!("failed to write doctor state: {e}");
        }
    }
    if !green {
        std::process::exit(1);
    }
    Ok(())
}
```

`LiveOnly` becomes:

```rust
        DoctorMode::LiveOnly => {
            let checks = crate::doctor_live::run(config_path)?;
            let codex_version = std::process::Command::new("codex")
                .arg("--version")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
            finish(checks, codex_version)
        }
```

`OfflineOnly` becomes `finish(offline_checks(config_path, codex_models), None)` — header prints only the codexferry version, no state write (no codex was verified), no remediation (no live checks → `remediation_lines` finds no probe names → empty).

- [ ] **Step 5: Run the tests and verify behavior**

Run: `cargo test doctor && cargo build && ./target/debug/codexferry doctor --offline --config config.toml; echo "exit=$?"`
Expected: tests pass; the manual run prints the codexferry version header + quick-checks, `exit=0`; no state file is written by `--offline`.

- [ ] **Step 6: Commit**

```bash
git add src/doctor.rs
git commit -m "feat: doctor verdict side-effects — state file, version header, remediation (spec §3.1, §5)"
```

---

### Task 11: `src/catalog.rs` — description string + quirk seam

**Files:**
- Modify: `src/catalog.rs:271-276` (`set_catalog_fields` description), catalog test at ~line 784, add the quirk seam near `build_catalog_value`

**Interfaces:**
- Produces: user-visible description `"codexferry route {route_key}"` (was `"Router route {route_key}"`) — product-facing string per the migration spec Layer 3 rule (spec §7). Catalog content changes ⇒ ETag changes ⇒ codex refetches once; benign.
- Produces: `fn catalog_quirks_for(_client_version: &str) -> CatalogQuirks` + `struct CatalogQuirks` — the EMPTY version-conditional seam (spec §6). Hard constraint documented in its doc comment: if ever populated, `CatalogCache`'s fingerprint MUST include the quirk set.

- [ ] **Step 1: Update the failing assertion first**

In `src/catalog.rs` tests, `pinned_fields_are_written_regardless_of_template` (~line 784):

```rust
// BEFORE:
        assert_eq!(entry["description"], "Router route x/model-a");
// AFTER:
        assert_eq!(entry["description"], "codexferry route x/model-a");
```

Run: `cargo test catalog`
Expected: FAIL (production still emits "Router route").

- [ ] **Step 2: Fix the production string**

In `set_catalog_fields` (~line 275):

```rust
// BEFORE:
    obj.insert(
        "description".into(),
        json!(format!("Router route {route_key}")),
    );
// AFTER:
    // Product-facing string: name the daemon, not the generic role
    // (migration spec Layer 3; redesign spec §7). Shown in codex's model
    // picker.
    obj.insert(
        "description".into(),
        json!(format!("codexferry route {route_key}")),
    );
```

- [ ] **Step 3: Add the quirk seam**

Near `build_catalog_value` (~line 86):

```rust
/// Version-conditional catalog quirks (spec 2026-08-22 §6). EMPTY today:
/// adding a field to catalog output is safe for ALL codex versions at once
/// (codex's ModelInfo is strict about MISSING fields, tolerant of unknown
/// ones), so the normal adaptation is an unconditional addition — version
/// branches are reserved for mutually-exclusive requirements, none known.
///
/// HARD CONSTRAINT if this is ever populated: `CatalogCache`'s fingerprint
/// (models_cache.rs `fingerprint_config`) MUST include the quirk set, or
/// two codex versions alternating requests would receive each other's
/// cached body and ETag. Unknown versions must use `default()`, never be
/// refused service.
#[derive(Default)]
struct CatalogQuirks;

/// The empty quirk-table seam (spec §6). Unused until a real mutually
/// exclusive requirement appears — see the struct doc before populating.
#[allow(dead_code)]
fn catalog_quirks_for(_client_version: &str) -> CatalogQuirks {
    CatalogQuirks
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test catalog && cargo test`
Expected: green (only the one pinned assertion changed; `grep -rn "Router route" src/` returns nothing).

- [ ] **Step 5: Commit**

```bash
git add src/catalog.rs
git commit -m "feat: codexferry-facing catalog description + empty quirk seam (spec §6-7)"
```

---

### Task 12: Docs sync + full verification

**Files:**
- Modify: `README.md`, `ARCHITECTURE.md`, `AGENTS.md`

**Interfaces:** none (documentation only). Repo convention #13: the three top-level docs describe the current codebase in the same change.

- [ ] **Step 1: README.md**

1a. Rewrite the `## doctor: regression check after Codex upgrades` section (lines ~316-343) to:

````markdown
## doctor: regression check after Codex upgrades

```bash
# Default: offline quick-checks + live probes (when codex is runnable)
codexferry doctor --config config.toml

# Fast path: quick-checks only (no codex needed; CI-safe)
codexferry doctor --offline --config config.toml

# Live probes only: in-process mock upstream + temporary codexferry
# instance + real `codex exec`, asserting the normalized wire shape and a
# full tool round-trip (offline, zero tokens)
codexferry doctor --live --config config.toml
```

The live probes are split: the responses probe runs WITHOUT any static
catalog, so codex must resolve its route through the live
`/v1/models?client_version=` fetch — proving the live-catalog contract
end-to-end. The chat probe pins a temp catalog, isolating dialect
conversion. Which probe fails bisects the problem: responses-only → live
catalog contract; both at shape checks → dialect drift; sentinel/exit →
response contract. On failure doctor prints a Remediation section with the
fix layer and stopgaps (e.g. `gen-catalog` + hand-added fields pinned via
`model_catalog_json` for catalog-contract breaks).

Exit codes: 0 all pass; 1 a check failed; 2 environment unusable (`--live`
with codex missing).

### Codex upgrade tripwire

The daemon observes the `client_version` value on every `/models` request.
The first sighting of a new codex version per daemon process logs
`codex client X → Y detected — rerun codexferry doctor` and, until doctor
goes green on that version, a warning. Green doctor runs record the version
in `~/.local/state/codexferry/doctor.json` (`$XDG_STATE_HOME` respected);
failed runs never update `last_green`, so the warning persists until a
green run.

### Codex upgrade runbook

```bash
cargo build --release
./target/release/codexferry doctor --live --config config.toml
# Red? Follow the printed Remediation section; stopgaps:
#   catalog-contract break → gen-catalog, hand-add missing fields,
#     point model_catalog_json at the file until the release
#   dialect/response break → lock the codex version until the release
```
````

1b. Reposition the `### gen-catalog (offline generation)` section (lines ~266-276): replace "for users who want a static `model_catalog_json` file (e.g. for `doctor` offline checks or if they cannot point Codex at the router)" with "as a **stopgap**: when a codex upgrade rejects the live catalog and the fix release is pending, generate a static file, hand-add the missing fields, and point `model_catalog_json` at it. Doctor no longer audits static catalogs."

- [ ] **Step 2: ARCHITECTURE.md**

2a. Update the `src/` tree listing (§ around lines 342-362): change the `doctor.rs` comment to `# doctor: quick-checks + live-probe orchestration (N)`, `doctor_live.rs` to mention the probe split, add a `version.rs` row (`# codex client-version tracker + doctor state (N)`), and update `metrics.rs`'s comment to mention the codex client family. Use real numbers: run `find src -name '*.rs' -not -path '*tests*' | xargs wc -l | sort -n` and copy them.

2b. Update the §11 test counts if the doc lists them (`cargo test 2>&1 | tail -5` for the totals).

- [ ] **Step 3: AGENTS.md**

3a. In the "Exactly one per-request log line" convention (#11), after the existing warn-exceptions sentence, add:

```markdown
  One further documented exception: the codex-version tripwire in
  `handle_models` logs one `info!` (first sighting of a client version per
  process) and one `warn!` (version not green in the doctor state file) —
  event logs, never per-request lines (spec 2026-08-22 §3).
```

3b. In "Module Responsibilities", add a `version.rs` row (`Codex client-version tracker, normalize_version, doctor state file`) and update the `doctor.rs` / `doctor_live.rs` rows to the new reality (quick-checks; probe split).

- [ ] **Step 4: Full verification (spec "Verification" matrix)**

```bash
cargo test                                   # 1. full suite green
# If the repo root has no config.toml, write one first from the minimal
# template below and pass --config to every doctor invocation:
#   [providers.x]
#   base_url = "https://x.com/v1"
#   api_key = "k"
#   format = "chat"
#   [routes]
#   "x/model-a" = { model = "a" }
./target/debug/codexferry doctor --offline --config config.toml   # 2. green without ~/.codex
HOME=/tmp/empty-home ./target/debug/codexferry doctor --offline --config config.toml  # 2b. green with no ~/.codex at all
./target/debug/codexferry doctor --config config.toml            # 3. with codex installed: green + state file written; rerun → version PASS
```

For matrix item 4 (tripwire simulation, needs a running daemon — skip if no config with a real upstream is handy, the integration test from Task 5 already covers the mechanics):

```bash
# Freshly started daemon + a doctored state file whose last_green differs:
mkdir -p ~/.local/state/codexferry
echo '{"last_green":"0.0.1"}' > ~/.local/state/codexferry/doctor.json
./target/debug/codexferry &   # with a valid config
curl "http://127.0.0.1:8787/v1/models?client_version=9.9.9" > /dev/null
# → stderr shows one info line + one unverified warn; repeat curl → no new lines
curl -s http://127.0.0.1:8787/metrics | grep codex_client
kill %1
```

- [ ] **Step 5: Commit**

```bash
git add README.md ARCHITECTURE.md AGENTS.md
git commit -m "docs: sync README/ARCHITECTURE/AGENTS for the doctor redesign (layer docs)"
```

---

## Self-Review notes (already applied)

- **Spec coverage:** §1 CLI → Task 9; §2 probe split → Tasks 7-8; §3 tripwire/state → Tasks 1-5 (+10 for writes); §4 quick-checks → Task 6; §5 remediation/state-on-verdict → Task 10; §6 quirk seam → Task 11; §7 terminology → Task 11 (+ doc wording throughout); Testing strategy → per-task tests + Task 12 matrix; Docs sync → Task 12.
- **Refinements over spec wording** (recorded): probe 1 *omits* `model_catalog_json` rather than pointing at a nonexistent file (spec §2 already records this refinement); wiring check uses at-least-one-provider semantics (a user's codex config legitimately holds other providers); `at_unix` epoch instead of ISO timestamp (no chrono dependency).
- **Type consistency:** `normalize_version -> Option<String>`, `CodexVersionTracker::observe(&str) -> Option<Transition>`, `DoctorState::{read,read_from,write,write_to}`, `Metrics::record_codex_client(&str)`, `offline_checks(&Path, Option<&Path>) -> Vec<Check>`, `remediation_lines(&[Check]) -> Vec<String>` — used identically in Tasks 5, 6, 9, 10.
