//! `doctor` subcommand: upgrade tripwire for the Codex ↔ router contract.
//!
//! Codex CLI auto-updates and occasionally changes its wire dialect (the
//! 2026-08-17 `use_responses_lite`/`additional_tools` incident). Doctor has
//! two modes:
//!
//! - **offline (default)**: regenerate the catalog in memory from the current
//!   config + the installed Codex's bundled template and deep-compare with
//!   the installed catalog. Any drift — a Codex upgrade adding template
//!   fields, a generator policy change, a hand edit — fails with "rerun
//!   gen-catalog". Plus best-effort wiring hints from `~/.codex/config.toml`.
//! - **live (`--live`)**: hosts a mock upstream + a temporary in-process
//!   router and drives the real `codex exec` through both upstream formats,
//!   asserting the normalized wire shape and a full tool round-trip.
//!
//! Exit codes: 0 all pass, 1 any check failed, 2 environment unusable (codex not installed or runnable).
//! Infrastructure errors (router bind failure, healthz timeout, probe panic) propagate

use crate::catalog;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// One numbered line of the doctor report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckStatus {
    Pass,
    Fail,
    Info,
}

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
    catalog: Option<&Path>,
    codex_models: Option<&Path>,
    live: bool,
) -> anyhow::Result<()> {
    // Live mode: in-process mock upstream + temporary router driving the
    // real Codex CLI (see `doctor_live`); exits 1 on FAIL, 2 on a missing
    // environment. The offline catalog checks below are skipped entirely.
    if live {
        return crate::doctor_live::run(config_path);
    }
    let catalog_path = catalog
        .map(PathBuf::from)
        .unwrap_or_else(default_catalog_path);
    let checks = offline_checks(config_path, &catalog_path, codex_models);
    print_report(&checks);
    if report_has_fail(&checks) {
        std::process::exit(1);
    }
    Ok(())
}

/// Default installed-catalog location: `$HOME/.codex/codexferry-catalog.json`.
fn default_catalog_path() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    PathBuf::from(home)
        .join(".codex")
        .join("codexferry-catalog.json")
}

