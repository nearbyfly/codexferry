//! `doctor` subcommand: codex-upgrade tripwire for the codexferry ↔ Codex
//! contract.
//!
//! The offline quick-checks from the 2026-08-23 mode-aware design spec
//! (`docs/superpowers/specs/2026-08-23-mode-aware-doctor-design.md` §Check
//! layers L1/L2): config loads (L1.1), router has routes (L1.2), template
//! dropped-field tripwire, mode classification (L1.4), Codex-side wiring
//! hints (L1.3), the mode-keyed pin/shadow check — the replacement for the
//! old single `shadow_check` (spec §Mode-specific shadow_check replacement
//! + L2.7'': static-pin INFO / fallback-wiring WARN), the pin-shadow WARN
//!   (spec L2.7': fires whenever BOTH `model_catalog_json` and a provider
//!   `[X.auth].command` coexist, so a mixed pinned/dynamic config is caught
//!   no matter which mode `detect_mode` reports), the pinned-mode checks
//!   (spec L2.7–L2.10: pin exists + parses, pin ⊇ router routes, pin ⊆ router
//!   routes, pin entry field shape), the dynamic-mode endpoint checks (spec
//!   L2.8'–L2.9': `/v1/models` smoke + catalog shape), version age (L2.6) and
//!   version status vs the persisted doctor state. Every environment-
//!   dependent input is best-effort: no `~/.codex/config.toml` or no codex on
//!   PATH degrades to INFO/WARN. The one exception is L2.7: in pinned mode an
//!   unreadable pin FAILs, because codex cannot start until it is regenerated.
//!
//! The three run modes: `--offline` runs L1 + L2 only (fast path); the
//! default composes L1 + L2 with the L3 live probe from
//! [`crate::doctor_live`] into ONE report when codex is available (and
//! degrades to L1 + L2 when it is not); `--live` runs the L3 probe only
//! (unchanged user-visible behavior).
//!
//! Live-involving runs (`--live` and the composed default when codex is
//! available) persist their outcome to the doctor state file before
//! exiting (spec §L4): a green run records `last_run` and sets `last_green`
//! from the normalized codex version, while a red run records `last_run`
//! with `green = false` and preserves `last_green`. `--offline` never
//! writes state. State write failures are best-effort warnings and never
//! change the verdict or exit code.
//!
//! Exit codes: 0 all pass, 1 any FAIL, 2 environment unusable (codex not
//! installed or runnable; raised inside [`crate::doctor_live::run`], in
//! practice reachable via `--live` only). WARN is advisory and never fails
//! the run (spec L2.7'/L2.7''). Infrastructure errors (router bind failure,
//! healthz timeout, probe panic) propagate.

use crate::catalog;
use crate::mode::Mode;
use crate::version::DoctorState;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

/// One numbered line of the doctor report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckStatus {
    /// The check passed.
    Pass,
    /// The check failed: any FAIL fails the run and exits 1.
    Fail,
    /// Advisory warning (spec L2.7'/L2.7''): a supported-but-degraded
    /// wiring. A WARN never fails the run.
    Warn,
    /// Informational detail; never fails the run.
    Info,
}

/// A single numbered line of the doctor report: check name, status and
/// detail text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
}

impl Check {
    pub(crate) fn pass(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Pass,
            detail: detail.into(),
        }
    }
    pub(crate) fn fail(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Fail,
            detail: detail.into(),
        }
    }
    /// Advisory warning: degraded but supported wiring (spec L2.7'/
    /// L2.7''), reported but never counted as a failure.
    pub(crate) fn warn(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Warn,
            detail: detail.into(),
        }
    }
    pub(crate) fn info(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Info,
            detail: detail.into(),
        }
    }
}

/// Entry point from `main`. Prints one report and exits 1 on any FAIL (2
/// is reserved for the live path's environment gate).
///
/// Modes:
/// - `--live`: runs the L3 probe only ([`crate::doctor_live`]) — probe
///   checks + progress lines, no quick checks.
/// - `--offline`: runs the L1 + L2 quick checks only (fast path).
/// - default (neither flag): composes L1 + L2 then L3 into one report when
///   codex is available, else runs L1 + L2 only.
///
/// `--live` wins over `--offline` (mode-aware doctor spec: `live = live ||
/// (codex_available && !offline)`), and the `codex --version` spawn is
/// skipped whenever either flag decides. Live-involving runs persist their
/// outcome to the doctor state file before printing the report (spec §L4);
/// `--offline` never writes state. Exit codes: 0 all pass, 1 any FAIL on
/// every mode; 2 is raised only by [`crate::doctor_live::run`]'s
/// environment gate (codex missing or unrunnable) — in practice reachable via
/// `--live` only, since the default enters the live path only when
/// `codex_version_output()` has just succeeded. The composed default is not
/// unit-tested (it needs codex on PATH and free ports); it is covered by
/// CLI verification.
pub fn run_doctor(
    config_path: &Path,
    codex_models: Option<&Path>,
    live: bool,
    offline: bool,
) -> anyhow::Result<()> {
    // Resolve the live path only when neither flag already decides:
    // `--live` forces it and `--offline` skips it, so both avoid a
    // redundant `codex --version` spawn. The captured output doubles as
    // the persisted-state record on the composed default path.
    let codex_version = if live || offline {
        None
    } else {
        codex_version_output()
    };
    let available = codex_version.is_some();
    let run_live = live_requested(live, offline, available);
    if run_live && live {
        // `--live`: probe-only report (unchanged user-visible behavior —
        // the archived "prints + exits" contract is preserved by the
        // caller below).
        let checks = crate::doctor_live::run(config_path)?;
        // `doctor_live::run`'s environment gate has just proven codex
        // spawnable; re-read its version for the persisted state record.
        let codex_version = codex_version_output();
        let green = !report_has_fail(&checks);
        let state = apply_record(green, codex_version.as_deref(), DoctorState::read());
        if let Err(e) = state.write() {
            eprintln!("warning: could not write doctor state: {e}");
        }
        return finish_report(&checks);
    }
    if run_live && !live {
        // Default: ONE composed report — L1 + L2 quick checks first, then
        // the L3 live probe (spec verification matrix: "doctor (default =
        // offline + live)").
        let mut checks = offline_checks(config_path, codex_models);
        checks.extend(crate::doctor_live::run(config_path)?);
        let green = !report_has_fail(&checks);
        let state = apply_record(green, codex_version.as_deref(), DoctorState::read());
        if let Err(e) = state.write() {
            eprintln!("warning: could not write doctor state: {e}");
        }
        return finish_report(&checks);
    }
    // `--offline` (or no codex on the default path): fast L1 + L2 check.
    // Offline runs never persist state (spec §L4: `--offline` never marks
    // a version verified).
    finish_report(&offline_checks(config_path, codex_models))
}

/// Print a complete report and translate any FAIL into exit 1 (0
/// otherwise). Shared by all three doctor modes so the print-and-exit
/// decision stays in one place.
fn finish_report(checks: &[Check]) -> anyhow::Result<()> {
    print_report(checks);
    if report_has_fail(checks) {
        std::process::exit(1);
    }
    Ok(())
}

/// Run `codex --version` and return its trimmed stdout when the spawn
/// succeeds; `None` when codex is missing or exits non-zero. This is the
/// same environment gate `doctor_live::run` applies before probing.
/// [`run_doctor`] invokes it ONCE on the offline/default decision path (the
/// captured output doubles as the persisted-state record) and the explicit
/// `--live` branch invokes it once after the probe for the same record.
fn codex_version_output() -> Option<String> {
    std::process::Command::new("codex")
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Whether the live probes should run: `--live` always forces them, while
/// the default runs them only when codex is available AND `--offline` was
/// not given. `--live` wins over `--offline` (mode-aware doctor spec:
/// `live = live || (codex_available && !offline)`).
fn live_requested(live: bool, offline: bool, codex_available: bool) -> bool {
    live || (codex_available && !offline)
}

/// Spec §L4 state rule: apply the outcome of a live-involving run to a
/// copy of the persisted state. Green sets `last_green` (only when the
/// codex version normalizes) and records `last_run`; red records `last_run`
/// while preserving `last_green`. Never changes a verdict.
pub(crate) fn apply_record(
    green: bool,
    codex_version_output: Option<&str>,
    mut state: crate::version::DoctorState,
) -> crate::version::DoctorState {
    state.last_run = Some(crate::version::LastRun {
        green,
        codex_version: codex_version_output.map(str::to_string),
        at_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        summary: if green {
            "all checks passed".to_string()
        } else {
            "some checks failed".to_string()
        },
    });
    if green {
        if let Some(v) = codex_version_output.and_then(crate::version::normalize_version) {
            state.last_green = Some(v);
        }
    }
    state
}

/// Default Codex config location: `$HOME/.codex/config.toml`.
fn default_codex_config_path() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    PathBuf::from(home).join(".codex").join("config.toml")
}

/// Offline quick-checks (spec 2026-08-23 §L1 + common L2): fast,
/// catalog-file-free checks that never require a running router. Every
/// environment-dependent check degrades to INFO/WARN — quick-checks never
/// fail a machine without codex or `~/.codex/`.
///
/// This is the production entry point: it resolves the three environment
/// inputs (`~/.codex/config.toml`, `codex --version`, the doctor state file)
/// and hands them to [`offline_checks_with_env`], which holds all the logic.
/// The split exists so unit tests drive the checks from injected values and
/// never read the developer's real files or spawn `codex`.
pub fn offline_checks(config_path: &Path, codex_models: Option<&Path>) -> Vec<Check> {
    // Codex-side wiring, mode and version status are best-effort: every
    // read here is allowed to fail into `None`.
    let codex_toml = std::fs::read_to_string(default_codex_config_path()).ok();
    // The availability gate in `run_doctor` spawns `codex` separately; this
    // spawn is the quick-checks' own version probe (L2.6 + version status).
    // The two are independent: the gate decides which path runs, these
    // checks decide the version lines.
    let codex_version = std::process::Command::new("codex")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string());
    offline_checks_with_env(
        config_path,
        codex_models,
        codex_toml.as_deref(),
        codex_version,
        &DoctorState::read(),
    )
}

