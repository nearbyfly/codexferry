//! Detect which catalog wiring codex is using, so doctor can branch its checks.
//!
//! See `docs/superpowers/specs/2026-08-23-mode-aware-doctor-design.md` §Mode detection.

use toml::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `model_catalog_json` present (any value): codex selects the pinned
    /// catalog path (`StaticModelsManager`), `/v1/models` is never
    /// consulted. A non-string pin still classifies as Pinned so doctor's
    /// L2.7 `read_pin` surfaces the malformed key instead of falling back.
    Pinned,
    /// No pin, provider has `[X.auth] command`: codex fetches `/v1/models`
    /// each session start (`OpenAiModelsManager`, `has_command_auth` gate).
    Dynamic,
    /// No pin and no auth command (typically env_key-only): codex falls
    /// through with default metadata (degraded; warn + recommend migration).
    Fallback,
}

/// Default active provider, mirroring codex's default `model_provider`.
pub const DEFAULT_ACTIVE_PROVIDER: &str = "codexferry";

/// Detect the catalog wiring mode from a codex TOML config.
///
/// The caller resolves and passes the active provider (default
/// `DEFAULT_ACTIVE_PROVIDER`). `None` text or a TOML parse failure degrades
/// to `Fallback`; a top-level `model_catalog_json` PRESENT (any value — a
/// TOML table/array/number counts) takes priority and yields `Pinned`,
/// because a non-string pin is still a pin and must surface as doctor's
/// L2.7 "pin unreadable" FAIL rather than a silent fallback; otherwise a
/// string `[model_providers.{active}].auth.command` yields `Dynamic`;
/// anything else is `Fallback`.
pub fn detect_mode(codex_toml_text: Option<&str>, active_provider: &str) -> Mode {
    let Some(text) = codex_toml_text else {
        return Mode::Fallback;
    };
    let Ok(val) = text.parse::<Value>() else {
        return Mode::Fallback;
    };
    if val.get("model_catalog_json").is_some() {
        return Mode::Pinned;
    }
    let provider = val
        .get("model_providers")
        .and_then(Value::as_table)
        .and_then(|t| t.get(active_provider))
        .and_then(Value::as_table);
    if provider
        .and_then(|t| t.get("auth"))
        .and_then(Value::as_table)
        .and_then(|t| t.get("command"))
        .and_then(Value::as_str)
        .is_some()
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
        assert_eq!(
            detect_mode(Some(toml), DEFAULT_ACTIVE_PROVIDER),
            Mode::Pinned
        );
    }

    /// A non-string pin is still a pin (spec §Mode detection: any present
    /// `model_catalog_json` → Pinned): codex's static manager would still be
    /// selected, so doctor must surface the malformed key via its L2.7
    /// "pin unreadable" FAIL instead of silently reporting fallback wiring.
    #[test]
    fn non_string_pin_classifies_pinned_and_auth_only_stays_dynamic() {
        let toml = r#"
            model = "x/y"
            model_provider = "codexferry"
            model_catalog_json = 123

            [model_providers.codexferry]
            base_url = "http://127.0.0.1:8787/v1"
            wire_api = "responses"
            [model_providers.codexferry.auth]
            command = "echo"
            args = ["dummy"]
        "#;
        assert_eq!(
            detect_mode(Some(toml), DEFAULT_ACTIVE_PROVIDER),
            Mode::Pinned
        );

        // Regression: auth-only (no pin) must stay Dynamic under the same
        // presence check — the numeric pin must not drag it into Pinned or
        // Fallback.
        let auth_only = r#"
            model = "x/y"
            [model_providers.codexferry]
            base_url = "http://127.0.0.1:8787/v1"
            wire_api = "responses"
            [model_providers.codexferry.auth]
            command = "echo"
            args = ["dummy"]
        "#;
        assert_eq!(
            detect_mode(Some(auth_only), DEFAULT_ACTIVE_PROVIDER),
            Mode::Dynamic
        );
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
        assert_eq!(
            detect_mode(Some(toml), DEFAULT_ACTIVE_PROVIDER),
            Mode::Dynamic
        );
    }

    #[test]
    fn no_pin_and_no_auth_is_fallback() {
        let toml = r#"
            model = "x/y"
            [model_providers.codexferry]
            base_url = "http://127.0.0.1:8787/v1"
            env_key = "DUMMY"
        "#;
        assert_eq!(
            detect_mode(Some(toml), DEFAULT_ACTIVE_PROVIDER),
            Mode::Fallback
        );
    }

    #[test]
    fn missing_text_is_fallback() {
        assert_eq!(detect_mode(None, DEFAULT_ACTIVE_PROVIDER), Mode::Fallback);
    }

    #[test]
    fn unparseable_text_is_fallback() {
        assert_eq!(
            detect_mode(Some("not valid toml [[["), DEFAULT_ACTIVE_PROVIDER),
            Mode::Fallback
        );
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
