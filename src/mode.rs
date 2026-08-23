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