/// Offline checks: readability/shape, regenerate-and-compare, wiring hints.
pub fn offline_checks(
    config_path: &Path,
    catalog_path: &Path,
    codex_models: Option<&Path>,
) -> Vec<Check> {
    let mut checks = Vec::new();

    let installed: Value = match std::fs::read_to_string(catalog_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
    {
        Some(v) => v,
        None => {
            return vec![Check::fail(
                "catalog readable and parseable",
                format!("{} missing or not valid JSON", catalog_path.display()),
            )]
        }
    };
    checks.push(Check::pass(
        "catalog readable and parseable",
        catalog_path.display().to_string(),
    ));

    let generated = match catalog::generate_catalog(config_path, codex_models) {
        Ok(g) => g,
        Err(e) => {
            checks.push(Check::fail("config loads", format!("{e:#}")));
            return checks;
        }
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

    // Regenerate-and-deep-compare: one check covering template drift
    // (Codex upgrade), generator policy change, and hand edits.
    if installed == generated.catalog {
        checks.push(Check::pass(
            "catalog regenerates identically",
            "installed catalog is exactly what gen-catalog would emit",
        ));
    } else {
        checks.push(Check::fail(
            "catalog regenerates identically",
            "stale or drifted (Codex upgrade, generator change, or hand edit) — rerun gen-catalog",
        ));
    }

    // Dropped template fields as INFO: visible without failing, so a Codex
    // upgrade that adds template fields is noticed even when benign.
    if generated.dropped_template_fields.is_empty() {
        checks.push(Check::info("template fields dropped", "none"));
    } else {
        checks.push(Check::info(
            "template fields dropped",
            generated.dropped_template_fields.join(", "),
        ));
    }

    checks.push(codex_wiring_info(catalog_path));
    checks
}

/// Best-effort `~/.codex/config.toml` wiring hint (INFO only, never FAIL).
fn codex_wiring_info(catalog_path: &Path) -> Check {
    // Same HOME → USERPROFILE fallback as `default_catalog_path`.
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    let codex_config = PathBuf::from(home).join(".codex").join("config.toml");
    let Ok(text) = std::fs::read_to_string(&codex_config) else {
        return Check::info("codex wiring", "~/.codex/config.toml not found (skipped)");
    };
    // Guard the degenerate case: a catalog path ending in `/` has no file
    // name, and `""` would match any text (`text.contains("")` is always
    // true), falsely reporting "references this catalog".
    let file_name = catalog_path.file_name().and_then(|n| n.to_str());
    if text.contains("model_catalog_json") {
        if file_name.is_some_and(|f| text.contains(f)) {
            // Spec §3.1 item 3: also verify base_url points at the local proxy.
            let base_info = if text.contains(r#"base_url = "http://127.0.0.1"#)
                || text.contains(r#"base_url = 'http://127.0.0.1"#)
            {
                "base_url -> 127.0.0.1"
            } else {
                "base_url does not point at 127.0.0.1 (may be misconfigured)"
            };
            Check::info(
                "codex wiring",
                format!("model_catalog_json references this catalog; {base_info}"),
            )
        } else {
            Check::info(
                "codex wiring",
                "model_catalog_json set but does not reference this catalog file; base_url not checked",
            )
        }
    } else {
        Check::info(
            "codex wiring",
            "model_catalog_json not set in ~/.codex/config.toml; base_url not checked",
        )
    }
}

/// Whether any check failed.
pub fn report_has_fail(checks: &[Check]) -> bool {
    checks.iter().any(|c| c.status == CheckStatus::Fail)
}

/// Print the numbered PASS/FAIL/INFO report to stdout.
pub fn print_report(checks: &[Check]) {
    for (i, c) in checks.iter().enumerate() {
        let tag = match c.status {
            CheckStatus::Pass => "PASS",
            CheckStatus::Fail => "FAIL",
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

    fn setup(dir: &std::path::Path) {
        std::fs::write(dir.join("config.toml"), CONFIG).unwrap();
    }

    #[test]
    fn fresh_catalog_passes_all_offline_checks() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path());
        let generated =
            crate::catalog::generate_catalog(&dir.path().join("config.toml"), None).unwrap();
        let catalog_path = dir.path().join("codexferry-catalog.json");
        std::fs::write(
            &catalog_path,
            serde_json::to_string_pretty(&generated.catalog).unwrap(),
        )
        .unwrap();
        let checks = offline_checks(&dir.path().join("config.toml"), &catalog_path, None);
        assert!(!report_has_fail(&checks), "{checks:?}");
    }

    #[test]
    fn hand_edited_catalog_fails_equality_check() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path());
        let mut catalog = crate::catalog::generate_catalog(&dir.path().join("config.toml"), None)
            .unwrap()
            .catalog;
        catalog["models"][0]["description"] = serde_json::json!("hand edited");
        let catalog_path = dir.path().join("codexferry-catalog.json");
        std::fs::write(
            &catalog_path,
            serde_json::to_string_pretty(&catalog).unwrap(),
        )
        .unwrap();
        let checks = offline_checks(&dir.path().join("config.toml"), &catalog_path, None);
        assert!(report_has_fail(&checks));
        let eq = checks
            .iter()
            .find(|c| c.name.contains("regenerate"))
            .unwrap();
        assert!(matches!(eq.status, CheckStatus::Fail));
    }

    #[test]
    fn missing_or_unparsable_catalog_fails() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path());
        let checks = offline_checks(
            &dir.path().join("config.toml"),
            &dir.path().join("nope.json"),
            None,
        );
        assert!(report_has_fail(&checks));
    }

    #[test]
    fn broken_config_fails_config_load_check() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "not toml [").unwrap();
        // A parseable catalog is needed so the early return happens on the
        // config-load failure, not on a missing-catalog failure.
        let catalog_path = dir.path().join("codexferry-catalog.json");
        std::fs::write(&catalog_path, r#"{"models": []}"#).unwrap();
        let checks = offline_checks(&dir.path().join("config.toml"), &catalog_path, None);
        assert!(report_has_fail(&checks));
        let cfg = checks.iter().find(|c| c.name == "config loads").unwrap();
        assert!(matches!(cfg.status, CheckStatus::Fail));
    }
}