/// The quick-checks proper, with every environment-dependent input injected.
///
/// `codex_toml` is the TEXT of `~/.codex/config.toml` (`None` when absent),
/// `codex_version_output` the raw `codex --version` stdout (`None` when codex
/// is not installed), and `state` doctor's persisted state. Only the
/// codexferry config is read from disk, because its path is already a
/// caller-supplied parameter.
///
/// This thin wrapper resolves the active provider (`model_provider` at the
/// top level of `codex_toml`, default [`crate::mode::DEFAULT_ACTIVE_PROVIDER`]
/// — spec §Mode detection), detects the wiring mode, then delegates to
/// [`offline_checks_with_mode`].
pub(crate) fn offline_checks_with_env(
    config_path: &Path,
    codex_models: Option<&Path>,
    codex_toml: Option<&str>,
    codex_version_output: Option<String>,
    state: &DoctorState,
) -> Vec<Check> {
    let active_provider = codex_toml
        .and_then(|text| text.parse::<toml::Value>().ok())
        .and_then(|val| {
            val.get("model_provider")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| crate::mode::DEFAULT_ACTIVE_PROVIDER.to_string());
    let mode = crate::mode::detect_mode(codex_toml, &active_provider);
    offline_checks_with_mode(
        config_path,
        codex_models,
        mode,
        &active_provider,
        codex_toml,
        codex_version_output,
        state,
    )
}

/// The core mode-aware quick-checks (spec 2026-08-23 §L1 + common L2), in
/// report order:
///
/// 1. `config loads` (L1.1): `catalog::generate_catalog`; an Err returns a
///    single FAIL and skips all later checks.
/// 2. `router has routes` (L1.2): FAIL when the generated catalog has no
///    models (no `[routes]` entries).
/// 3. `template fields dropped`: INFO listing template fields the allowlist
///    dropped (a new Codex template's fields surface here).
/// 4. `detected mode` (L1.4): INFO naming the wiring mode + active provider.
/// 5. `codex wiring` (L1.3): [`wiring_check`] against this instance's bind
///    address.
/// 6. `mode_keyed_check`: one mode-specific pin/shadow check (spec
///    §Mode-specific shadow_check replacement) — static-pin INFO /
///    fallback-wiring WARN only; the dynamic pin-shadow arm lives in
///    [`pin_shadow_warn`] (item 7) so it cannot double-emit.
/// 7. `pin_shadow_warn` (L2.7'): WARN when the codex TOML carries BOTH a
///    `model_catalog_json` pin AND a provider `[X.auth].command` — the pin
///    forces `StaticModelsManager`, so the live fetch `auth.command` would
///    enable never runs. Called from BOTH the Pinned and Dynamic branches
///    (never Fallback: no pin), so a mixed config warns no matter how
///    `detect_mode` classified it — any present pin (string or non-string)
///    makes `detect_mode` say Pinned, which must not hide the shadow.
///    Emitted exactly once; always WARN, never FAIL.
/// 8. pinned-mode checks (spec L2.7–L2.10, only when `mode` is
///    [`Mode::Pinned`]): pin exists + parses, pin ⊇ router routes,
///    pin ⊆ router routes, and pin entry field shape.
/// 9. dynamic-mode checks (spec L2.8'–L2.9', only when `mode` is
///    [`Mode::Dynamic`]): ONE shared `/v1/models` fetch (see
///    [`fetch_models`]) feeding the endpoint smoke (L2.8') and endpoint
///    catalog shape (L2.9') checks against this instance's own bind address.
/// 10. `codex version age` (L2.6): [`version_age`] — with `LAST_VERIFIED`
///     empty for now, any installed codex is newer than everything codexferry
///     has verified against.
/// 11. `codex version status`: [`version_status`] vs the persisted state.
///
/// Items 10–11 are mode-independent ("L1 always" per the plan): they run in
/// every mode as the last two lines. The pinned L2.7–L2.10 checks ARE here
/// (the L2.7 pin-existence check, L2.8/L2.9 reconciliation, and L2.10 field
/// shape run in the block below);
/// [`mode_keyed_check`] implements the static-pin INFO / degraded-fallback
/// L2.7'' branches above, and [`pin_shadow_warn`] the both-keys L2.7' WARN
/// (called from both the Pinned and Dynamic branches below, because a string
/// pin classifies as Pinned and a Dynamic-only call would be unreachable).
///
/// `active_provider` and `codex_toml` are threaded through (rather than
/// re-read inside) so the L1.4 detail can name the provider and L1.3 /
/// mode_keyed_check can parse the Codex config the caller already holds.
pub(crate) fn offline_checks_with_mode(
    config_path: &Path,
    codex_models: Option<&Path>,
    mode: Mode,
    active_provider: &str,
    codex_toml: Option<&str>,
    codex_version_output: Option<String>,
    state: &DoctorState,
) -> Vec<Check> {
    let mut checks = Vec::new();

    let generated = match catalog::generate_catalog(config_path, codex_models) {
        Ok(g) => g,
        Err(e) => return vec![Check::fail("config loads", format!("{e:#}"))],
    };
    let route_count = generated.catalog["models"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0);
    checks.push(Check::pass(
        "config loads",
        format!("{} route(s)", route_count),
    ));

    // L1.2: an empty catalog means the config has no routes — the router
    // would answer every model with an unknown-route error.
    checks.push(if route_count == 0 {
        Check::fail(
            "router has routes",
            "no routes in config — add at least one [routes] entry",
        )
    } else {
        Check::pass("router has routes", route_count.to_string())
    });

    // Template tripwire (INFO only): fields the allowlist dropped — a new
    // Codex version's template fields surface here.
    if generated.dropped_template_fields.is_empty() {
        checks.push(Check::info("template fields dropped", "none"));
    } else {
        checks.push(Check::info(
            "template fields dropped",
            generated.dropped_template_fields.join(", "),
        ));
    }

    // L1.4: the wiring mode every mode-specific check branches on.
    checks.push(Check::info(
        "detected mode",
        format!("{:?} (active provider: {active_provider})", mode),
    ));

    // The expected Codex-side `base_url` is this instance's own bind address.
    // This re-reads the config file that `generate_catalog` already parsed
    // above, so the fallback is not dead code: it covers the TOCTOU window in
    // between (codexferry ships a hot-reload watcher, so a concurrent edit can
    // make this second parse fail). The fallback is derived from
    // `ServerConfig::default()` rather than hardcoded, so it cannot drift from
    // the real default host/port.
    let expected = crate::config::Config::parse_file(config_path)
        .ok()
        .map(|c| format!("http://{}:{}/v1", c.server.host, c.server.port))
        .unwrap_or_else(|| {
            let s = crate::config::ServerConfig::default();
            format!("http://{}:{}/v1", s.host, s.port)
        });
    checks.push(wiring_check(codex_toml, &expected));
    if let Some(c) = mode_keyed_check(codex_toml, mode) {
        checks.push(c);
    }
    // L2.7': the pin-shadow WARN fires whenever BOTH `model_catalog_json`
    // and a provider `[X.auth].command` coexist, regardless of the detected
    // mode — any present pin (string or non-string) makes `detect_mode` say
    // Pinned, but the `auth.command` still exists and the live fetch still
    // never runs under the pin. Each branch below calls [`pin_shadow_warn`]
    // exactly once.
    // Pinned-mode checks (spec L2.7–L2.10): the pin is codex's only catalog
    // source in static mode, so doctor validates the file offline before
    // codex hard-errors on it at session start.
    if mode == Mode::Pinned {
        if let Some(c) = pin_shadow_warn(codex_toml) {
            checks.push(c);
        }
        match read_pin(codex_toml) {
            Err(e) => checks.push(Check::fail("pin unreadable", e)),
            Ok((path, pin)) => {
                checks.push(Check::pass(
                    "pin exists and parses",
                    path.display().to_string(),
                ));
                // Re-parse + re-validate the router for reconciliation.
                // `generate_catalog` already validated the same file moments
                // ago, so a failure here is a concurrent hot-reload edit; in
                // that race the reconciliation checks are skipped rather
                // than compared against a half-updated config.
                if let Ok(router) =
                    crate::config::Config::parse_file(config_path).and_then(|c| c.validate())
                {
                    checks.push(pin_covers_router(&router, &pin));
                    checks.push(pin_matches_router(&router, &pin));
                    checks.extend(pin_field_shape(&pin));
                }
            }
        }
    }
    // Dynamic-mode checks (spec L2.8'–L2.9'): the live `/v1/models` fetch
    // enabled by `auth.command` is the catalog source, so doctor smokes the
    // endpoint codex will hit. `expected` re-read the router config above
    // (same fallback semantics as the L1.3 wiring hint), so the endpoint
    // checks target this instance's own bind address.
    if mode == Mode::Dynamic {
        if let Some(c) = pin_shadow_warn(codex_toml) {
            checks.push(c);
        }
        // ONE fetch, shared by both endpoint checks: a hung endpoint pays a
        // single send+body timeout pair, never two (the checks consume the
        // shared [`fetch_models`] outcome rather than fetching themselves).
        let fetch = fetch_models(&expected);
        checks.push(models_endpoint_reachable(&fetch));
        checks.push(models_endpoint_shape(&fetch));
    }
    // L2.6 (mode-independent): with `LAST_VERIFIED` empty, any readable
    // codex version is newer than what codexferry has verified against.
    if let Some(c) = version_age(codex_version_output.as_deref()) {
        checks.push(c);
    }
    checks.push(version_status(codex_version_output, state));
    checks
}

/// Best-effort codex wiring hint (spec L1.3): PASS when at least one
/// `[model_providers.X].base_url` points at this codexferry instance,
/// INFO with the exact snippet otherwise. At-least-one semantics: the
/// user's config legitimately holds providers for other services.
///
/// Never FAIL — the Codex-side config is the user's, not codexferry's.
pub(crate) fn wiring_check(codex_toml: Option<&str>, expected_base: &str) -> Check {
    let Some(text) = codex_toml else {
        return Check::info("codex wiring", "~/.codex/config.toml not found (skipped)");
    };
    let Ok(val) = text.parse::<toml::Value>() else {
        return Check::info(
            "codex wiring",
            "~/.codex/config.toml not valid TOML (skipped)",
        );
    };
    // A real TOML parse, not the old `contains("127.0.0.1")` heuristic: the
    // whole `base_url` must match, so a right-host/wrong-port provider is
    // correctly reported as unwired.
    let empty = toml::value::Table::new();
    let providers = val
        .get("model_providers")
        .and_then(|p| p.as_table())
        .unwrap_or(&empty);
    let matches: Vec<&str> = providers
        .iter()
        .filter(|(_, p)| p.get("base_url").and_then(|b| b.as_str()) == Some(expected_base))
        .map(|(k, _)| k.as_str())
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
            format!(
                "provider(s) {} point at {expected_base}",
                matches.join(", ")
            ),
        )
    }
}

/// Mode-keyed replacement for the old `shadow_check` (spec §Mode-specific
/// shadow_check replacement): the pin's meaning depends on the detected
/// wiring mode.
///
/// - pin present (`model_catalog_json` top-level, any value):
///   - `Mode::Pinned` → INFO confirming the pin (the file is the catalog
///     source in static mode);
///   - `Mode::Dynamic` → no check here: the dynamic pin-shadow WARN (L2.7')
///     is emitted by [`pin_shadow_warn`], which [`offline_checks_with_mode`]
///     calls from the Pinned and Dynamic branches — this function must not
///     double-emit it;
///   - `Mode::Fallback` → INFO: in fallback mode the pin is the only real
///     metadata source, keep it.
/// - pin absent:
///   - `Mode::Fallback` → WARN "fallback wiring" (L2.7''): no pin and no
///     `auth.command` means codex resolves routes with degraded fallback
///     metadata; generate a pin or switch to `auth.command`. Never FAIL.
///   - `Mode::Pinned` / `Mode::Dynamic` → no check.
/// - `codex_toml` absent or unparseable → no check.
///
/// Never FAIL in any mode: all three wirings are user-side supported
/// configurations, and doctor must not exit 1 over the user's own config.
pub(crate) fn mode_keyed_check(codex_toml: Option<&str>, mode: Mode) -> Option<Check> {
    let val = parse_codex_toml(codex_toml)?;
    let Some(pin) = val.get("model_catalog_json") else {
        return match mode {
            Mode::Fallback => Some(Check::warn(
                "fallback wiring",
                "no `model_catalog_json` and no `auth.command`: codex resolves routes \
                 with fallback metadata, which is degraded — generate a pin (static mode) \
                 or switch to `auth.command` (dynamic mode). See \
                 `scripts/codex-config-static.toml.example` and \
                 `scripts/codex-config-dynamic.toml.example`",
            )),
            Mode::Pinned | Mode::Dynamic => None,
        };
    };
    // `<not a string>` fallback: a non-string pin value is still a pin
    // (it forces the static manager), but there is no path to name.
    let pin_repr = pin.as_str().unwrap_or("<not a string>");
    // Dynamic with a pin is [`pin_shadow_warn`]'s job (see the docs
    // above) — returning early here is what prevents double-emission.
    let (name, status, detail) = match mode {
        Mode::Pinned => (
            "static catalog pin",
            CheckStatus::Info,
            format!(
                "model_catalog_json = {pin_repr:?} — codex reads this pin in pinned \
                 mode; re-run `codexferry gen-catalog` after adding routes or \
                 upgrading codex"
            ),
        ),
        Mode::Dynamic => return None,
        Mode::Fallback => (
            "static catalog pin",
            CheckStatus::Info,
            format!(
                "model_catalog_json = {pin_repr:?} — fallback wiring: codex cannot \
                 fetch `/v1/models`, so this pin is the only real source of model \
                 metadata; keep it"
            ),
        ),
    };
    Some(Check {
        name: name.into(),
        status,
        detail,
    })
}

