//! `doctor` subcommand: codex-upgrade tripwire for the codexferry ↔ Codex
//! contract.
//!
//! The default (non-`--live`) path runs the offline quick-checks from the
//! 2026-08-23 mode-aware design spec (`docs/superpowers/specs/
//! 2026-08-23-mode-aware-doctor-design.md` §Check layers L1/L2): config
//! loads (L1.1), router has routes (L1.2), template dropped-field tripwire,
//! mode classification (L1.4), Codex-side wiring hints (L1.3), the
//! mode-keyed pin/shadow check — the replacement for the old single
//! `shadow_check` (spec §Mode-specific shadow_check replacement + L2.7'/
//! L2.7'') — and version status vs the persisted doctor state. Every
//! environment-dependent input is best-effort: no `~/.codex/config.toml`
//! or no codex on PATH degrades to INFO/WARN, never FAIL.
//!
//! `--live` runs the real-codex wire-shape + tool round-trip probe instead
//! (see [`crate::doctor_live`]).
//!
//! Exit codes: 0 all pass, 1 any FAIL, 2 environment unusable (codex not
//! installed or runnable, live path only). WARN is advisory and never
//! fails the run (spec L2.7'/L2.7''). Infrastructure errors (router bind
//! failure, healthz timeout, probe panic) propagate.

use crate::catalog;
use crate::mode::Mode;
use crate::version::DoctorState;
use std::path::{Path, PathBuf};

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

/// Entry point from `main`. Prints the report; exits 1 on any FAIL (2 is
/// reserved for environment failures, used by the live path).
pub fn run_doctor(
    config_path: &Path,
    codex_models: Option<&Path>,
    live: bool,
) -> anyhow::Result<()> {
    // Live mode: in-process mock upstream + temporary router driving the
    // real Codex CLI (see `doctor_live`). `doctor_live::run` prints the
    // report and raises exit 1 (FAIL) / 2 (environment) itself, preserving
    // the archived "prints + exits" live behavior without a second report
    // here. The offline quick-checks below are skipped entirely.
    if live {
        return crate::doctor_live::run(config_path);
    }
    let checks = offline_checks(config_path, codex_models);
    print_report(&checks);
    if report_has_fail(&checks) {
        std::process::exit(1);
    }
    Ok(())
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
    // Spawning codex happens ONLY here, on the production path.
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
///    §Mode-specific shadow_check replacement).
/// 7. `codex version status`: [`version_status`] vs the persisted state.
///
/// L2.6 (version age) and the pinned/dynamic L2.7–10 checks are not here —
/// they belong to later tasks.
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
///   - `Mode::Dynamic` → WARN "pin shadows live fetch" (L2.7'): the pin
///     forces `StaticModelsManager`, so the live `/v1/models` fetch enabled
///     by `auth.command` never runs — remove the pin OR `auth.command`;
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
    let text = codex_toml?;
    let val = text.parse::<toml::Value>().ok()?;
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
        Mode::Dynamic => (
            "pin shadows live fetch",
            CheckStatus::Warn,
            format!(
                "model_catalog_json = {pin_repr:?} — this pin shadows the live \
                 `/v1/models` fetch enabled by `auth.command` (pin forces \
                 StaticModelsManager, so that fetch never runs); remove the pin \
                 OR remove `auth.command` — pick one mode"
            ),
        ),
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

    /// The environment a machine without `~/.codex/`, without codex on PATH
    /// and without a doctor state file presents. Tests drive the injected
    /// entry points with this so they never read the developer's real
    /// `~/.codex/config.toml` or state file, and never spawn codex.
    fn no_env() -> (Option<&'static str>, Option<String>, DoctorState) {
        (None, None, DoctorState::default())
    }

    #[test]
    fn quick_checks_never_fail_on_a_good_dynamic_config() {
        let dir = tempfile::tempdir().unwrap();
        let (_, version, state) = no_env();
        let checks = offline_checks_with_env(
            &write_config(dir.path()),
            None,
            Some(DYNAMIC_CODEX_TOML),
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

    /// L2.7': dynamic mode + pin is a shadow — WARN, and a WARN never fails.
    #[test]
    fn dynamic_pin_warns_pin_shadows_live_fetch() {
        let c = mode_keyed_check(Some(PIN_TOML), Mode::Dynamic).unwrap();
        assert!(matches!(c.status, CheckStatus::Warn), "{c:?}");
        assert_eq!(c.name, "pin shadows live fetch");
        assert!(c.detail.contains("shadows"), "{c:?}");
        assert!(c.detail.contains("/tmp/pinned.json"), "{c:?}");
        // WARN must not fail the run.
        assert!(!report_has_fail(&[c]));
    }

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

    /// A non-string pin is still a pin (it forces the static manager); the
    /// detail names the fallback instead of a path.
    #[test]
    fn non_string_pin_is_still_a_pin() {
        let c = mode_keyed_check(Some("model_catalog_json = 123\n"), Mode::Dynamic).unwrap();
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
    #[test]
    fn quick_checks_report_all_common_checks_in_a_wired_dynamic_environment() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config(dir.path());
        // CONFIG has no `[server]` section, so the expected Codex-side
        // base_url is the default bind address — DYNAMIC_CODEX_TOML matches.
        let state = DoctorState {
            last_green: Some("0.158.0".into()),
            last_run: None,
        };
        let checks = offline_checks_with_env(
            &config_path,
            None,
            Some(DYNAMIC_CODEX_TOML),
            Some("codex-cli 0.158.0".into()),
            &state,
        );
        assert!(!report_has_fail(&checks), "{checks:?}");
        assert_eq!(checks.len(), 6, "{checks:?}");
        for (name, want) in [
            ("config loads", CheckStatus::Pass),
            ("router has routes", CheckStatus::Pass),
            ("template fields dropped", CheckStatus::Info),
            ("detected mode", CheckStatus::Info),
            ("codex wiring", CheckStatus::Pass),
            ("codex version status", CheckStatus::Pass),
        ] {
            let c = checks
                .iter()
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("check {name:?} missing from {checks:?}"));
            assert_eq!(c.status, want, "check {name:?} in {checks:?}");
        }
    }
}
