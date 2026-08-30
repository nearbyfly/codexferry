//! Registry of provider-quirk switches and model-name matchers.
//!
//! Quirks are workarounds for provider-specific behavior that is not part
//! of the Responses ⇄ Chat Completions contract itself. They are gated by
//! the `[quirks]` config section (`disabled = [...]`, case-insensitive),
//! which — unlike codex-relay's env-var switch — is hot-reloaded by the
//! config watcher, so a quirk can be turned off without restarting the
//! daemon. The accepted quirk names are [`QUIRK_NAMES`]; unknown names in
//! the disable list are warned about (not rejected) at validation time.

/// Names of all registered quirks, for the unknown-name warning in
/// `config.rs` and for documentation. Keep in sync with the quirk lookups:
/// the `glm_thinking` and `missing_done` gates live in `proxy`;
/// `convert::request` receives the `glm_thinking` gate as a pre-read bool;
/// the `dsml_heal`, `think_tags`, and `merge_fragmented` gates are pre-read
/// by `proxy` per request ([`crate::heal::HealGates`]) and consumed in
/// `convert::response` / the passthrough healing path.
pub const QUIRK_NAMES: &[&str] = &["glm_thinking", "missing_done", "dsml_heal", "think_tags", "merge_fragmented"];

/// Whether a model name looks like a GLM/Zhipu reasoning model that needs
/// the explicit `thinking` switch to emit `reasoning_content` (quirk
/// `glm_thinking`; codex-relay issue #26).
pub fn is_glm_like_model(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.contains("glm") || m.contains("zhipu") || m.contains("bigmodel")
}

/// Names in a `[quirks] disabled` list that are not registered quirks.
///
/// Comparison is case-insensitive against [`QUIRK_NAMES`]; the original
/// spelling is returned so the caller's warning names what the user wrote.
/// Pure (no logging) so it is unit-testable without a tracing harness;
/// `Config::validate` logs each returned name.
pub fn unknown_quirk_names(disabled: &[String]) -> Vec<String> {
    disabled
        .iter()
        .filter(|name| !QUIRK_NAMES.iter().any(|q| q.eq_ignore_ascii_case(name)))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glm_like_model_matching() {
        assert!(is_glm_like_model("glm-5.2"));
        assert!(is_glm_like_model("ZhipuAI/foo"));
        assert!(is_glm_like_model("bigmodel-x"));
        assert!(!is_glm_like_model("deepseek-v4-pro"));
        assert!(!is_glm_like_model("kimi-k2-thinking"));
    }

    #[test]
    fn unknown_quirk_names_filters_case_insensitively() {
        let disabled: Vec<String> = ["GLM_Thinking", "missing_done", "no_such_quirk"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(unknown_quirk_names(&disabled), vec!["no_such_quirk"]);
        assert!(unknown_quirk_names(&[]).is_empty());
    }
}
