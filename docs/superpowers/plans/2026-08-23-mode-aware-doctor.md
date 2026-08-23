# Mode-Aware Doctor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the mode-aware doctor described in `docs/superpowers/specs/2026-08-23-mode-aware-doctor-design.md`. Detect mode (`pinned` / `dynamic` / `fallback`) from `~/.codex/config.toml`; run mode-appropriate checks; live probe mirrors the user's actual mode.

**Architecture:** Three layers of checks (L1 offline config / L2 mode-specific static / L3 live probe) plus the L4 tripwire from the archived work. A new `src/mode.rs` module owns detection so all callers agree. `src/doctor.rs` is restructured around mode; the existing `shadow_check` becomes mode-keyed (warn or confirm depending on mode).

**Tech Stack:** Rust (axum 0.8, tokio, prometheus-client 0.25, toml 0.8, clap 4). **No new dependencies.** The `toml` crate is already a dependency for config parsing.

**Spec:** `docs/superpowers/specs/2026-08-23-mode-aware-doctor-design.md` — read §Mode detection, §Check layers (L1–L4), §Failure bisection, §Mode-specific shadow_check replacement before starting.

**Reuse from archived branch `feat/doctor-live-catalog-archived`:** the tripwire stack (`src/version.rs`, `DoctorState`, `CodexVersionTracker`, `client_version` observer wire-in) is solid and unchanged in mechanism — Tasks 1 re-ports it. L1/L4 unchanged; L2/L3 are net-new.

## Global Constraints