/// Parse the caller-held codex TOML document, degrading to `None` on an
/// absent or unparseable file. Shared by [`mode_keyed_check`] and
/// [`pin_shadow_warn`], which both inspect `model_catalog_json` and provider
/// auth blocks — one small parse+degrade helper instead of two copies of the
/// same dance.
fn parse_codex_toml(codex_toml: Option<&str>) -> Option<toml::Value> {
    codex_toml?.parse::<toml::Value>().ok()
}

/// Spec L2.7': when the codex TOML has BOTH a `model_catalog_json` pin AND
/// at least one `[model_providers.X].auth.command` (any provider — the
/// spec's trigger is the two keys coexisting), the pin shadows the live
/// fetch: it forces `StaticModelsManager`, so the `/v1/models` fetch that
/// `auth.command` would enable never runs. Always WARN, never FAIL (the
/// wiring still works, the user just gets stale pinned metadata), advising
/// "remove the pin OR remove `auth.command` — pick one mode".
///
/// Mode-neutral by design: `detect_mode` classifies any present pin (string
/// or non-string) as [`Mode::Pinned`], so a mixed config would never reach
/// a Dynamic-only shadow check. [`offline_checks_with_mode`] therefore
/// calls this from BOTH the Pinned and Dynamic branches (exactly once per
/// run). Absent or unparseable `codex_toml`, no pin, or no provider
/// `auth.command` emits nothing.
pub(crate) fn pin_shadow_warn(codex_toml: Option<&str>) -> Option<Check> {
    let val = parse_codex_toml(codex_toml)?;
    let pin = val.get("model_catalog_json")?;
    let has_auth_command = val
        .get("model_providers")
        .and_then(toml::Value::as_table)
        .is_some_and(|providers| {
            providers.values().any(|p| {
                p.get("auth")
                    .and_then(toml::Value::as_table)
                    .is_some_and(|auth| auth.get("command").and_then(toml::Value::as_str).is_some())
            })
        });
    if !has_auth_command {
        return None;
    }
    // `<not a string>` fallback: a non-string pin value is still a pin
    // (it forces the static manager), but there is no path to name.
    let pin_repr = pin.as_str().unwrap_or("<not a string>");
    Some(Check::warn(
        "pin shadows live fetch",
        format!(
            "model_catalog_json = {pin_repr:?} — this pin shadows the live \
             `/v1/models` fetch enabled by `auth.command` (pin forces \
             StaticModelsManager, so that fetch never runs); remove the pin \
             OR remove `auth.command` — pick one mode"
        ),
    ))
}

/// The field set codex >= 0.147's `ModelInfo` requires (spec L2.10).
///
/// Mirrors the structural fields `catalog.rs::set_catalog_fields` always
/// emits: codex deserializes them with no serde default, so an entry missing
/// one is rejected outright at session start. Keep this list in sync with
/// `set_catalog_fields` when a codex upgrade adds a required field.
const REQUIRED_FIELDS: &[&str] = &[
    "slug",
    "display_name",
    "supported_reasoning_levels",
    "shell_type",
    "visibility",
    "supported_in_api",
    "priority",
    "support_verbosity",
    "truncation_policy",
    "experimental_supported_tools",
];

/// Resolve a `model_catalog_json` value to a filesystem path.
///
/// Absolute paths are used as-is; relative paths resolve against
/// `$HOME/.codex/` (the config root codex's static model manager treats as
/// current) when `HOME` is set, and are otherwise used unchanged.
fn resolve_pin_path(raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        return path;
    }
    match std::env::var("HOME") {
        Ok(home) => PathBuf::from(home).join(".codex").join(path),
        Err(_) => path,
    }
}

/// Read and parse the pinned catalog for pinned mode (spec L2.7).
///
/// Resolves the `model_catalog_json` path from the codex TOML the caller
/// already holds, reads the file, and parses it with [`pin_file_check`].
/// Every failure is folded into a `String` detail the wiring reports as the
/// single "pin unreadable" check, so the wiring stays one match arm. A
/// missing pin key is defensive here (a direct pinned-mode caller can lack
/// the key); a non-string pin is a real pin (`detect_mode` classifies any
/// present `model_catalog_json` as Pinned) and still fails the check.
fn read_pin(codex_toml: Option<&str>) -> Result<(PathBuf, Value), String> {
    let raw = codex_toml
        .and_then(|text| text.parse::<toml::Value>().ok())
        .and_then(|val| {
            val.get("model_catalog_json")
                .and_then(toml::Value::as_str)
                .map(str::to_string)
        })
        .ok_or_else(|| {
            "~/.codex/config.toml has no string `model_catalog_json` — run \
             `codexferry gen-catalog --config cxf.toml --out ~/.codex/codexferry-catalog.json` \
             and set `model_catalog_json` to that path"
                .to_string()
        })?;
    let path = resolve_pin_path(&raw);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("{}: {e} — rerun `codexferry gen-catalog`", path.display()))?;
    let pin = pin_file_check(&text)
        .map_err(|e| format!("{}: {e} — rerun `codexferry gen-catalog`", path.display()))?;
    Ok((path, pin))
}

/// Parse a pin file produced by `gen-catalog` (spec L2.7).
///
/// Pins are plain JSON `{"models": [...]}` (there is no `ModelsResponse`
/// struct in this repo), so the document is validated as a [`Value`]:
/// malformed JSON, a missing `models` key, or a non-array `models` are all
/// errors. Shared by the three pinned-mode checks.
fn pin_file_check(text: &str) -> Result<Value, String> {
    let val: Value =
        serde_json::from_str(text).map_err(|e| format!("pin is not valid JSON: {e}"))?;
    match val.get("models") {
        Some(Value::Array(_)) => Ok(val),
        _ => Err("pin has no `models` array".to_string()),
    }
}

/// The sorted, deduplicated STRING slug list of a parsed pin.
///
/// Non-string slugs are deliberately dropped here: reconciliation compares
/// router route keys against string slugs only, while [`pin_field_shape`]
/// rejects a present-but-non-string `slug` outright (codex's `ModelInfo`
/// deserialization would too). The caller's [`pin_file_check`] guarantees
/// `models` is an array, so extraction cannot fail.
fn pin_slugs(pin: &Value) -> Vec<&str> {
    let models = pin
        .get("models")
        .and_then(Value::as_array)
        .expect("pin_file_check guarantees a `models` array");
    let mut slugs: Vec<&str> = models
        .iter()
        .filter_map(|entry| entry.get("slug").and_then(Value::as_str))
        .collect();
    slugs.sort_unstable();
    slugs.dedup();
    slugs
}

/// Spec L2.8: every router route key must appear as a pin `models[].slug`.
///
/// Missing slugs mean the pin is stale relative to the router (or the
/// router is stale relative to the pin) — FAIL listing them with the
/// `gen-catalog` remediation. The caller's [`pin_file_check`] guarantees the
/// pin is a well-formed `{"models": [...]}` document.
fn pin_covers_router(router: &crate::config::ValidatedConfig, pin: &Value) -> Check {
    let slugs = pin_slugs(pin);
    let mut missing: Vec<&str> = router
        .routes
        .keys()
        .map(String::as_str)
        .filter(|route| !slugs.contains(route))
        .collect();
    missing.sort_unstable();
    if missing.is_empty() {
        Check::pass("pin covers router", "every router route is in the pin")
    } else {
        Check::fail(
            "pin covers router",
            format!(
                "pin is missing {} router route(s): {} — rerun `codexferry gen-catalog`",
                missing.len(),
                missing.join(", "),
            ),
        )
    }
}

/// Spec L2.9: every pin `models[].slug` must be a router route key.
///
/// Orphan slugs mean the pin holds models the router no longer serves —
/// FAIL listing them with the `gen-catalog` remediation. Symmetric to
/// [`pin_covers_router`].
fn pin_matches_router(router: &crate::config::ValidatedConfig, pin: &Value) -> Check {
    let slugs = pin_slugs(pin);
    let orphans: Vec<&str> = slugs
        .iter()
        .copied()
        .filter(|slug| !router.routes.contains_key(*slug))
        .collect();
    if orphans.is_empty() {
        Check::pass("pin matches router", "every pin slug is a router route")
    } else {
        Check::fail(
            "pin matches router",
            format!(
                "pin has {} orphan slug(s) not in the router: {} — rerun `codexferry gen-catalog`",
                orphans.len(),
                orphans.join(", "),
            ),
        )
    }
}

/// Spec L2.10: every pin entry must carry every [`REQUIRED_FIELDS`] field.
///
/// This is the static-mode equivalent of the live class-3 shape check: a
/// codex upgrade that adds a required `ModelInfo` field surfaces here as
/// soon as the user runs doctor. A present-but-non-string `slug` is treated
/// as a malformed `slug` field — `contains_key` alone would false-green an
/// entry codex rejects at session start. Returns one PASS when every entry
/// is complete, or one FAIL per offending entry naming the missing/malformed
/// fields and the entry's slug (a missing `slug` is identified as
/// `<missing slug>`, a non-string `slug` as `<non-string slug>`); a non-object
/// entry is itself malformed and FAILs as `<non-object entry>` — never
/// silently skipped, the same stance as [`models_endpoint_shape`]. The
/// per-entry field loop is shared with the dynamic-mode
/// [`models_endpoint_shape`] via [`entry_missing_fields`].
fn pin_field_shape(pin: &Value) -> Vec<Check> {
    let Some(models) = pin.get("models").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut checks = Vec::new();
    for entry in models {
        let Some(obj) = entry.as_object() else {
            checks.push(Check::fail(
                "pin entry shape",
                "pin entry <non-object entry> has missing or malformed required field(s)",
            ));
            continue;
        };
        let slug = entry_slug_label(obj);
        let missing = entry_missing_fields(obj);
        if !missing.is_empty() {
            checks.push(Check::fail(
                "pin entry shape",
                format!(
                    "pin entry {slug:?} has missing or malformed required field(s): {}",
                    missing.join(", "),
                ),
            ));
        }
    }
    if checks.is_empty() {
        vec![Check::pass(
            "pin entry shape",
            format!("all {} pin entries have the required fields", models.len()),
        )]
    } else {
        checks
    }
}

/// The [`REQUIRED_FIELDS`] of one `models[]` entry that are missing or
/// malformed. `slug` is the only special-cased field: a present-but-
/// non-string `slug` counts as missing because codex's `ModelInfo`
/// deserialization rejects it at session start — `contains_key` alone would
/// false-green such an entry. Shared by [`pin_field_shape`] and
/// [`models_endpoint_shape`] so the two catalog sources cannot drift.
fn entry_missing_fields(obj: &serde_json::Map<String, Value>) -> Vec<&str> {
    REQUIRED_FIELDS
        .iter()
        .copied()
        .filter(|field| match *field {
            "slug" => !matches!(obj.get("slug"), Some(Value::String(_))),
            _ => !obj.contains_key(*field),
        })
        .collect()
}

/// A human-readable slug label for a `models[]` entry, used in shape FAIL
/// details: the string slug itself, `<non-string slug>` for a present-but-
/// malformed `slug`, or `<missing slug>` when the key is absent.
fn entry_slug_label(obj: &serde_json::Map<String, Value>) -> &str {
    match obj.get("slug") {
        Some(Value::String(s)) => s.as_str(),
        Some(_) => "<non-string slug>",
        None => "<missing slug>",
    }
}

/// The outcome of one shared `/v1/models` fetch (spec L2.8'/L2.9'): either
/// the HTTP status + raw body, or the fetch error string. Both dynamic
/// endpoint checks consume ONE [`ModelsFetch`], so a dynamic offline run
/// costs a single GET (one send + one body timeout pair even on a hung
/// endpoint), never two.
type ModelsFetch = Result<(u16, String), String>;

/// One GET `<base>/v1/models?client_version=probe`, returning the HTTP status
/// and raw body as a [`ModelsFetch`].
///
/// `main` is `#[tokio::main]`, so the offline doctor path runs inside a
/// tokio runtime — a nested `Runtime::new()` would panic ("Cannot start a
/// runtime from within a runtime"). Mirror `doctor_live.rs`: run each GET on
/// a fresh OS thread with its own tokio current-thread runtime, apply a 5s
/// timeout around both the header phase and the body read, then `join()`.
/// A connect failure and a timeout both surface as a FAIL via the returned
/// error string.
///
/// The probe value is deliberately digit-less. `normalize_version` treats any
/// whitespace token containing a digit as a real codex version, so an earlier
/// `doctor-l2` value made the daemon's per-process version tripwire log a
/// bogus "codex client → doctor-l2 detected" transition and polluted the next
/// real turn's `from`. `probe` collapses onto the daemon's existing
/// `unparseable` sentinel instead — the one-time "codex client (none) →
/// unparseable detected" tripwire event on the first probe is expected
/// (see `proxy::observe_client_version`).
fn fetch_models(base_url: &str) -> ModelsFetch {
    let url = format!("{base_url}/models?client_version=probe");
    let join = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("tokio runtime: {e}"))?;
        rt.block_on(async move {
            let client = reqwest::Client::new();
            let resp = tokio::time::timeout(Duration::from_secs(5), client.get(&url).send())
                .await
                .map_err(|_| "timeout after 5s".to_string())?
                .map_err(|e| format!("request failed: {e}"))?;
            let status = resp.status().as_u16();
            let bytes = tokio::time::timeout(Duration::from_secs(5), resp.bytes())
                .await
                .map_err(|_| "timeout after 5s reading the body".to_string())?
                .map_err(|e| format!("body read failed: {e}"))?;
            Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
        })
    });
    join.join()
        .map_err(|_| "request thread panicked".to_string())?
}

/// Spec L2.8': offline smoke that the daemon's `/v1/models` endpoint responds
/// at all — the dynamic-mode class-3 endpoint must not 500 before codex sees
/// it. Consumes a [`ModelsFetch`] from [`fetch_models`], which the Dynamic
/// branch of [`offline_checks_with_mode`] shares with
/// [`models_endpoint_shape`] — the two checks never fetch separately. PASS on
/// a 2xx status, FAIL on a non-2xx status, a connect failure, or a >5s
/// timeout. Remediation on FAIL: start the daemon / check
/// `models_cache.rs::CatalogCache::get` (the server-side catalog path).
pub(crate) fn models_endpoint_reachable(fetch: &ModelsFetch) -> Check {
    const NAME: &str = "models endpoint reachable";
    match fetch {
        Ok((status, _)) if (200..300).contains(status) => {
            Check::pass(NAME, format!("GET /v1/models returned HTTP {status}"))
        }
        Ok((status, _)) => Check::fail(
            NAME,
            format!(
                "GET /v1/models returned HTTP {status} — the dynamic-mode catalog \
                 endpoint is down; start the daemon / check \
                 `models_cache.rs::CatalogCache::get` (the server-side catalog path)"
            ),
        ),
        Err(e) => Check::fail(
            NAME,
            format!(
                "GET /v1/models failed: {e} — the dynamic-mode catalog endpoint is \
                 down; start the daemon / check `models_cache.rs::CatalogCache::get` \
                 (the server-side catalog path)"
            ),
        ),
    }
}

/// Spec L2.9': the offline part of class 3 — the response to the same GET as
/// [`models_endpoint_reachable`] must be catalog-shaped. Parses the body as
/// JSON and asserts a `models` array whose entries carry every
/// [`REQUIRED_FIELDS`] field (the same set [`pin_field_shape`] asserts in
/// pinned mode). FAIL on a fetch failure, a non-2xx response, a parse error,
/// or a missing/malformed required field; PASS names the entry count. This
/// catches our own catalog output regression before codex has to. Consumes
/// the same shared [`ModelsFetch`] as [`models_endpoint_reachable`].
pub(crate) fn models_endpoint_shape(fetch: &ModelsFetch) -> Check {
    const NAME: &str = "models endpoint shape";
    let (status, body) = match fetch {
        Ok(fetched) => fetched,
        Err(e) => {
            return Check::fail(
                NAME,
                format!("GET /v1/models failed: {e} — cannot verify the catalog shape"),
            );
        }
    };
    if !(200..300).contains(status) {
        return Check::fail(
            NAME,
            format!("GET /v1/models returned HTTP {status}; cannot verify the catalog shape"),
        );
    }
    let val = match serde_json::from_str::<Value>(body) {
        Ok(val) => val,
        Err(e) => return Check::fail(NAME, format!("response is not valid JSON: {e}")),
    };
    let Some(models) = val.get("models").and_then(Value::as_array) else {
        return Check::fail(
            NAME,
            "response has no `models` array (not a catalog response)",
        );
    };
    let mut offenders: Vec<String> = Vec::new();
    for (i, entry) in models.iter().enumerate() {
        let Some(obj) = entry.as_object() else {
            offenders.push(format!("models[{i}] is not an object"));
            continue;
        };
        let missing = entry_missing_fields(obj);
        if !missing.is_empty() {
            offenders.push(format!(
                "{:?} missing {}",
                entry_slug_label(obj),
                missing.join(", ")
            ));
        }
    }
    if offenders.is_empty() {
        Check::pass(
            NAME,
            format!("all {} entries have the required fields", models.len()),
        )
    } else {
        Check::fail(
            NAME,
            format!(
                "{} entry(ies) missing required fields: {}",
                offenders.len(),
                offenders.join("; ")
            ),
        )
    }
}

/// Spec L2.6: if the installed codex's normalized version is newer than any
/// version codexferry has tested against, INFO "codex X is newer than the
/// latest version codexferry has been verified against; run `codexferry doctor
/// --live` to re-verify".
///
/// For now, the "latest verified" set is empty (we are at the first release
/// that ships mode-aware doctor). Future codexferry releases extend it.
const LAST_VERIFIED: &[&str] = &[];

/// The version-age check (spec L2.6 of the 2026-08-23 design): an INFO when
/// the installed codex has never been verified by codexferry — the first cut
/// has an empty [`LAST_VERIFIED`] set, so every readable version qualifies.
/// `None` when codex is absent or its `--version` output carries no digit
/// token to normalize. Visibility only, never a FAIL.
pub(crate) fn version_age(codex_version: Option<&str>) -> Option<Check> {
    let raw = codex_version?;
    let cur = crate::version::normalize_version(raw)?;
    if LAST_VERIFIED.is_empty() {
        return Some(Check::info(
            "codex version age",
            format!(
                "{cur} has not been verified by codexferry doctor yet; run \
                 `codexferry doctor --live` to establish a baseline"
            ),
        ));
    }
    // Parse semver-ish strings and compare; for now treat all as "newer".
    Some(Check::info(
        "codex version age",
        format!("{cur} is newer than the latest codexferry-verified version; run `codexferry doctor --live`"),
    ))
}