- No new crates.
- Exit codes: 0 all pass, 1 any FAIL, 2 environment unusable (`--live` only).
- AGENTS.md #11 (exactly one per-request log line) — preserve; tripwire info/warn are once-per-version event logs already documented there.
- Tracing format strings use `{field}` placeholders capturing locals (AGENTS.md #3); for `Option` values assign a plain local first.
- Code comments in English, `//!` module docs, `///` on public items (AGENTS.md #10).
- Terminology (migration spec Layer 3): "router" stays; user-facing product-facing strings say "codexferry".
- Conventional Commits (`feat:`, `fix:`, `test:`, `docs:`, `refactor:`).
- Run `cargo test` (full suite) at minimum before each commit; targeted `cargo test <filter>` inside TDD loops.

---

### Task 1: Port `src/version.rs` from the archived branch

**Files:**
- Create: `src/version.rs` (copy from `feat/doctor-live-catalog-archived`)
- Modify: `src/main.rs` — add `mod version;` to the module list (alphabetical between `mod upstream;` and `mod wire;`)

**Interfaces (carried over from archived spec §3.2):**
- `pub fn normalize_version(s: &str) -> Option<String>` — trim, return LAST whitespace-separated token containing an ASCII digit.
- `pub struct DoctorState` + `pub fn load() -> Self`, `pub fn write_to(&self, path: &Path)`, `pub fn write(&self)` — state file at `$XDG_STATE_HOME/codexferry/doctor.json` (default `~/.local/state/codexferry/`).
- `pub struct CodexVersionTracker { seen: Mutex<HashSet<String>>, current: Mutex<Option<String>> }` — `pub fn observe(&self, version: &str) -> Option<Transition>`.

**Source:** `git show feat/doctor-live-catalog-archived:src/version.rs` (383 lines, all tests + docs intact).

- [ ] **Step 1: Cherry-pick src/version.rs as-is**

```bash
git show feat/doctor-live-catalog-archived:src/version.rs > src/version.rs
```

- [ ] **Step 2: Wire the module**

In `src/main.rs`, add `mod version;` to the module list in alphabetical order.

- [ ] **Step 3: Build + test**

```bash
cargo build && cargo test version
```

Expected: green. The archived module is self-contained — no other integration needed for it to compile and its tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/version.rs src/main.rs
git commit -m "feat: add version module (normalize_version, DoctorState, CodexVersionTracker)"
```

---

### Task 2: Wire tripwire into the daemon (`client_version` observer)

**Files:**
- Modify: `src/proxy/mod.rs` — `handle_models` calls `tracker.observe` on every request
- Modify: `src/main.rs::serve` (or wherever `AppState` is built) — construct `CodexVersionTracker`, share via `Arc`

**Interface:** mirrors the archived Task 5; the new spec does not change the tripwire contract. Reuse `Arc<CodexVersionTracker>` from `src/version.rs`.

- [ ] **Step 1: Add tracker field to AppState**

In `src/proxy/mod.rs`, find the `AppState` struct (around line 230–300). Add `pub version_tracker: Arc<codex_version::CodexVersionTracker>`. Add to the constructor arguments and the `serve` call in `src/main.rs`.

- [ ] **Step 2: Observe in handle_models**

In `proxy/mod.rs::handle_models`, before building the catalog, call:
```rust
if let Some(transition) = state.version_tracker.observe(&client_version) {
    info!("codex client {}->{} detected - rerun `codexferry doctor`", transition.from.as_deref().unwrap_or("none"), transition.to);
    // also: warn if last_green from DoctorState differs
}
```

- [ ] **Step 3: Build + test**

```bash
cargo build && cargo test
```

- [ ] **Step 4: Commit**

```bash
git add src/proxy/mod.rs src/main.rs
git commit -m "feat: tripwire CodexVersionTracker on /models client_version"
```

---

### Task 3: `src/mode.rs` — Mode detection

**Files:**
- Create: `src/mode.rs`
- Modify: `src/main.rs` — add `mod mode;`

**Interface:**
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode { Pinned, Dynamic, Fallback }

pub fn detect_mode(codex_toml_text: Option<&str>, active_provider: &str) -> Mode {
    // Parse codex_toml_text as TOML.
    // If top-level `model_catalog_json` is set -> Pinned.
    // Else if [model_providers.{active_provider}.auth].command is set -> Dynamic.
    // Else -> Fallback.
    // Parse failures -> Fallback (so doctor still runs; L2.7'' will WARN).
}
```

**Active provider resolution:** `codex_toml.model_provider` defaults to `"codexferry"` if absent.

- [ ] **Step 1: Write failing tests in `src/mode.rs`**

```rust
//! Detect which catalog wiring codex is using, so doctor can branch its checks.
//!
//! See `docs/superpowers/specs/2026-08-23-mode-aware-doctor-design.md` §Mode detection.

use toml::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `model_catalog_json` set: codex reads the pinned file
    /// (`StaticModelsManager`), `/v1/models` is never consulted.
    Pinned,
    /// No pin, provider has `[X.auth] command`: codex fetches `/v1/models`
    /// each session start (`OpenAiModelsManager`, `has_command_auth` gate).
    Dynamic,
    /// No pin, only `env_key`: codex falls through with default metadata
    /// (degraded; warn + recommend migration).
    Fallback,
}

pub const DEFAULT_ACTIVE_PROVIDER: &str = "codexferry";

pub fn detect_mode(codex_toml_text: Option<&str>, active_provider: &str) -> Mode {
    let Some(text) = codex_toml_text else { return Mode::Fallback; };
    let Ok(val) = text.parse::<Value>() else { return Mode::Fallback; };
    if val.get("model_catalog_json").and_then(Value::as_str).is_some() {
        return Mode::Pinned;
    }
    let provider = val
        .get("model_providers")
        .and_then(Value::as_table)
        .and_then(|t| t.get(active_provider))
        .and_then(Value::as_table);
    if provider.and_then(|t| t.get("auth")).and_then(Value::as_table)
        .and_then(|t| t.get("command")).and_then(Value::as_str).is_some()
    {
        return Mode::Dynamic;
    }
    Mode::Fallback
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_present_is_pinned() {
        let toml = r#"
            model = "x/y"
            model_provider = "codexferry"
            model_catalog_json = "catalog.json"

            [model_providers.codexferry]
            base_url = "http://127.0.0.1:8787/v1"
            wire_api = "responses"
            [model_providers.codexferry.auth]
            command = "echo"
            args = ["dummy"]
        "#;
        assert_eq!(detect_mode(Some(toml), DEFAULT_ACTIVE_PROVIDER), Mode::Pinned);
    }

    #[test]
    fn no_pin_with_auth_command_is_dynamic() {
        let toml = r#"
            model = "x/y"
            [model_providers.codexferry]
            base_url = "http://127.0.0.1:8787/v1"
            wire_api = "responses"
            [model_providers.codexferry.auth]
            command = "echo"
            args = ["dummy"]
        "#;
        assert_eq!(detect_mode(Some(toml), DEFAULT_ACTIVE_PROVIDER), Mode::Dynamic);
    }

    #[test]
    fn no_pin_and_no_auth_is_fallback() {
        let toml = r#"
            model = "x/y"
            [model_providers.codexferry]
            base_url = "http://127.0.0.1:8787/v1"
            env_key = "DUMMY"
        "#;
        assert_eq!(detect_mode(Some(toml), DEFAULT_ACTIVE_PROVIDER), Mode::Fallback);
    }

    #[test]
    fn missing_text_is_fallback() {
        assert_eq!(detect_mode(None, DEFAULT_ACTIVE_PROVIDER), Mode::Fallback);
    }

    #[test]
    fn unparseable_text_is_fallback() {
        assert_eq!(detect_mode(Some("not valid toml [[["), DEFAULT_ACTIVE_PROVIDER), Mode::Fallback);
    }

    #[test]
    fn uses_active_provider_argument() {
        // Default provider is "codexferry", but user can override.
        let toml = r#"
            model = "x/y"
            model_provider = "alt"
            [model_providers.codexferry]
            base_url = "x"
            env_key = "DUMMY"
            [model_providers.alt]
            base_url = "x"
            [model_providers.alt.auth]
            command = "echo"
            args = ["d"]
        "#;
        // alt provider has auth, codexferry does not -> use model_provider to pick alt -> Dynamic.
        assert_eq!(detect_mode(Some(toml), "alt"), Mode::Dynamic);
        // codexferry has only env_key -> Fallback.
        assert_eq!(detect_mode(Some(toml), "codexferry"), Mode::Fallback);
    }
}
```

- [ ] **Step 2: Run cargo test mode — expect green (write tests first, then this works)**

```bash
cargo test mode
```

Expected: 7 passed.

- [ ] **Step 3: Wire the module**

In `src/main.rs`, add `mod mode;` to the module list.

- [ ] **Step 4: Build + commit**

```bash
cargo build
git add src/mode.rs src/main.rs
git commit -m "feat: add mode detection (pinned/dynamic/fallback) from ~/.codex/config.toml"
```

---

### Task 4: Restructure `src/doctor.rs` offline_checks + replace shadow_check with mode-aware version

**Files:**
- Modify: `src/doctor.rs` — replace `shadow_check` with `mode_keyed_check`, refactor `offline_checks` to accept `Mode`, add L2.6 (version-age) and L1.4 (mode classification INFO line)

**Interface (new in `src/doctor.rs`):**
```rust
pub fn offline_checks_with_mode(
    config_path: &Path,
    codex_models: Option<&Path>,
    mode: Mode,
    codex_version: Option<String>,
    doctor_state: &DoctorState,
) -> Vec<Check>;
```

The old `offline_checks_with_env` is preserved as a thin wrapper that calls `detect_mode` then `offline_checks_with_mode` (keeps the existing `run_doctor` and its tests working during the transition).

- [ ] **Step 1: Add the mode-aware wrapper**

Inside `offline_checks_with_env`, after the existing L1 checks and before pushing them, call `crate::mode::detect_mode(codex_toml, &active_provider)` and push a single INFO check:

```rust
let mode = crate::mode::detect_mode(codex_toml, &active_provider);
checks.push(Check::info(
    "detected mode",
    format!("{:?} (active provider: {})", mode, active_provider),
));
```

This is the new L1.4 line. Existing tests that assert exact check names will need an update.

- [ ] **Step 2: Replace shadow_check with mode_keyed_check**

Delete `pub(crate) fn shadow_check(...)`. Add:

```rust
/// Mode-keyed version of the old shadow_check (spec §Mode-specific shadow_check replacement).
/// In pinned/fallback mode the pin is required -> confirm.
/// In dynamic mode the pin is a shadow -> warn to remove.
/// Never FAILs — only informs.
pub(crate) fn mode_keyed_check(codex_toml: Option<&str>, mode: Mode) -> Option<Check> {
    let text = codex_toml?;
    let val = text.parse::<toml::Value>().ok()?;
    let pin = val.get("model_catalog_json")?;
    let detail = match mode {
        Mode::Pinned => format!(
            "model_catalog_json = {:?} — codex reads this pin in pinned mode; \
             re-run `codexferry gen-catalog` after adding routes or upgrading codex",
            pin.as_str().unwrap_or("<not a string>")
        ),
        Mode::Dynamic => format!(
            "model_catalog_json = {:?} — pin shadows the live fetch enabled by auth.command; \
             remove the pin OR remove [model_providers.codexferry.auth] (pick one mode)",
            pin.as_str().unwrap_or("<not a string>")
        ),
        Mode::Fallback => format!(
            "model_catalog_json = {:?} — fallback wiring: codex cannot fetch /v1/models; \
             this pin is the only source of real model metadata. Keep it",
            pin.as_str().unwrap_or("<not a string>")
        ),
    };
    let title = match mode {
        Mode::Pinned | Mode::Fallback => "static catalog pin",
        Mode::Dynamic => "pin shadows live fetch",
    };
    Some(Check::warn(title, detail))
}
```

Wait — the old shadow_check emitted `Check::info`; the spec says the new check should also be `Check::info` ("Never FAIL"). I overrode to warn above; revert: use `Check::info` per spec §Mode-specific shadow_check replacement (line "The check is still never FAIL — it informs, never blocks."). Adjust:

```rust
let check = Check::info(title, detail);
```

- [ ] **Step 3: Wire the call**

In `offline_checks_with_env`, replace `if let Some(c) = shadow_check(codex_toml) { ... }` with the mode-aware call, passing the detected mode. Update the test that asserts `shadow_check_reports_the_static_catalog_pin` to:
- Pass a dynamic-mode toml with `auth.command` and `model_catalog_json` → expect the "shadows" detail.
- Pass a pinned-mode toml with no pin → expect no check (we already have `shadow_check_skips_when_pin_absent`).
- Pass a fallback-mode toml with `model_catalog_json` → expect the "fallback wiring" detail.

- [ ] **Step 4: Run doctor tests**

```bash
cargo test doctor
```

Expected: existing tests pass with the new INFO line; updated tests verify the mode-aware wording.

- [ ] **Step 5: Commit**

```bash
git add src/doctor.rs
git commit -m "refactor: replace shadow_check with mode_keyed_check (pinned/dynamic/fallback)"
```

---

### Task 5: Pinned-mode checks — pin ⊇ router, pin ⊆ router, field shape

**Files:**
- Modify: `src/doctor.rs` — add three new check functions called from `offline_checks_with_mode` when `mode == Mode::Pinned`

**Interface:**
```rust
/// Spec L2.8: every route key in the router config appears in the pin.
fn pin_covers_router(router: &ValidatedConfig, pin: &ModelsResponse) -> Option<Check>;
/// Spec L2.9: every slug in the pin appears in the router config.
fn pin_matches_router(router: &ValidatedConfig, pin: &ModelsResponse) -> Option<Check>;
/// Spec L2.10: each pinned ModelInfo has every field codex 0.147's ModelInfo requires.
fn pin_field_shape(pin: &ModelsResponse) -> Option<Check>;
```

The FAIL list:
- L2.8 FAIL: missing slugs + rerun-gen-catalog hint.
- L2.9 FAIL: orphan slugs + rerun-gen-catalog hint.
- L2.10 FAIL: missing fields + the offending entry's slug.

- [ ] **Step 1: Pin parsing**

`pin_file_check(text: &str) -> Result<ModelsResponse, _>` — wraps the `serde_json`/`toml` parse + `serde::Deserialize`. Reusable across the three checks. Add a unit test: valid JSON pin parses; malformed → `Err`.

- [ ] **Step 2: L2.8 + L2.9 — pin/router reconciliation**

Two simple set comparisons. Unit tests:
- pin ⊇ router (passes when pin has every router slug).
- pin ⊇ router (fails when router has a slug missing from pin).
- pin ⊆ router (passes / fails symmetrically).
- empty router → empty pin → both pass (degenerate but consistent).

- [ ] **Step 3: L2.10 — field shape**

Define `REQUIRED_FIELDS: &[&str]` listing the fields codex 0.147's `ModelInfo` requires (per `catalog.rs::set_catalog_fields` and the archived branch's analysis: `slug`, `display_name`, `supported_reasoning_levels`, `shell_type`, `visibility`, `supported_in_api`, `priority`, `support_verbosity`, `truncation_policy`, `experimental_supported_tools`). Emit one FAIL per missing-field entry, listing the slug.

Add tests: a fixture pin missing each field individually fails with that field named.

- [ ] **Step 4: Wire in offline_checks_with_mode**

When `mode == Mode::Pinned`:
- Read the pin text from the path `model_catalog_json` points at.
- If parse fails → FAIL "pin unreadable" + remediation (rerun gen-catalog).
- If parse OK → run the three checks; push any FAILs.

- [ ] **Step 5: Run tests**

```bash
cargo test doctor
```

- [ ] **Step 6: Commit**

```bash
git add src/doctor.rs
git commit -m "feat: pinned-mode checks (pin existence, pin-router reconciliation, field shape)"
```

---

### Task 6: Dynamic-mode checks — pin shadow, endpoint smoke, shape

**Files:**
- Modify: `src/doctor.rs` — add three new checks under `mode == Mode::Dynamic`

**Interface:**
```rust
/// Spec L2.7': dynamic mode with a pin is a misconfiguration (pin shadows the
/// live fetch). Always WARN — never FAIL — same severity as the old
/// shadow_check.
fn pin_shadow_in_dynamic(codex_toml: Option<&str>) -> Option<Check>;
/// Spec L2.8': the daemon's /v1/models endpoint must respond at all
/// (offline-level smoke — proves the endpoint isn't 500ing before
/// codexever sees it).
fn models_endpoint_reachable(base_url: &str) -> Check;
/// Spec L2.9': a GET to /v1/models with a sample client_version parses as
/// ModelsResponse and has every required field. (Does NOT prove codex
/// accepts it — that's L3.A.)
fn models_endpoint_shape(base_url: &str) -> Check;
```

- [ ] **Step 1: L2.7' pin shadow check**

Pulled in by Task 4 already (the mode_keyed_check with Mode::Dynamic emits it). But split out as its own function `pin_shadow_in_dynamic` for readability + dedicated tests.

- [ ] **Step 2: L2.8' endpoint smoke**

Use `reqwest::blocking::Client` (or async via tokio + a small timeout) to GET `<base>/v1/models?client_version=doctor-l2`. FAIL on non-2xx (or timeout > 5s).

- [ ] **Step 3: L2.9' shape check**

Same call as L2.8' but also parse the response as `ModelsResponse` and check field set. FAIL on parse error or missing fields.

- [ ] **Step 4: Wire in offline_checks_with_mode**

When `mode == Mode::Dynamic`:
- Call L2.7', L2.8', L2.9'.

- [ ] **Step 5: Tests**

- L2.7' against a dynamic-mode toml with no pin → no check.
- L2.7' against a dynamic-mode toml WITH pin → check present with "shadows" detail.
- L2.8' / L2.9' against a stub HTTP server (use `axum::serve` or `wiremock` — wiremock is already used in `e2e-mock`); test PASS on 200 + valid body, FAIL on 500.

- [ ] **Step 6: Commit**

```bash
git add src/doctor.rs
git commit -m "feat: dynamic-mode checks (pin shadow, endpoint smoke, endpoint shape)"
```

---

### Task 7: L2.6 — version-age check (codex is newer than codexferry has verified)

**Files:**
- Modify: `src/doctor.rs` — add `fn version_age(codex_version: Option<&str>) -> Option<Check>`

**Interface:**
```rust
/// Spec L2.6: if the installed codex's normalized version is newer than any
/// version codexferry has tested against, INFO "codex X is newer than the
/// latest version codexferry has been verified against; run `codexferry doctor
/// --live` to re-verify".
///
/// For now, the "latest verified" set is empty (we are at the first release
/// that ships mode-aware doctor). Future codexferry releases extend it.
```

- [ ] **Step 1: Implement**

The check needs a constant list of "known-tested" codex versions, or a recent `latest_verified` recorded in `DoctorState`. For the first cut: hardcode `const LAST_VERIFIED: &[&str] = &[];` so any installed codex is "newer than what we've verified". Adjust later.

```rust
pub(crate) fn version_age(codex_version: Option<&str>) -> Option<Check> {
    let Some(raw) = codex_version else { return None; };
    let Some(cur) = crate::version::normalize_version(raw) else { return None; };
    if LAST_VERIFIED.is_empty() {
        return Some(Check::info(
            "codex version age",
            format!("{cur} has not been verified by codexferry doctor yet; run `codexferry doctor --live` to establish a baseline"),
        ));
    }
    // Parse semver-ish strings and compare; for now treat all as "newer".
    Some(Check::info(
        "codex version age",
        format!("{cur} is newer than the latest codexferry-verified version; run `codexferry doctor --live`"),
    ))
}
```

- [ ] **Step 2: Wire in offline_checks_with_mode**

Add to L1 always (mode-independent).

- [ ] **Step 3: Tests**

- No codex version provided → no check.
- codex 0.147.0 provided → check present with "not been verified" wording (since LAST_VERIFIED is empty).

- [ ] **Step 4: Commit**

```bash
git add src/doctor.rs
git commit -m "feat: L2.6 version-age INFO check"
```

---

### Task 8: Mode-aware live probe wiring — `scripts/e2e-lib.sh` helpers + `src/doctor_live.rs` dispatch

**Files:**
- Modify: `scripts/e2e-lib.sh` — add `run_codex_dynamic`, `run_codex_pinned`, `run_codex_fallback` (analogous to the existing `run_codex_static` and `run_codex_resume`)

**Interface (lib helpers):**
```bash
run_codex_dynamic() { -c auth={command="echo",args=["dummy"]}; NO model_catalog_json }
run_codex_pinned()   { -c auth={...}; -c model_catalog_json=... }
run_codex_fallback() { -c env_key="E2E_DUMMY_KEY"; NO model_catalog_json }
```

`run_codex_static` (existing) IS the pinned helper — rename to `run_codex_pinned` and update its callers (the existing scenarios). Or leave `run_codex_static` as alias for `run_codex_pinned` to minimize churn.

- [ ] **Step 1: Decide naming**

Recommendation: keep `run_codex_static` as the canonical pinned-mode helper (it's already used in 3 scenarios) and add `run_codex_dynamic` + `run_codex_fallback` next to it. Document each helper's mode clearly.

- [ ] **Step 2: Add helpers**

Each helper uses a 4-arg-or-5-arg signature consistent with the existing pattern:
```bash
run_codex_dynamic() { # args…: -m <route> "<prompt>"
  ... -c 'model_providers.e2e.auth={command="echo",args=["dummy"]}' ...
}
run_codex_fallback() { # args…: -m <route> "<prompt>"
  ... -c 'model_providers.e2e.env_key="E2E_DUMMY_KEY"' ...
}
```

- [ ] **Step 3: Wire `doctor_live.rs` dispatch**

In `doctor_live.rs`, replace the hardcoded `run_codex_static` / `run_codex_resume` calls with a mode-detection step:

```rust
let mode = crate::mode::detect_mode(codex_toml_text, active_provider);
let (run_first, run_resume) = match mode {
    Mode::Dynamic => ("run_codex_dynamic", "run_codex_resume"),
    Mode::Pinned => ("run_codex_static", "run_codex_resume_static"),
    Mode::Fallback => ("run_codex_fallback", "run_codex_resume_fallback"),
};
```

Resolve at runtime via `crate::run_codex_helper!(run_first, ...)` style or a registry. (If shell indirection proves brittle, port the dispatch into Rust.)

- [ ] **Step 4: Verify e2e.sh all still green**

```bash
scripts/e2e.sh all
```

- [ ] **Step 5: Commit**

```bash
git add scripts/e2e-lib.sh src/doctor_live.rs
git commit -m "feat: mode-aware live probe wiring (run_codex_dynamic/pinned/fallback)"
```

---

### Task 9: CLI reshape — `--offline` flag

**Files:**
- Modify: `src/main.rs` — `Commands::Doctor` adds `--offline: bool` (default false, meaning live probes run)
- Modify: `src/doctor.rs::run_doctor` — `live` arg only true when `--live` OR (default AND codex available)

**Interface:** unchanged from the archived spec §1, just re-applied to current branch state.

- [ ] **Step 1: Add the flag**

Clap derive on `Commands::Doctor`:
```rust
/// Skip live probes (L1 + L2 only). Useful for fast checks without codex.
#[arg(long)]
offline: bool,
```

- [ ] **Step 2: Wire in run_doctor**

```rust
let live = live || (codex_available && !offline);
```

(`live` is the existing `--live` flag; `offline` is the new inverse.)

- [ ] **Step 3: Test the CLI**

```bash
cargo build
./target/debug/codexferry doctor --help | grep -E "live|offline"
```

- [ ] **Step 4: Commit**

```bash
git add src/main.rs src/doctor.rs
git commit -m "feat: doctor --offline flag (skip live probes for fast checks)"
```

---

### Task 10: New e2e scenarios — `scenario_doctor_dynamic`, `scenario_doctor_pinned`, `scenario_doctor_fallback`

**Files:**
- Modify: `scripts/e2e.sh` — add three scenarios
- Modify: `scripts/e2e-lib.sh` — add `assert_doctor_passes` / `assert_doctor_fails_with` helpers

**Interface:**
```bash
scenario_doctor_dynamic() { # warm router, run doctor with dynamic-mode ~/.codex/config.toml, assert PASS lines }
scenario_doctor_pinned()   { # same + write pin.json, assert PASS including L2.10 }
scenario_doctor_fallback() { # same + WARN line for fallback mode }
```

- [ ] **Step 1: helpers**

`assert_doctor_offline_passes(codex_toml: &str, expected_mode: Mode)` — runs `codexferry doctor --offline`, parses the report, asserts the detected-mode INFO line + the expected PASS lines (and absence of FAIL).

`assert_doctor_offline_fails(codex_toml: &str, expected_fail_substr: &str)` — same but asserts a FAIL with the given substring.

- [ ] **Step 2: scenario_doctor_dynamic**

Spawn router with the existing 3-route mock config. Drop `~/.codex/config.toml` with `auth.command` (no pin). Run `codexferry doctor --offline`. Assert:
- INFO "detected mode: Dynamic"
- INFO "pin shadows live fetch" absent (no pin)
- L2.9' / L2.6 PASS

- [ ] **Step 3: scenario_doctor_pinned**

Same setup but write `~/.codex/codexferry-catalog.json` via `codexferry gen-catalog` first. Drop config with `model_catalog_json = "codexferry-catalog.json"`. Run doctor. Assert:
- INFO "detected mode: Pinned"
- L2.7 INFO confirm-pin
- L2.8/9/10 PASS

- [ ] **Step 4: scenario_doctor_fallback**

Same setup but config uses `env_key = "DUMMY"` and no pin. Run doctor. Assert:
- INFO "detected mode: Fallback"
- WARN "no auth.command and no pin" present

- [ ] **Step 5: Wire in dispatch**

Add the three to the case block + scenario list (`all` runs them in order: basic, models, static, tools, multiturn, cross_format_switch, stale_catalog, doctor_dynamic, doctor_pinned, doctor_fallback).

- [ ] **Step 6: Verify**

```bash
scripts/e2e.sh all
```

Expected: 10/10 pass.

- [ ] **Step 7: Commit**

```bash
git add scripts/e2e.sh scripts/e2e-lib.sh
git commit -m "test(e2e): doctor scenarios for dynamic, pinned, and fallback modes"
```

---

### Task 11: Docs sync — README, ARCHITECTURE, AGENTS

**Files:**
- Modify: `README.md` — rewrite "doctor" section to reflect the three modes + bisection table
- Modify: `ARCHITECTURE.md` — §doctor module descriptions
- Modify: `AGENTS.md` — list the new module (`mode.rs`); ensure the version-tripwire event-log exception is documented

- [ ] **Step 1: README**

Replace the existing "doctor: regression check after Codex upgrades" section with:
- Brief intro: doctor detects mode (pinned/dynamic/fallback) and runs the appropriate checks.
- Mode-specific guidance ("if you're on dynamic mode...").
- Updated bisection table from the spec.
- `--offline` flag mention.

- [ ] **Step 2: ARCHITECTURE.md**

- §Doctor module: add `src/mode.rs` to the module list.
- Update the doctor module responsibilities: `mode detection`, `mode-aware shadow_check`, `L2.7'–L2.9' dynamic checks`, `L2.8–L2.10 pinned checks`.
- Update §11 line counts.

- [ ] **Step 3: AGENTS.md**

- Add `mode.rs` to module responsibilities table.
- Confirm `version-tripwire event-log` is in the "Things to Watch Out For" list (likely already there from the archived branch — verify).

- [ ] **Step 4: Commit**

```bash
git add README.md ARCHITECTURE.md AGENTS.md
git commit -m "docs: mode-aware doctor (readme, architecture, agents)"
```

---

### Task 12: Full verification + final commit

- [ ] **Step 1: Full test suite**

```bash
cargo test
```

Expected: all green. Test count should be the prior 313 + ~20 new doctor + mode tests.

- [ ] **Step 2: e2e**

```bash
scripts/e2e.sh all
```

Expected: 10/10 scenarios pass.

- [ ] **Step 3: Real-layer smoke**

Skip token spend if not needed; `scripts/e2e-real.sh` already has `E2E_REAL_MODE` exercising both dynamic and static wirings.

- [ ] **Step 4: Manual doctor dry-run**

```bash
./target/debug/codexferry doctor --offline
```

Expected: exit 0, INFO lines for each check; one INFO naming the detected mode.

- [ ] **Step 5: Final commit if anything outstanding**

```bash
git status
# if clean: nothing
# if any: git commit -am "chore: post-verification tidy"
```

- [ ] **Step 6: PR**

Push the branch and create a PR against `main`:

```bash
git push origin feat/doctor-mode-aware
```

Use the Gitea token (per memory) to create the PR via API (the prior PR pattern works).

---

## End-to-end test scenario recap (matches the spec's verification matrix)

| Scenario | Expected L1/L2 outcome | Expected L3 outcome |
|---|---|---|
| dynamic, valid wiring | mode INFO Dynamic, no pin shadow | `codex client X detected` in router log, probe PASS |
| dynamic, with pin shadow | mode INFO Dynamic, pin shadow WARN | probe still PASSES (shadow doesn't break live) but the WARN persists |
| pinned, valid pin | mode INFO Pinned, confirm-pin INFO, L2.8/9/10 PASS | probe PASS, no `codex client detected` in router log (pinned doesn't fetch) |
| pinned, stale pin missing a route | mode INFO Pinned, L2.8 FAIL | (probe picks the missing route from cxf.toml... actually the probe would 400; L2.8 already FAILed so doctor exits 1) |
| fallback | mode INFO Fallback, fallback WARN | probe PASS but degraded (fallback metadata path) |