/// Version status vs the doctor state file (spec §3.2 of the 2026-08-22
/// spec, carried into L1.5 of the 2026-08-23 design): visibility only,
/// never a FAIL — an unverified codex is a reminder to run doctor.
///
/// Both sides go through [`crate::version::normalize_version`] and a `None`
/// on EITHER side means "unverified", never "equal" (spec §3.2). Comparing
/// the two `Option`s with `==` would be a false green, because `None == None`
/// is true in Rust; same tuple-match shape as
/// `proxy::version_is_doctor_verified`.
pub(crate) fn version_status(codex_version_output: Option<String>, state: &DoctorState) -> Check {
    let Some(out) = codex_version_output else {
        return Check::info("codex version status", "codex not found — skipped");
    };
    let cur = crate::version::normalize_version(&out);
    let green = state
        .last_green
        .as_deref()
        .and_then(crate::version::normalize_version);
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

/// Whether any check FAILED. WARN and INFO never fail the run (spec
/// L2.7'/L2.7'' — the dynamic/fallback advisories stay advisory).
pub fn report_has_fail(checks: &[Check]) -> bool {
    checks.iter().any(|c| c.status == CheckStatus::Fail)
}

/// Print the numbered PASS/FAIL/WARN/INFO report to stdout.
pub fn print_report(checks: &[Check]) {
    for (i, c) in checks.iter().enumerate() {
        let tag = match c.status {
            CheckStatus::Pass => "PASS",
            CheckStatus::Fail => "FAIL",
            CheckStatus::Warn => "WARN",
            CheckStatus::Info => "INFO",
        };
        println!(
            "[{}/{}] {tag}: {} — {}",
            i + 1,
            checks.len(),
            c.name,
            c.detail
        );
    }
}

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

    /// Representative dynamic-mode wiring: base_url at the default bind
    /// address (CONFIG has no `[server]` section), `auth.command` present,
    /// no pin.
    const DYNAMIC_CODEX_TOML: &str = r#"
model_provider = "codexferry"

[model_providers.codexferry]
base_url = "http://127.0.0.1:8787/v1"
wire_api = "responses"
[model_providers.codexferry.auth]
command = "echo"
args = ["dummy"]
"#;

    /// A dynamic-mode codex TOML pointing at a caller-chosen base URL:
    /// `auth.command` present, no pin, `model_provider` = codexferry (so
    /// `detect_mode` classifies it Dynamic).
    fn dynamic_codex_toml(base_url: &str) -> String {
        format!(
            "model_provider = \"codexferry\"\n\
             \n\
             [model_providers.codexferry]\n\
             base_url = \"{base_url}\"\n\
             wire_api = \"responses\"\n\
             [model_providers.codexferry.auth]\n\
             command = \"echo\"\n\
             args = [\"dummy\"]\n"
        )
    }

    /// The route-bearing subset of CONFIG plus a `[server] port` pointing at
    /// a test stub, so the expected Codex-side base (and the dynamic
    /// endpoint checks) target the stub instead of the default 8787.
    fn write_config_on_port(dir: &std::path::Path, port: u16) -> std::path::PathBuf {
        let p = dir.join("config.toml");
        std::fs::write(&p, format!("[server]\nport = {port}\n\n{CONFIG}")).unwrap();
        p
    }

    /// The environment a machine without `~/.codex/`, without codex on PATH
    /// and without a doctor state file presents. Tests drive the injected
    /// entry points with this so they never read the developer's real
    /// `~/.codex/config.toml` or state file, and never spawn codex.
    fn no_env() -> (Option<&'static str>, Option<String>, DoctorState) {
        (None, None, DoctorState::default())
    }

    /// `--live` always forces the live probes, even when `--offline` is set
    /// or codex is not available.
    #[test]
    fn live_requested_live_flag_forces_live_even_when_offline_or_codex_missing() {
        assert!(live_requested(true, true, false));
        assert!(live_requested(true, true, true));
        assert!(live_requested(true, false, false));
    }

    /// Without `--live`, `--offline` skips the live probes regardless of
    /// whether codex is available.
    #[test]
    fn live_requested_offline_skips_live_without_live_flag() {
        assert!(!live_requested(false, true, true));
        assert!(!live_requested(false, true, false));
    }

    /// The default (no flags) runs the live probes on a machine with codex
    /// available.
    #[test]
    fn live_requested_default_runs_live_when_codex_is_available() {
        assert!(live_requested(false, false, true));
    }

    /// Without codex the default degrades to the offline L1 + L2 checks.
    #[test]
    fn live_requested_default_skips_live_without_codex() {
        assert!(!live_requested(false, false, false));
    }

    // ---- apply_record (spec §L4 state rule) ----

    /// Seconds since the Unix epoch, for bounding `apply_record`'s clock
    /// reads in tests without depending on exact timing.
    fn epoch_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    /// Green + parseable version: `last_green` is set to the NORMALIZED
    /// version and `last_run` records the green run.
    #[test]
    fn apply_record_green_with_parseable_version_sets_last_green_and_records_run() {
        let before = epoch_now();
        let state = apply_record(true, Some("codex-cli 0.147.0"), DoctorState::default());
        let after = epoch_now();
        assert_eq!(state.last_green.as_deref(), Some("0.147.0"));
        let run = state.last_run.expect("green run must record last_run");
        assert!(run.green);
        assert_eq!(run.codex_version.as_deref(), Some("codex-cli 0.147.0"));
        assert!(
            (before..=after).contains(&run.at_unix),
            "at_unix {at} not in [{before}, {after}]",
            at = run.at_unix
        );
        assert!(run.summary.contains("passed"), "summary: {}", run.summary);
    }

    /// Green + unparseable (or missing) version: the verdict records green
    /// but `last_green` is PRESERVED from the input state — an unparseable
    /// version must never establish a verified version.
    #[test]
    fn apply_record_green_with_unparseable_or_missing_version_preserves_last_green() {
        let before = epoch_now();
        let mut stamps = Vec::new();
        for raw in [Some("no digits"), None] {
            let prior = DoctorState {
                last_green: Some("0.158.0".into()),
                last_run: None,
            };
            let state = apply_record(true, raw, prior);
            assert_eq!(
                state.last_green.as_deref(),
                Some("0.158.0"),
                "last_green must survive a green run with unparseable version"
            );
            let run = state.last_run.expect("green run must record last_run");
            assert!(run.green);
            stamps.push(run.at_unix);
        }
        let after = epoch_now();
        assert!(
            stamps.iter().all(|&at| (before..=after).contains(&at)),
            "stamps {stamps:?} outside [{before}, {after}]"
        );
    }

    /// Red: `last_run` records green=false while `last_green` is PRESERVED
    /// — the daemon's unverified warning persists until the next green run.
    #[test]
    fn apply_record_red_records_run_and_preserves_last_green() {
        let before = epoch_now();
        let prior = DoctorState {
            last_green: Some("0.158.0".into()),
            last_run: Some(crate::version::LastRun {
                green: true,
                codex_version: Some("codex-cli 0.158.0".into()),
                at_unix: 1,
                summary: "all checks passed".into(),
            }),
        };
        let state = apply_record(false, Some("codex-cli 0.160.0"), prior);
        let after = epoch_now();
        assert_eq!(state.last_green.as_deref(), Some("0.158.0"));
        let run = state.last_run.expect("red run must record last_run");
        assert!(!run.green);
        assert_eq!(run.codex_version.as_deref(), Some("codex-cli 0.160.0"));
        assert!((before..=after).contains(&run.at_unix));
        assert!(run.summary.contains("failed"), "summary: {}", run.summary);
    }

    /// Defaults (empty state): a red run with no version output records the
    /// red run and leaves `last_green` at its default `None`.
    #[test]
    fn apply_record_handles_default_empty_state() {
        let state = apply_record(false, None, DoctorState::default());
        assert_eq!(state.last_green, None);
        let run = state.last_run.expect("red run must record last_run");
        assert!(!run.green);
        assert_eq!(run.codex_version, None);
        assert!(run.at_unix > 0);
    }

    // The composed default (L1 + L2 then L3 in one report) is exercised by
    // CLI verification instead of a unit test: it needs codex on PATH and
    // free ports, so injecting it would only test trivial branch wiring.
    // `--offline` and `--live` cover the other two modes end-to-end.

    #[test]
    fn quick_checks_never_fail_on_a_good_dynamic_config() {
        let dir = tempfile::tempdir().unwrap();
        let (_, version, state) = no_env();
        // The dynamic branch smokes /v1/models, so the router config must
        // point at a stub that answers (a dead default 8787 would FAIL).
        let (base, port) = spawn_models_stub(200, &catalog_body(&["x/model-a"]));
        let checks = offline_checks_with_env(
            &write_config_on_port(dir.path(), port),
            None,
            Some(&dynamic_codex_toml(&base)),
            version,
            &state,
        );
        assert!(!report_has_fail(&checks), "{checks:?}");
        let cfg = checks.iter().find(|c| c.name == "config loads").unwrap();
        assert!(matches!(cfg.status, CheckStatus::Pass), "{cfg:?}");
        let mode = checks.iter().find(|c| c.name == "detected mode").unwrap();
        assert!(mode.detail.contains("Dynamic"), "{mode:?}");
    }

    #[test]
    fn broken_config_fails_config_load_check() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "not toml [").unwrap();
        let (toml, version, state) = no_env();
        let checks = offline_checks_with_env(&p, None, toml, version, &state);
        assert!(report_has_fail(&checks));
        let cfg = checks.iter().find(|c| c.name == "config loads").unwrap();
        assert!(matches!(cfg.status, CheckStatus::Fail), "{cfg:?}");
    }

    /// L1.2: a valid config with no routes generates an empty catalog —
    /// doctor must fail on "router has routes".
    #[test]
    fn empty_router_fails_router_has_routes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            r#"
[providers.x]
base_url = "https://x.com/v1"
api_key = "k"
format = "chat"
"#,
        )
        .unwrap();
        let (toml, version, state) = no_env();
        let checks = offline_checks_with_env(&p, None, toml, version, &state);
        let routes = checks
            .iter()
            .find(|c| c.name == "router has routes")
            .unwrap();
        assert!(matches!(routes.status, CheckStatus::Fail), "{routes:?}");
        assert!(report_has_fail(&checks), "{checks:?}");
    }

    /// L1.4 is emitted through the real wrapper, with the active provider
    /// resolved from `codex_toml`'s top-level `model_provider`.
    #[test]
    fn detected_mode_names_mode_and_active_provider() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"
model = "x/y"
model_provider = "alt"
model_catalog_json = "/tmp/pinned.json"

[model_providers.codexferry]
base_url = "http://127.0.0.1:8787/v1"
env_key = "DUMMY"
[model_providers.alt]
base_url = "http://127.0.0.1:8787/v1"
[model_providers.alt.auth]
command = "echo"
args = ["dummy"]
"#;
        let checks = offline_checks_with_env(
            &write_config(dir.path()),
            None,
            Some(toml),
            None,
            &DoctorState::default(),
        );
        let mode = checks.iter().find(|c| c.name == "detected mode").unwrap();
        assert_eq!(mode.status, CheckStatus::Info, "{mode:?}");
        assert!(mode.detail.contains("Pinned"), "{mode:?}");
        assert!(mode.detail.contains("active provider: alt"), "{mode:?}");
    }

    /// With a non-default `model_provider`, detect_mode must branch on THAT
    /// provider's auth block, not the default one.
    #[test]
    fn detected_mode_uses_active_provider_auth() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"
model = "x/y"
model_provider = "alt"

[model_providers.codexferry]
base_url = "http://127.0.0.1:8787/v1"
env_key = "DUMMY"
[model_providers.alt]
base_url = "http://127.0.0.1:8787/v1"
[model_providers.alt.auth]
command = "echo"
args = ["dummy"]
"#;
        let checks = offline_checks_with_env(
            &write_config(dir.path()),
            None,
            Some(toml),
            None,
            &DoctorState::default(),
        );
        let mode = checks.iter().find(|c| c.name == "detected mode").unwrap();
        assert!(mode.detail.contains("Dynamic"), "{mode:?}");
        assert!(mode.detail.contains("active provider: alt"), "{mode:?}");
    }

    // ---- mode_keyed_check (spec §Mode-specific shadow_check replacement) ----

    const PIN_TOML: &str = r#"
model_catalog_json = "/tmp/pinned.json"

[model_providers.codexferry]
base_url = "http://127.0.0.1:8787/v1"
"#;

    /// Pin + `auth.command` on a provider — the L2.7' trigger (a mixed
    /// static/dynamic wiring that `detect_mode` classifies as Pinned).
    const MIXED_PIN_TOML: &str = r#"
model_catalog_json = "/tmp/pinned.json"

[model_providers.codexferry]
base_url = "http://127.0.0.1:8787/v1"
[model_providers.codexferry.auth]
command = "echo"
args = ["dummy"]
"#;

    #[test]
    fn pinned_pin_confirms_static_catalog() {
        let c = mode_keyed_check(Some(PIN_TOML), Mode::Pinned).unwrap();
        assert!(matches!(c.status, CheckStatus::Info), "{c:?}");
        assert_eq!(c.name, "static catalog pin");
        assert!(c.detail.contains("/tmp/pinned.json"), "{c:?}");
        assert!(c.detail.contains("gen-catalog"), "{c:?}");
        // The pinned-mode message must not tell the user to remove the pin.
        assert!(!c.detail.contains("remove"), "{c:?}");
    }

    #[test]
    fn fallback_pin_confirms_only_metadata_source() {
        let c = mode_keyed_check(Some(PIN_TOML), Mode::Fallback).unwrap();
        assert!(matches!(c.status, CheckStatus::Info), "{c:?}");
        assert_eq!(c.name, "static catalog pin");
        assert!(c.detail.contains("fallback"), "{c:?}");
    }

    /// L2.7'': fallback with no pin and no auth.command is degraded — WARN,
    /// never FAIL.
    #[test]
    fn fallback_without_pin_warns_degraded_wiring() {
        let toml = r#"
[model_providers.codexferry]
base_url = "http://127.0.0.1:8787/v1"
env_key = "DUMMY"
"#;
        let c = mode_keyed_check(Some(toml), Mode::Fallback).unwrap();
        assert!(matches!(c.status, CheckStatus::Warn), "{c:?}");
        assert_eq!(c.name, "fallback wiring");
        assert!(c.detail.contains("fallback"), "{c:?}");
        assert!(!report_has_fail(&[c]));
    }

    #[test]
    fn dynamic_without_pin_emits_nothing() {
        assert!(mode_keyed_check(Some(DYNAMIC_CODEX_TOML), Mode::Dynamic).is_none());
    }

    /// The pin-shadow arm moved to [`pin_shadow_warn`];
    /// `mode_keyed_check` must NOT emit it here, or
    /// `offline_checks_with_mode`'s Dynamic branch would double-report.
    #[test]
    fn mode_keyed_check_never_emits_the_dynamic_pin_warn() {
        assert!(mode_keyed_check(Some(PIN_TOML), Mode::Dynamic).is_none());
    }

    #[test]
    fn pinned_without_pin_emits_nothing() {
        let toml = r#"
[model_providers.codexferry]
base_url = "http://127.0.0.1:8787/v1"
"#;
        assert!(mode_keyed_check(Some(toml), Mode::Pinned).is_none());
    }

    /// Missing or unparseable `codex_toml` never produces a check (the
    /// fallback WARN needs a real config to inspect, otherwise it would
    /// fire on every machine without `~/.codex/`).
    #[test]
    fn mode_keyed_check_skips_absent_or_unparseable_toml() {
        assert!(mode_keyed_check(None, Mode::Dynamic).is_none());
        assert!(mode_keyed_check(None, Mode::Fallback).is_none());
        assert!(mode_keyed_check(Some("not toml [[["), Mode::Fallback).is_none());
    }

    // ---- pin_shadow_warn (spec L2.7') ----

    /// L2.7': pin + `auth.command` coexisting is a shadow — WARN, and a WARN
    /// never fails. The trigger is the two keys, not the detected mode: a
    /// string pin makes `detect_mode` say Pinned, which is exactly the case
    /// this test drives.
    #[test]
    fn dynamic_pin_warns_pin_shadows_live_fetch() {
        let c = pin_shadow_warn(Some(MIXED_PIN_TOML)).unwrap();
        assert!(matches!(c.status, CheckStatus::Warn), "{c:?}");
        assert_eq!(c.name, "pin shadows live fetch");
        assert!(c.detail.contains("shadows"), "{c:?}");
        assert!(c.detail.contains("/tmp/pinned.json"), "{c:?}");
        assert!(c.detail.contains("remove the pin"), "{c:?}");
        // WARN must not fail the run.
        assert!(!report_has_fail(&[c]));
    }

    /// A pin without any provider `auth.command` is not a shadow: the live
    /// fetch would not be enabled even without the pin, so there is nothing
    /// to warn about.
    #[test]
    fn pin_shadow_warn_emits_nothing_with_pin_but_no_auth_command() {
        assert!(pin_shadow_warn(Some(PIN_TOML)).is_none());
    }

    #[test]
    fn pin_shadow_warn_emits_nothing_without_a_pin() {
        assert!(pin_shadow_warn(Some(DYNAMIC_CODEX_TOML)).is_none());
    }

    #[test]
    fn pin_shadow_warn_skips_absent_or_unparseable_toml() {
        assert!(pin_shadow_warn(None).is_none());
        assert!(pin_shadow_warn(Some("not toml [[[")).is_none());
    }

    /// A non-string pin is still a pin (it forces the static manager); the
    /// detail names the fallback instead of a path. Requires a provider
    /// `auth.command` alongside, per the both-keys trigger.
    #[test]
    fn non_string_pin_is_still_a_pin() {
        let c = pin_shadow_warn(Some(
            "model_catalog_json = 123\n\
             [model_providers.codexferry]\n\
             base_url = \"http://127.0.0.1:8787/v1\"\n\
             [model_providers.codexferry.auth]\n\
             command = \"echo\"\n",
        ))
        .unwrap();
        assert!(matches!(c.status, CheckStatus::Warn), "{c:?}");
        assert!(c.detail.contains("<not a string>"), "{c:?}");
    }

    // ---- wiring_check (spec L1.3) ----

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

    /// A provider pointing at the right host but the WRONG port is not a
    /// match: the old `contains("127.0.0.1")` heuristic called this green.
    #[test]
    fn wiring_check_does_not_match_a_different_port() {
        let toml = r#"
[model_providers.codexferry]
base_url = "http://127.0.0.1:9999/v1"
"#;
        let c = wiring_check(Some(toml), "http://127.0.0.1:8787/v1");
        assert!(matches!(c.status, CheckStatus::Info), "{c:?}");
    }

    // ---- version_status ----

    // ---- version_age (spec L2.6) ----

    #[test]
    fn version_age_skips_when_no_codex_version() {
        assert!(version_age(None).is_none());
    }

    /// With `LAST_VERIFIED` empty, any installed codex — raw `codex --version`
    /// output included — is unverified, so the INFO fires with the "not been
    /// verified" wording.
    #[test]
    fn version_age_infos_when_version_has_not_been_verified() {
        let c = version_age(Some("codex-cli 0.147.0")).expect("a check");
        assert_eq!(c.name, "codex version age");
        assert!(matches!(c.status, CheckStatus::Info), "{c:?}");
        assert!(c.detail.contains("0.147.0"), "{c:?}");
        assert!(c.detail.contains("not been verified"), "{c:?}");
        assert!(c.detail.contains("doctor --live"), "{c:?}");
    }

    /// A version string with no digit token has nothing to normalize, so the
    /// check is skipped instead of comparing garbage.
    #[test]
    fn version_age_skips_when_version_is_unparseable() {
        assert!(version_age(Some("no digits here")).is_none());
        assert!(version_age(Some("   ")).is_none());
    }

    /// Composition: the same dynamic-mode run, with and without a codex
    /// version, gains/loses ONLY the version-age line — the other checks keep
    /// their statuses. The with-version half is asserted thoroughly in
    /// [`quick_checks_report_all_common_checks_in_a_wired_dynamic_environment`];
    /// this test pins the without-version half and the ordering of the two
    /// version lines (age before status).
    #[test]
    fn version_age_presence_follows_the_codex_version_only() {
        let dir = tempfile::tempdir().unwrap();
        let (base, port) = spawn_models_stub(200, &catalog_body(&["x/model-a"]));
        let config_path = write_config_on_port(dir.path(), port);
        let codex_toml = dynamic_codex_toml(&base);
        let without = offline_checks_with_mode(
            &config_path,
            None,
            Mode::Dynamic,
            "codexferry",
            Some(&codex_toml),
            None,
            &DoctorState::default(),
        );
        assert!(
            without.iter().all(|c| c.name != "codex version age"),
            "{without:?}"
        );
        let status = without
            .iter()
            .find(|c| c.name == "codex version status")
            .expect("version status always present");
        assert!(matches!(status.status, CheckStatus::Info), "{status:?}");

        let with = offline_checks_with_mode(
            &config_path,
            None,
            Mode::Dynamic,
            "codexferry",
            Some(&codex_toml),
            Some("codex-cli 0.147.0".into()),
            &DoctorState::default(),
        );
        assert_eq!(with.len(), without.len() + 1, "{without:?} {with:?}");
        let age = with
            .iter()
            .find(|c| c.name == "codex version age")
            .expect("version age present with a codex version");
        assert!(matches!(age.status, CheckStatus::Info), "{age:?}");
        let age_pos = with.iter().position(|c| c.name == "codex version age");
        let status_pos = with.iter().position(|c| c.name == "codex version status");
        assert!(age_pos < status_pos, "age must precede status in {with:?}");
        // Every non-version check keeps the same name+status in both runs.
        for c in &without {
            if c.name == "codex version status" {
                continue;
            }
            let w = with
                .iter()
                .find(|o| o.name == c.name)
                .unwrap_or_else(|| panic!("check {:?} missing in {with:?}", c.name));
            assert_eq!(w.status, c.status, "check {:?}", c.name);
        }
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
        assert!(
            matches!(unverified.status, CheckStatus::Info),
            "{unverified:?}"
        );
        let never = DoctorState::default();
        assert!(matches!(
            version_status(Some("0.160.0".into()), &never).status,
            CheckStatus::Info
        ));
        assert!(matches!(
            version_status(None, &never).status,
            CheckStatus::Info
        ));
        assert!(matches!(
            version_status(Some("no digits".into()), &never).status,
            CheckStatus::Info
        ));
    }

    /// The false-green guard (spec §3.2): an unparseable observed version and
    /// an unparseable `last_green` must NOT compare equal. Comparing the two
    /// `Option`s directly would report PASS here, because `None == None`.
    #[test]
    fn unparseable_on_both_sides_is_never_a_pass() {
        let state = DoctorState {
            last_green: Some("no digits".into()),
            last_run: None,
        };
        let c = version_status(Some("also no digits".into()), &state);
        assert!(matches!(c.status, CheckStatus::Info), "{c:?}");
    }

    /// `last_green` is normalized on the state side too, so a state file
    /// holding raw `codex --version` output still compares equal.
    #[test]
    fn version_status_normalizes_both_sides() {
        let state = DoctorState {
            last_green: Some("codex-cli 0.158.0".into()),
            last_run: None,
        };
        let c = version_status(Some("codex-cli 0.158.0".into()), &state);
        assert!(matches!(c.status, CheckStatus::Pass), "{c:?}");
    }

    /// Composition with a POPULATED environment: every common check is
    /// present, by name and by status. The no-environment tests cannot see
    /// checks disappear behind INFO-skips, so deleting a `checks.push(...)`
    /// would go unnoticed without this.
    ///
    /// Every environment input is injected: the test never reads the
    /// developer's real `~/.codex/config.toml` or doctor state file, and
    /// never spawns `codex`.
    ///
    /// Dynamic mode also smokes `/v1/models`, so the router config points at
    /// a stub that answers — a dead endpoint would (correctly) FAIL, and the
    /// old hardcoded default 8787 is dead on CI.
    #[test]
    fn quick_checks_report_all_common_checks_in_a_wired_dynamic_environment() {
        let dir = tempfile::tempdir().unwrap();
        let (base, port) = spawn_models_stub(200, &catalog_body(&["x/model-a"]));
        let config_path = write_config_on_port(dir.path(), port);
        let state = DoctorState {
            last_green: Some("0.158.0".into()),
            last_run: None,
        };
        let checks = offline_checks_with_env(
            &config_path,
            None,
            Some(&dynamic_codex_toml(&base)),
            Some("codex-cli 0.158.0".into()),
            &state,
        );
        assert!(!report_has_fail(&checks), "{checks:?}");
        assert_eq!(checks.len(), 9, "{checks:?}");
        for (name, want) in [
            ("config loads", CheckStatus::Pass),
            ("router has routes", CheckStatus::Pass),
            ("template fields dropped", CheckStatus::Info),
            ("detected mode", CheckStatus::Info),
            ("codex wiring", CheckStatus::Pass),
            ("models endpoint reachable", CheckStatus::Pass),
            ("models endpoint shape", CheckStatus::Pass),
            ("codex version age", CheckStatus::Info),
            ("codex version status", CheckStatus::Pass),
        ] {
            let c = checks
                .iter()
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("check {name:?} missing from {checks:?}"));
            assert_eq!(c.status, want, "check {name:?} in {checks:?}");
        }
    }

    // ---- dynamic endpoint checks (spec L2.8'/L2.9') ----

    /// A serialized `{"models": [...]}` body with one complete entry per
    /// slug (a valid stub response for the endpoint checks; see
    /// [`pin_entry`] for the presence-only field values).
    fn catalog_body(slugs: &[&str]) -> String {
        serde_json::to_string(&good_pin(slugs)).unwrap()
    }

    /// Reserve a free loopback port with nothing listening, for the
    /// connect-failure tests.
    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind for free port")
            .local_addr()
            .expect("local addr")
            .port()
    }

    /// Spawn a stub axum server answering `GET /v1/models?client_version=probe`
    /// (the exact fetch the endpoint checks make) on a fresh OS thread with
    /// its own tokio current-thread runtime — the same thread+runtime pattern
    /// the production checks use. Serves `(status, body)` for every request,
    /// or 400 when the probe query is absent, so a wrong URL fails loudly.
    /// Returns `(base_url, port)`; the thread lives until the test process
    /// exits.
    fn spawn_models_stub(status: u16, body: &str) -> (String, u16) {
        let body = body.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let app = axum::Router::new().route(
                    "/v1/models",
                    axum::routing::get(
                        move |axum::extract::Query(query): axum::extract::Query<
                            std::collections::HashMap<String, String>,
                        >| async move {
                            if query.get("client_version").map(String::as_str) != Some("probe") {
                                return (
                                    axum::http::StatusCode::BAD_REQUEST,
                                    "expected client_version=probe".to_string(),
                                );
                            }
                            (
                                axum::http::StatusCode::from_u16(status).unwrap(),
                                body.clone(),
                            )
                        },
                    ),
                );
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let _ = tx.send((format!("http://127.0.0.1:{}/v1", addr.port()), addr.port()));
                axum::serve(listener, app).await.unwrap();
            });
        });
        rx.recv().unwrap()
    }

    #[test]
    fn models_endpoint_reachable_passes_on_200() {
        let (base, _) = spawn_models_stub(200, &catalog_body(&["x/a"]));
        let c = models_endpoint_reachable(&fetch_models(&base));
        assert_eq!(c.name, "models endpoint reachable");
        assert_eq!(c.status, CheckStatus::Pass, "{c:?}");
        assert!(c.detail.contains("200"), "{c:?}");
    }

    #[test]
    fn models_endpoint_reachable_fails_on_500() {
        let (base, _) = spawn_models_stub(500, "unavailable");
        let c = models_endpoint_reachable(&fetch_models(&base));
        assert_eq!(c.status, CheckStatus::Fail, "{c:?}");
        assert!(c.detail.contains("500"), "{c:?}");
        assert!(
            c.detail.contains("CatalogCache::get"),
            "FAIL must carry the remediation: {c:?}"
        );
    }

    /// A dynamic-mode offline run against a dead endpoint MUST produce a
    /// reachability FAIL (connect refusal surfaces as the error branch, not
    /// a timeout hang).
    #[test]
    fn models_endpoint_reachable_fails_when_nothing_is_listening() {
        let base = format!("http://127.0.0.1:{}/v1", free_port());
        let c = models_endpoint_reachable(&fetch_models(&base));
        assert_eq!(c.status, CheckStatus::Fail, "{c:?}");
    }

    #[test]
    fn models_endpoint_shape_passes_on_a_valid_catalog() {
        let (base, _) = spawn_models_stub(200, &catalog_body(&["x/a", "x/b"]));
        let c = models_endpoint_shape(&fetch_models(&base));
        assert_eq!(c.name, "models endpoint shape");
        assert_eq!(c.status, CheckStatus::Pass, "{c:?}");
        assert!(c.detail.contains("all 2 entries"), "{c:?}");
    }

    /// L2.9': a body missing any required field fails with that field named.
    #[test]
    fn models_endpoint_shape_fails_on_a_missing_required_field() {
        let mut pin = good_pin(&["x/a"]);
        pin["models"][0]
            .as_object_mut()
            .unwrap()
            .remove("visibility");
        let (base, _) = spawn_models_stub(200, &serde_json::to_string(&pin).unwrap());
        let c = models_endpoint_shape(&fetch_models(&base));
        assert_eq!(c.status, CheckStatus::Fail, "{c:?}");
        assert!(c.detail.contains("visibility"), "{c:?}");
    }

    /// The parse FAIL must embed the serde error, not just say "not valid
    /// JSON" (so the user can see why).
    #[test]
    fn models_endpoint_shape_fails_when_body_is_not_catalog_json() {
        for body in ["not json", r#"{"foo":"bar"}"#, r#"{"models":"nope"}"#] {
            let (base, _) = spawn_models_stub(200, body);
            let c = models_endpoint_shape(&fetch_models(&base));
            assert_eq!(c.status, CheckStatus::Fail, "body {body:?}: {c:?}");
        }
        // Serde-error embedding: the malformed-JSON case must name the error.
        let (base, _) = spawn_models_stub(200, "not json");
        let c = models_endpoint_shape(&fetch_models(&base));
        assert!(c.detail.contains("not valid JSON:"), "{c:?}");
    }

    #[test]
    fn models_endpoint_shape_fails_when_the_endpoint_is_down() {
        let base = format!("http://127.0.0.1:{}/v1", free_port());
        let c = models_endpoint_shape(&fetch_models(&base));
        assert_eq!(c.status, CheckStatus::Fail, "{c:?}");
    }

    // ---- dynamic-mode composition (spec L2.7'-L2.9') ----

    /// Composition through `offline_checks_with_mode`: a dynamic-mode config
    /// whose `[server] port` is the stub's port, with a codex TOML carrying
    /// BOTH a pin and `auth.command`, must emit the pin-shadow WARN and pass
    /// both endpoint checks against the live stub.
    #[test]
    fn dynamic_mode_reports_pin_shadow_warn_and_passes_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let (base, port) = spawn_models_stub(200, &catalog_body(&["x/model-a"]));
        let config_path = write_config_on_port(dir.path(), port);
        let codex_toml = format!(
            "model_catalog_json = \"/tmp/pinned.json\"\n{}",
            dynamic_codex_toml(&base)
        );
        let checks = offline_checks_with_mode(
            &config_path,
            None,
            Mode::Dynamic,
            "codexferry",
            Some(&codex_toml),
            None,
            &DoctorState::default(),
        );
        assert!(!report_has_fail(&checks), "{checks:?}");
        // Exactly one shadow WARN per run (the Pinned and Dynamic branches
        // both call pin_shadow_warn, but only one branch executes per mode).
        let shadows: Vec<&Check> = checks
            .iter()
            .filter(|c| c.name == "pin shadows live fetch")
            .collect();
        assert_eq!(shadows.len(), 1, "{checks:?}");
        assert!(matches!(shadows[0].status, CheckStatus::Warn), "{checks:?}");
        assert!(shadows[0].detail.contains("remove the pin"), "{checks:?}");
        for name in ["models endpoint reachable", "models endpoint shape"] {
            let c = checks
                .iter()
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("missing {name:?} in {checks:?}"));
            assert_eq!(c.status, CheckStatus::Pass, "check {name:?}: {c:?}");
        }
    }

    /// Composition: a dynamic-mode offline run against a 500-ing endpoint
    /// must FAIL the reachability check (and the run), as codex itself would
    /// be unable to fetch the catalog.
    #[test]
    fn dynamic_mode_offline_run_fails_when_endpoint_returns_500() {
        let dir = tempfile::tempdir().unwrap();
        let (base, port) = spawn_models_stub(500, "boom");
        let config_path = write_config_on_port(dir.path(), port);
        let codex_toml = dynamic_codex_toml(&base);
        let checks = offline_checks_with_mode(
            &config_path,
            None,
            Mode::Dynamic,
            "codexferry",
            Some(&codex_toml),
            None,
            &DoctorState::default(),
        );
        let reachable = checks
            .iter()
            .find(|c| c.name == "models endpoint reachable")
            .unwrap_or_else(|| panic!("missing reachable check in {checks:?}"));
        assert_eq!(reachable.status, CheckStatus::Fail, "{reachable:?}");
        assert!(report_has_fail(&checks), "{checks:?}");
    }

    /// L2.7' must be reachable in PINNED mode too: a string pin +
    /// `auth.command` is classified Pinned by `detect_mode`, so a
    /// Dynamic-only shadow call would never fire. The WARN appears alongside
    /// the pinned checks, which still run and pass against a valid pin file.
    #[test]
    fn pinned_mode_with_a_mixed_pin_and_auth_warns_shadow_while_pinned_checks_pass() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config(dir.path());
        let pin_path = write_pin(dir.path(), "catalog.json", &good_pin(&["x/model-a"]));
        let codex_toml = format!(
            "model_catalog_json = \"{}\"\n{}",
            pin_path.display(),
            dynamic_codex_toml("http://127.0.0.1:8787/v1")
        );
        let checks = offline_checks_with_mode(
            &config_path,
            None,
            Mode::Pinned,
            "codexferry",
            Some(&codex_toml),
            None,
            &DoctorState::default(),
        );
        assert!(!report_has_fail(&checks), "{checks:?}");
        // One shadow WARN, no duplicate.
        let shadows: Vec<&Check> = checks
            .iter()
            .filter(|c| c.name == "pin shadows live fetch")
            .collect();
        assert_eq!(shadows.len(), 1, "{checks:?}");
        assert!(matches!(shadows[0].status, CheckStatus::Warn), "{checks:?}");
        assert!(shadows[0].detail.contains("remove the pin"), "{checks:?}");
        // The pinned-mode checks still run and pass alongside the WARN.
        for name in [
            "pin exists and parses",
            "pin covers router",
            "pin matches router",
            "pin entry shape",
        ] {
            let c = checks
                .iter()
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("check {name:?} missing from {checks:?}"));
            assert_eq!(c.status, CheckStatus::Pass, "check {name:?}: {c:?}");
        }
    }

    // ---- pinned-mode checks (spec L2.7-L2.10) ----

    /// Build a validated router with one `x/<slug>` route per given slug.
    /// The empty case is a valid config with no routes, so the degenerate
    /// reconciliation tests can drive it without hand-writing TOML.
    fn validated_router_with(slugs: &[&str]) -> crate::config::ValidatedConfig {
        let routes = slugs
            .iter()
            .map(|slug| format!("{slug:?} = {{ model = {slug:?}, context_window = 131072 }}"))
            .collect::<Vec<_>>()
            .join("\n");
        let config = format!(
            "[providers.x]\nbase_url = \"https://x.com/v1\"\napi_key = \"k\"\nformat = \"chat\"\n[routes]\n{routes}"
        );
        crate::config::Config::parse_str(&config)
            .unwrap()
            .validate()
            .unwrap()
    }

    /// One fully-shaped pin entry: every field codex >= 0.147's `ModelInfo`
    /// requires, each with a presence-only stand-in value. The field list is
    /// written literally (rather than derived from `REQUIRED_FIELDS`) so a
    /// drift between the two still fails the shape test. Values deliberately
    /// do not mirror real gen-catalog output byte-for-byte — e.g. here
    /// `supported_reasoning_levels` is a flat string array where the real
    /// generator emits effort/description objects — because the shape check
    /// tests field presence, not field values.
    fn pin_entry(slug: &str) -> serde_json::Value {
        serde_json::json!({
            "slug": slug,
            "display_name": slug,
            "supported_reasoning_levels": ["minimal", "low", "medium", "high", "max"],
            "shell_type": "default",
            "visibility": "list",
            "supported_in_api": true,
            "priority": 99,
            "support_verbosity": false,
            "truncation_policy": { "mode": "tokens", "limit": 10000 },
            "experimental_supported_tools": [],
        })
    }

    /// A `{"models": [...]}` pin with one complete entry per slug: the same
    /// top-level shape `codexferry gen-catalog` writes, using the
    /// presence-only [`pin_entry`] fixture.
    fn good_pin(slugs: &[&str]) -> serde_json::Value {
        let models: Vec<serde_json::Value> = slugs.iter().map(|s| pin_entry(s)).collect();
        serde_json::json!({ "models": models })
    }

    /// Write a pin file into `dir` and return its path (all pin tests live
    /// in a tempdir; none touch the real `~/.codex/`).
    fn write_pin(dir: &std::path::Path, name: &str, pin: &serde_json::Value) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, serde_json::to_string_pretty(pin).unwrap()).unwrap();
        path
    }

    #[test]
    fn pin_file_check_parses_a_valid_pin() {
        let pin = pin_file_check(r#"{"models":[{"slug":"x/a"}]}"#).unwrap();
        assert_eq!(pin["models"][0]["slug"], "x/a");
    }

    #[test]
    fn pin_file_check_rejects_malformed_json() {
        assert!(pin_file_check("not json").is_err());
    }

    #[test]
    fn pin_file_check_rejects_a_missing_or_non_array_models_key() {
        assert!(pin_file_check("{}").is_err());
        assert!(pin_file_check(r#"{"models": "nope"}"#).is_err());
    }

    #[test]
    fn pin_covers_router_passes_when_every_route_is_pinned() {
        let router = validated_router_with(&["x/a", "x/b"]);
        let pin = good_pin(&["x/a", "x/b"]);
        let c = pin_covers_router(&router, &pin);
        assert_eq!(c.name, "pin covers router");
        assert_eq!(c.status, CheckStatus::Pass, "{c:?}");
    }

    #[test]
    fn pin_covers_router_fails_listing_routes_missing_from_the_pin() {
        let router = validated_router_with(&["x/a", "x/b"]);
        let pin = good_pin(&["x/a"]);
        let c = pin_covers_router(&router, &pin);
        assert_eq!(c.status, CheckStatus::Fail, "{c:?}");
        assert!(c.detail.contains("x/b"), "{c:?}");
        assert!(c.detail.contains("gen-catalog"), "{c:?}");
    }

    #[test]
    fn pin_matches_router_passes_when_every_pin_slug_is_a_route() {
        let router = validated_router_with(&["x/a", "x/b"]);
        let pin = good_pin(&["x/a", "x/b"]);
        let c = pin_matches_router(&router, &pin);
        assert_eq!(c.name, "pin matches router");
        assert_eq!(c.status, CheckStatus::Pass, "{c:?}");
    }

    #[test]
    fn pin_matches_router_fails_listing_orphan_slugs() {
        let router = validated_router_with(&["x/a"]);
        let pin = good_pin(&["x/a", "x/c"]);
        let c = pin_matches_router(&router, &pin);
        assert_eq!(c.status, CheckStatus::Fail, "{c:?}");
        assert!(c.detail.contains("x/c"), "{c:?}");
        assert!(c.detail.contains("gen-catalog"), "{c:?}");
    }

    /// Degenerate but consistent: with nothing on either side there is
    /// nothing to reconcile, so both directions pass.
    #[test]
    fn empty_router_and_empty_pin_reconcile_cleanly() {
        let router = validated_router_with(&[]);
        let pin = good_pin(&[]);
        let covers = pin_covers_router(&router, &pin);
        let matches = pin_matches_router(&router, &pin);
        assert_eq!(covers.status, CheckStatus::Pass, "{covers:?}");
        assert_eq!(matches.status, CheckStatus::Pass, "{matches:?}");
    }

    #[test]
    fn pin_field_shape_passes_when_every_entry_is_complete() {
        let checks = pin_field_shape(&good_pin(&["x/a", "x/b"]));
        assert_eq!(checks.len(), 1, "{checks:?}");
        assert_eq!(checks[0].name, "pin entry shape");
        assert_eq!(checks[0].status, CheckStatus::Pass, "{checks:?}");
    }

    /// L2.10: remove each required field in turn; the FAIL must name that
    /// field and the offending entry's slug (an entry missing `slug` itself
    /// has no slug to name, so it is identified as `<missing slug>`).
    #[test]
    fn pin_field_shape_fails_naming_each_missing_required_field() {
        for field in REQUIRED_FIELDS {
            let mut pin = good_pin(&["x/a"]);
            pin["models"][0].as_object_mut().unwrap().remove(*field);
            let checks = pin_field_shape(&pin);
            assert_eq!(checks.len(), 1, "field {field:?}: {checks:?}");
            let c = &checks[0];
            assert_eq!(c.name, "pin entry shape", "field {field:?}: {c:?}");
            assert_eq!(c.status, CheckStatus::Fail, "field {field:?}: {c:?}");
            assert!(c.detail.contains(field), "field {field:?}: {c:?}");
            if *field == "slug" {
                assert!(
                    c.detail.contains("<missing slug>"),
                    "field {field:?}: {c:?}"
                );
            } else {
                assert!(c.detail.contains("x/a"), "field {field:?}: {c:?}");
            }
        }
    }

    #[test]
    fn pin_field_shape_names_every_missing_field_of_an_entry() {
        let mut pin = good_pin(&["x/a"]);
        let entry = pin["models"][0].as_object_mut().unwrap();
        entry.remove("display_name");
        entry.remove("priority");
        let checks = pin_field_shape(&pin);
        assert_eq!(checks.len(), 1, "{checks:?}");
        assert!(checks[0].detail.contains("display_name"), "{checks:?}");
        assert!(checks[0].detail.contains("priority"), "{checks:?}");
        assert!(checks[0].detail.contains("x/a"), "{checks:?}");
    }

    /// The `Vec` mechanism: one FAIL per offending entry, and no pass line
    /// mixed in when only some entries are bad.
    #[test]
    fn pin_field_shape_emits_one_fail_per_offending_entry() {
        let mut pin = good_pin(&["x/a", "x/b", "x/c"]);
        pin["models"][1]
            .as_object_mut()
            .unwrap()
            .remove("visibility");
        pin["models"][2].as_object_mut().unwrap().remove("priority");
        let checks = pin_field_shape(&pin);
        assert_eq!(checks.len(), 2, "{checks:?}");
        assert!(
            checks.iter().all(|c| c.status == CheckStatus::Fail),
            "{checks:?}"
        );
    }

    /// A present-but-non-string `slug` must not false-pass the shape check:
    /// codex's `ModelInfo` deserialization rejects it at session start even
    /// though every other required field is present. The FAIL names the
    /// `slug` field and identifies the entry as `<non-string slug>`.
    #[test]
    fn pin_field_shape_flags_a_non_string_slug_instead_of_false_passing() {
        let mut pin = good_pin(&["x/a"]);
        pin["models"][0]
            .as_object_mut()
            .unwrap()
            .insert("slug".into(), serde_json::json!(123));
        let checks = pin_field_shape(&pin);
        assert_eq!(checks.len(), 1, "{checks:?}");
        let c = &checks[0];
        assert_eq!(c.name, "pin entry shape");
        assert_eq!(c.status, CheckStatus::Fail, "{c:?}");
        assert!(c.detail.contains("slug"), "{c:?}");
        assert!(c.detail.contains("<non-string slug>"), "{c:?}");
        assert!(report_has_fail(&checks), "{checks:?}");
    }

    /// A non-object entry is a malformed entry codex would reject outright:
    /// the shape check must FAIL it (labelled `<non-object entry>`), not
    /// silently skip it — same stance as `models_endpoint_shape`, so the two
    /// catalog sources cannot drift.
    #[test]
    fn pin_field_shape_flags_a_non_object_entry() {
        let mut pin = good_pin(&["x/a"]);
        pin["models"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!(42));
        let checks = pin_field_shape(&pin);
        assert_eq!(checks.len(), 1, "{checks:?}");
        let c = &checks[0];
        assert_eq!(c.name, "pin entry shape");
        assert_eq!(c.status, CheckStatus::Fail, "{c:?}");
        assert!(c.detail.contains("<non-object entry>"), "{c:?}");
        assert!(report_has_fail(&checks), "{checks:?}");
    }

    /// Drift guard: the generator and [`REQUIRED_FIELDS`] must stay in
    /// sync, or a codex upgrade adding a required `ModelInfo` field would
    /// go unnoticed. Drive the real generator on a temp config and feed its
    /// output straight into the shape check. An explicit empty template
    /// keeps the test off the host's real `~/.codex/` and off `codex`.
    #[test]
    fn generated_catalog_always_satisfies_pin_field_shape() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config(dir.path());
        let template = dir.path().join("models.json");
        std::fs::write(&template, "{}").unwrap();
        let generated = catalog::generate_catalog(&config_path, Some(&template)).unwrap();
        let checks = pin_field_shape(&generated.catalog);
        assert!(!checks.is_empty(), "shape check emitted nothing");
        assert!(
            checks.iter().all(|c| c.status == CheckStatus::Pass),
            "{checks:?}"
        );
    }

    /// Composition through the real entry point: a pinned config whose pin
    /// matches the router produces all four pinned checks as PASS.
    #[test]
    fn pinned_mode_with_a_matching_pin_passes_the_pinned_checks() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config(dir.path());
        let pin_path = write_pin(dir.path(), "catalog.json", &good_pin(&["x/model-a"]));
        let codex_toml = format!(r#"model_catalog_json = "{}""#, pin_path.display());
        let checks = offline_checks_with_mode(
            &config_path,
            None,
            Mode::Pinned,
            "codexferry",
            Some(&codex_toml),
            None,
            &DoctorState::default(),
        );
        assert!(!report_has_fail(&checks), "{checks:?}");
        for name in [
            "pin exists and parses",
            "pin covers router",
            "pin matches router",
            "pin entry shape",
        ] {
            let c = checks
                .iter()
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("check {name:?} missing from {checks:?}"));
            assert_eq!(c.status, CheckStatus::Pass, "check {name:?}: {c:?}");
        }
    }

    /// L2.8: a stale pin missing a router route fails the coverage check.
    #[test]
    fn pinned_mode_with_a_stale_pin_fails_pin_covers_router() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config(dir.path());
        let pin_path = write_pin(dir.path(), "catalog.json", &good_pin(&["x/other"]));
        let codex_toml = format!(r#"model_catalog_json = "{}""#, pin_path.display());
        let checks = offline_checks_with_mode(
            &config_path,
            None,
            Mode::Pinned,
            "codexferry",
            Some(&codex_toml),
            None,
            &DoctorState::default(),
        );
        let covers = checks
            .iter()
            .find(|c| c.name == "pin covers router")
            .unwrap();
        assert_eq!(covers.status, CheckStatus::Fail, "{covers:?}");
        assert!(covers.detail.contains("x/model-a"), "{covers:?}");
        assert!(report_has_fail(&checks), "{checks:?}");
    }

    /// L2.7: a pin path pointing at a missing file is the pinned mode's
    /// one legitimate doctor FAIL — codex cannot start until it is
    /// regenerated.
    #[test]
    fn pinned_mode_with_an_unreadable_pin_fails_pin_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config(dir.path());
        let pin_path = dir.path().join("missing.json");
        let codex_toml = format!(r#"model_catalog_json = "{}""#, pin_path.display());
        let checks = offline_checks_with_mode(
            &config_path,
            None,
            Mode::Pinned,
            "codexferry",
            Some(&codex_toml),
            None,
            &DoctorState::default(),
        );
        let unreadable = checks.iter().find(|c| c.name == "pin unreadable").unwrap();
        assert_eq!(unreadable.status, CheckStatus::Fail, "{unreadable:?}");
        assert!(unreadable.detail.contains("gen-catalog"), "{unreadable:?}");
        assert!(report_has_fail(&checks), "{checks:?}");
    }

    /// A pin that exists but is not valid JSON is equally unreadable to
    /// codex, so it fails the same L2.7 check.
    #[test]
    fn pinned_mode_with_a_malformed_pin_fails_pin_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config(dir.path());
        let pin_path = dir.path().join("catalog.json");
        std::fs::write(&pin_path, "not json").unwrap();
        let codex_toml = format!(r#"model_catalog_json = "{}""#, pin_path.display());
        let checks = offline_checks_with_mode(
            &config_path,
            None,
            Mode::Pinned,
            "codexferry",
            Some(&codex_toml),
            None,
            &DoctorState::default(),
        );
        let unreadable = checks.iter().find(|c| c.name == "pin unreadable").unwrap();
        assert_eq!(unreadable.status, CheckStatus::Fail, "{unreadable:?}");
        assert!(unreadable.detail.contains("JSON"), "{unreadable:?}");
        assert!(report_has_fail(&checks), "{checks:?}");
    }

    /// Defensive L2.7 branch: `detect_mode` classifies ANY present
    /// `model_catalog_json` (string or non-string) as Pinned, but this test
    /// still drives the pinned path directly — including a missing key — so
    /// the L2.7 pin-unreadable FAIL stays reachable no matter how mode
    /// detection changes.
    #[test]
    fn pinned_mode_without_a_string_pin_key_fails_pin_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config(dir.path());
        for codex_toml in [
            None,
            Some("model_provider = \"codexferry\"\n"),
            Some("model_catalog_json = 123\n"),
        ] {
            let checks = offline_checks_with_mode(
                &config_path,
                None,
                Mode::Pinned,
                "codexferry",
                codex_toml,
                None,
                &DoctorState::default(),
            );
            let unreadable = checks
                .iter()
                .find(|c| c.name == "pin unreadable")
                .unwrap_or_else(|| panic!("missing \"pin unreadable\" in {checks:?}"));
            assert_eq!(unreadable.status, CheckStatus::Fail, "{unreadable:?}");
            assert!(
                unreadable.detail.contains("model_catalog_json"),
                "{unreadable:?}"
            );
            assert!(unreadable.detail.contains("gen-catalog"), "{unreadable:?}");
            assert!(report_has_fail(&checks), "{checks:?}");
        }
    }
}
