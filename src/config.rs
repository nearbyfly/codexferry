//! TOML configuration types, validation rules, and hot-reload watcher.
//!
//! The config file (default `cxf.toml`, overridable via the
//! `CODEXFERRY_CONFIG` env var) is a TOML document with five optional
//! sections, each mirrored by a struct below:
//!
//! * `[server]` — bind `host`/`port` ([`ServerConfig`]).
//! * `[providers.<name>]` — one entry per upstream provider: `base_url`,
//!   `format` (`"chat"` or `"responses"`), an API-key source, and optional
//!   `timeout_ms` / `extra_headers` / `extra_params` / `drop_params`
//!   ([`ProviderConfig`]).
//! * `[routes]` — maps `provider/alias` route keys to upstream model names
//!   ([`RouteConfig`]).
//! * `[session]` — session-store tuning: TTL, max session count, memory
//!   budget ([`SessionConfig`]).
//! * `[quirks]` — provider-quirk kill switches (`disabled = [...]`)
//!   ([`QuirksConfig`]).
//!
//! Every section and field has a default, so an empty file is valid.
//!
//! ## Route-key rules (`provider/alias`)
//!
//! * A route key **must** contain `/`; it is split on the **first** `/` only
//!   (`split_once`), so aliases may themselves contain `/`
//!   (e.g. `openai/o3-mini/high`). Keys without a `/` fail validation.
//! * The prefix (before the first `/`) must match a `[providers.<name>]`
//!   key; otherwise the route is rejected with an `UnknownProvider` error.
//! * Route keys must be unique; a duplicate is a hard error.
//!
//! ## API-key resolution priority
//!
//! A provider needs exactly one key source; resolution happens at request
//! time in `upstream::resolve_api_key`, in this priority order:
//! `api_key` (plaintext) → `api_key_env` (env var name) → `api_key_file`
//! (file path, contents trimmed). Validation only checks that **at least
//! one** is configured — the actual lookup failure (e.g. unset env var) is
//! reported per-request, not at startup.
//!
//! ## Hot reload
//!
//! [`spawn_watcher`] watches the config file with the `notify` crate and
//! re-parses + re-validates it on every change. On success the new config
//! atomically replaces the old one in the shared [`SharedConfig`]; on
//! failure the **old config is kept** and the error is logged (the process
//! never exits because of a bad edit). Because `notify` callbacks run on a
//! synchronous thread, the callback only sends the parsed config into an
//! unbounded channel; an applier task on the tokio runtime awaits the write
//! lock, so an update is queued while the lock is busy and never lost - see
//! the `spawn_watcher` docs for details.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use thiserror::Error;

/// Raw, unvalidated mirror of the TOML config file.
///
/// This is the direct serde deserialization target; absent fields fall back
/// to their defaults. It is converted into a [`ValidatedConfig`] by
/// [`Config::validate`] before use, which cross-checks routes against
/// providers and pre-builds the route lookup table.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// `[server]` section: bind address and port.
    #[serde(default)]
    pub server: ServerConfig,
    /// `[providers.<name>]` section: upstream connections, keyed by provider name.
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    /// `[routes]` section: `provider/alias` → upstream model mapping.
    #[serde(default)]
    pub routes: HashMap<String, RouteConfig>,
    /// `[session]` section: session-store tuning.
    #[serde(default)]
    pub session: SessionConfig,
    /// `[quirks]` section: quirk kill switches (defaults: all enabled).
    #[serde(default)]
    pub quirks: QuirksConfig,
}

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
fn default_host() -> String {
    "127.0.0.1".into()
}
fn default_port() -> u16 {
    8787
}

/// `[providers.<name>]` section: one entry per upstream LLM provider.
///
/// The provider name (the map key) is the prefix used in route keys. At
/// request time the proxy appends `/chat/completions` or `/responses` to
/// `base_url`, so the base URL must already include the API version path
/// (e.g. `.../v1`) — the proxy never inserts `/v1` itself.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    /// Upstream base URL, **including the API version path**
    /// (e.g. `https://api.deepseek.com/v1`).
    pub base_url: String,
    /// Plaintext API key (highest priority of the three key sources).
    #[serde(default)]
    pub api_key: Option<String>,
    /// Name of an environment variable holding the API key
    /// (second priority).
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Path to a file containing the API key (lowest priority; the file is
    /// read and trimmed at request time).
    #[serde(default)]
    pub api_key_file: Option<String>,
    /// Upstream wire format: `"chat"` (Responses ↔ Chat conversion) or
    /// `"responses"` (native passthrough; leaked DSML/think markup is healed
    /// in place when the `dsml_heal`/`think_tags` quirks fire).
    pub format: String,
    /// Upstream request timeout in milliseconds. Defaults to 120000 (120 s).
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    /// Optional static headers injected into every upstream request
    /// (e.g. `{ "X-Custom" = "value" }`).
    #[serde(default)]
    pub extra_headers: Option<HashMap<String, String>>,
    /// Extra top-level fields merged into every chat request body for this
    /// provider (applied after conversion; wins on collision). Chat path
    /// only. Keys must not be router-managed fields (`model`, `messages`,
    /// `tools`, `stream`).
    #[serde(default)]
    pub extra_params: Option<HashMap<String, serde_json::Value>>,
    /// Top-level chat request fields to strip before sending — e.g.
    /// `["reasoning_effort"]` for an upstream that rejects unknown fields.
    /// Chat path only; same key restrictions as `extra_params`.
    #[serde(default)]
    pub drop_params: Option<Vec<String>>,
}
fn default_timeout() -> u64 {
    120_000
}

/// `[routes]` section: one entry per `provider/alias` route.
///
/// The map key is the route key — the model name Codex selects with `-m`.
/// The value says which upstream model to call and how large its context
/// window is (the latter is used by `gen-catalog`).
#[derive(Debug, Clone, Deserialize)]
pub struct RouteConfig {
    /// The upstream's actual model name (e.g. `deepseek-reasoner`).
    pub model: String,
    /// Context window advertised in the generated model catalog.
    /// Defaults to 1_048_576 (1 Mi tokens).
    #[serde(default = "default_context_window")]
    pub context_window: u64,
    /// Reasoning effort advertised as the model's default in the generated
    /// catalog (`default_reasoning_level`), restricting the Codex picker to
    /// this single effort. One of `minimal`|`low`|`medium`|`high`|`xhigh`
    /// (the values Codex accepts as reasoning levels). Optional: without it
    /// the catalog entry carries no reasoning fields at all, so nothing is
    /// inherited from the upstream template.
    #[serde(default)]
    pub default_reasoning_effort: Option<String>,
    /// Optional human-readable description appended to the generated model
    /// catalog entry's description (after "CodexFerry route {key}"),
    /// visible in the Codex `/model` picker as the model's subtitle.
    #[serde(default)]
    pub description: Option<String>,
}
fn default_context_window() -> u64 {
    1_048_576
}

/// `[session]` section: tuning knobs for the in-memory session store.
///
/// All fields are optional; see `session.rs` (`SessionStore`) for how each
/// limit is enforced at runtime.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionConfig {
    /// Idle time (hours) before a session is evicted. Defaults to 168
    /// (7 days).
    #[serde(default = "default_ttl")]
    pub ttl_hours: u64,
    /// Maximum number of cached sessions; LRU-evicted beyond this.
    /// Defaults to 256.
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,
    /// Memory budget in MiB; LRU-evicted beyond this. Defaults to 512.
    #[serde(default = "default_max_memory")]
    pub max_memory_mb: usize,
}
impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            ttl_hours: default_ttl(),
            max_sessions: default_max_sessions(),
            max_memory_mb: default_max_memory(),
        }
    }
}
fn default_ttl() -> u64 {
    168
}
fn default_max_sessions() -> usize {
    256
}
fn default_max_memory() -> usize {
    512
}

/// `[quirks]` section: kill switches for provider-quirk workarounds.
///
/// Quirks default to enabled; listing a name in `disabled` turns it off.
/// Names are matched case-insensitively; unknown names log a warning at
/// validation time (see [`crate::quirks::QUIRK_NAMES`] for the registry).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct QuirksConfig {
    /// Quirk names to disable, e.g. `["glm_thinking"]`.
    #[serde(default)]
    pub disabled: Vec<String>,
}

/// Errors raised while parsing or validating the config file.
///
/// Each variant documents the exact condition that raises it; see
/// [`Config::validate`] for the validation order.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// A route key contains no `/` — every key must be `provider/alias`.
    #[error("route key '{0}' must contain '/' (format: provider/alias)")]
    MissingSlash(String),
    /// A route key's prefix (before the first `/`) does not match any
    /// `[providers.<name>]` entry; `available` lists the known providers.
    #[error("route key '{key}': provider '{provider}' not found in [providers] (available: {available})")]
    UnknownProvider {
        key: String,
        provider: String,
        available: String,
    },
    /// The same route key appears more than once in `[routes]`.
    #[error("duplicate route key '{0}'")]
    DuplicateRoute(String),
    /// A provider defines none of `api_key` / `api_key_env` / `api_key_file`.
    #[error("provider '{0}' has no API key (set api_key, api_key_env, or api_key_file)")]
    MissingApiKey(String),
    /// A provider's `format` is neither `"chat"` nor `"responses"`.
    #[error("provider '{0}': format must be 'chat' or 'responses', got '{1}'")]
    InvalidFormat(String, String),
    /// A provider's `extra_params`/`drop_params` names a field the router
    /// itself manages (or an empty key) — rewriting it would break routing.
    #[error("provider '{provider}': {field} key '{key}' must not be empty or name a router-managed field (model|messages|tools|stream)")]
    ManagedParamViolation {
        provider: String,
        field: String,
        key: String,
    },
    /// A route's `default_reasoning_effort` is not a known Codex effort.
    #[error("route '{key}': default_reasoning_effort must be one of none|minimal|low|medium|high|xhigh|max|ultra, got '{effort}'")]
    InvalidReasoningEffort { key: String, effort: String },
    /// TOML syntax / schema error from `toml::from_str`.
    #[error(transparent)]
    Parse(#[from] toml::de::Error),
    /// I/O error while reading the config file from disk.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Parsed and validated config with pre-built route->provider lookup.
///
/// Produced by [`Config::validate`]: every route key is cross-checked
/// against `[providers]`, and each route is flattened into a
/// [`ValidatedRoute`] that embeds its resolved [`ProviderConfig`]. Request
/// handlers therefore need a single map lookup (`routes[model]`) to find
/// both the upstream model and the provider settings.
#[derive(Debug, Clone)]
pub struct ValidatedConfig {
    pub server: ServerConfig,
    pub providers: HashMap<String, ProviderConfig>,
    /// Validated routes, keyed by the full `provider/alias` route key.
    pub routes: HashMap<String, ValidatedRoute>,
    pub session: SessionConfig,
    pub quirks: QuirksConfig,
}

impl Default for ValidatedConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            providers: HashMap::new(),
            routes: HashMap::new(),
            session: SessionConfig::default(),
            quirks: QuirksConfig {
                disabled: Vec::new(),
            },
        }
    }
}

/// A single validated route: route key → provider + upstream model.
///
/// This is what `ValidatedConfig::routes` maps each `provider/alias` key
/// to. The provider name is captured separately from the full provider
/// config so handlers never have to re-derive it from the key.
#[derive(Debug, Clone)]
pub struct ValidatedRoute {
    /// The provider-name prefix of the route key (before the first `/`).
    #[allow(dead_code)]
    pub provider_name: String,
    /// The resolved provider configuration (base_url, format, key source, …).
    pub provider: ProviderConfig,
    /// The upstream model name to call.
    pub model: String,
    /// Context window, used by `gen-catalog`.
    pub context_window: u64,
    /// Reasoning effort advertised as the catalog default for this route;
    /// `None` leaves the catalog entry without reasoning fields (see
    /// [`RouteConfig::default_reasoning_effort`]).
    pub default_reasoning_effort: Option<String>,
    /// Optional description appended to the catalog entry's description
    /// (see [`RouteConfig::description`]).
    pub description: Option<String>,
}

/// Current default config filename (the file the server and subcommands load
/// when no explicit path is given). Renamed from `config.toml` on 2026-08-23
/// to stop colliding, in name and in conversation, with Codex's own
/// `~/.codex/config.toml` - the two files describe opposite ends of the same
/// pipe and the shared name kept producing mixups.
pub const DEFAULT_CONFIG_FILENAME: &str = "cxf.toml";

/// Pre-rename default filename, still honored so an existing installation
/// does not silently lose its routes (the rename would otherwise manifest as
/// "config not found" or an empty route table - the exact silent-loss class
/// the doctor route-count check exists to catch).
const LEGACY_CONFIG_FILENAME: &str = "config.toml";

/// Resolve the config path used when no explicit `--config` is passed:
///
/// 1. `$CODEXFERRY_CONFIG`, if set (unchanged semantics);
/// 2. `./cxf.toml`, if it exists;
/// 3. `./config.toml` (legacy), if it exists - emits a deprecation warning;
/// 4. otherwise `./cxf.toml`, so the downstream "file not found" error names
///    the current default rather than a name the user never wrote.
pub fn default_config_path() -> std::path::PathBuf {
    let env = std::env::var("CODEXFERRY_CONFIG").ok();
    let (path, legacy) = resolve_default_config(env.as_deref(), std::path::Path::new("."));
    if legacy {
        // eprintln, not tracing: the doctor and gen-catalog paths may run
        // without a tracing subscriber, and a silent legacy pickup is worse
        // than the current behavior it replaces.
        eprintln!(
            "WARNING: loading legacy config `{}`; rename it to `{}` (the default filename) \
             to silence this warning",
            path.display(),
            DEFAULT_CONFIG_FILENAME
        );
    }
    path
}

/// Pure resolution core of [`default_config_path`], parameterized for tests.
/// Returns the resolved path and whether it is the legacy filename.
fn resolve_default_config(env: Option<&str>, cwd: &Path) -> (std::path::PathBuf, bool) {
    if let Some(env) = env {
        return (std::path::PathBuf::from(env), false);
    }
    let current = cwd.join(DEFAULT_CONFIG_FILENAME);
    if current.exists() {
        return (current, false);
    }
    let legacy = cwd.join(LEGACY_CONFIG_FILENAME);
    if legacy.exists() {
        return (legacy, true);
    }
    (current, false)
}

impl Config {
    /// Load a config from a TOML file on disk.
    ///
    /// Reads the file (I/O failures become [`ConfigError::Io`]) and
    /// delegates to [`Config::parse_str`]. Note this only *parses* — call
    /// [`Config::validate`] on the result before use.
    pub fn parse_file(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)?;
        Self::parse_str(&text)
    }

    /// Parse a TOML string into the raw config types.
    ///
    /// Syntax errors are wrapped in [`ConfigError::Parse`]. Unknown or
    /// misspelled fields are silently ignored by serde's default behavior.
    pub fn parse_str(text: &str) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(ConfigError::Parse)
    }

    /// Validate the raw config and build the route→provider lookup table.
    ///
    /// Rules run in this order:
    /// 0. per-provider, before the route loop: `extra_params`/`drop_params`
    ///    must not name a router-managed field (model|messages|tools|stream)
    ///    or an empty key (see the `MANAGED_FIELDS` check below);
    /// For each route key:
    /// 1. it must contain `/` — split on the **first** `/` only
    ///    (`split_once`, so aliases may contain `/` themselves);
    /// 2. the prefix must match a `[providers.<name>]` entry;
    /// 3. that provider's `format` must be `chat` or `responses`;
    /// 4. that provider must define at least one API-key source;
    /// 5. the key must not already be present in the output map
    ///    (uniqueness check, using the return value of `HashMap::insert`).
    ///
    /// On success the consumed `self` is transformed into a
    /// [`ValidatedConfig`]; the first failing rule produces the
    /// corresponding [`ConfigError`].
    pub fn validate(self) -> Result<ValidatedConfig, ConfigError> {
        let mut validated_routes = HashMap::new();
        let provider_names: Vec<&str> = self.providers.keys().map(|s| s.as_str()).collect();

        // Rule 0 (providers): extra_params/drop_params may not touch the
        // fields the router itself manages — rewriting model/messages/
        // tools/stream would silently break routing or the conversion
        // contract. Empty keys are always a mistake.
        // NOTE: `stream_options` is deliberately NOT in this list — it is
        // router-managed, but the plan chose a 4-field list; overriding it
        // via extra_params is an explicit, visible escape hatch.
        const MANAGED_FIELDS: [&str; 4] = ["model", "messages", "tools", "stream"];
        for (name, provider) in &self.providers {
            if let Some(extra) = &provider.extra_params {
                for key in extra.keys() {
                    if key.is_empty() || MANAGED_FIELDS.contains(&key.as_str()) {
                        return Err(ConfigError::ManagedParamViolation {
                            provider: name.clone(),
                            field: "extra_params".into(),
                            key: key.clone(),
                        });
                    }
                }
            }
            if let Some(drop) = &provider.drop_params {
                for key in drop {
                    if key.is_empty() || MANAGED_FIELDS.contains(&key.as_str()) {
                        return Err(ConfigError::ManagedParamViolation {
                            provider: name.clone(),
                            field: "drop_params".into(),
                            key: key.clone(),
                        });
                    }
                }
            }
        }

        for (key, route) in &self.routes {
            // Rule 1: the route key must contain a `/` separator.
            let (provider_name, _alias) = key
                .split_once('/')
                .ok_or_else(|| ConfigError::MissingSlash(key.clone()))?;

            // Rule 2: the prefix must name a configured provider.
            let provider =
                self.providers
                    .get(provider_name)
                    .ok_or_else(|| ConfigError::UnknownProvider {
                        key: key.clone(),
                        provider: provider_name.into(),
                        available: provider_names.join(", "),
                    })?;

            // Rule 3: the provider's wire format must be known.
            if provider.format != "chat" && provider.format != "responses" {
                return Err(ConfigError::InvalidFormat(
                    provider_name.into(),
                    provider.format.clone(),
                ));
            }

            // Rule 4: at least one API-key source must be configured.
            // (Resolution — api_key → api_key_env → api_key_file — happens
            // later at request time in upstream::resolve_api_key.)
            if provider.api_key.is_none()
                && provider.api_key_env.is_none()
                && provider.api_key_file.is_none()
            {
                return Err(ConfigError::MissingApiKey(provider_name.into()));
            }

            // Rule 5: `default_reasoning_effort`, when set, must be one of
            // the efforts Codex accepts as a reasoning level (the wire
            // values of `ReasoningEffort`; `ultra` is remapped to `max` by
            // the Codex client before sending).
            if let Some(effort) = &route.default_reasoning_effort {
                if !matches!(
                    effort.as_str(),
                    "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra"
                ) {
                    return Err(ConfigError::InvalidReasoningEffort {
                        key: key.clone(),
                        effort: effort.clone(),
                    });
                }
            }

            // Rule 6: duplicate keys are rejected — `insert` returns the
            // previous value if the key already existed.
            if validated_routes
                .insert(
                    key.clone(),
                    ValidatedRoute {
                        provider_name: provider_name.to_string(),
                        provider: provider.clone(),
                        model: route.model.clone(),
                        context_window: route.context_window,
                        default_reasoning_effort: route.default_reasoning_effort.clone(),
                        description: route.description.clone(),
                    },
                )
                .is_some()
            {
                return Err(ConfigError::DuplicateRoute(key.clone()));
            }
        }

        // Unknown quirk names in the disable list are a warning, not an
        // error: a typo should not take the daemon down, and the list may
        // legitimately name a quirk this binary predates.
        for name in crate::quirks::unknown_quirk_names(&self.quirks.disabled) {
            tracing::warn!("[quirks] disabled: unknown quirk name `{name}`");
        }

        Ok(ValidatedConfig {
            server: self.server,
            providers: self.providers,
            routes: validated_routes,
            session: self.session,
            quirks: self.quirks,
        })
    }
}

impl ValidatedConfig {
    /// Whether a quirk is enabled, honoring the `[quirks]` disable list
    /// (case-insensitive). The single lookup every quirk gate goes through.
    pub fn quirk_enabled(&self, name: &str) -> bool {
        !self
            .quirks
            .disabled
            .iter()
            .any(|disabled| disabled.eq_ignore_ascii_case(name))
    }
}

use notify::{EventKind, RecursiveMode, Watcher};
use std::sync::Arc;

#[cfg(test)]
mod effort_validation_tests {
    use super::*;

    fn parse(text: &str) -> Result<ValidatedConfig, ConfigError> {
        Config::parse_str(text).unwrap().validate()
    }

    #[test]
    fn valid_efforts_pass_validation() {
        for effort in [
            "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
        ] {
            let cfg = parse(&format!(
                r#"
[providers.x]
base_url = "https://x.com/v1"
api_key = "k"
format = "responses"
[routes]
"x/m" = {{ model = "m", default_reasoning_effort = "{effort}" }}
"#
            ))
            .unwrap_or_else(|e| panic!("{effort} should validate: {e}"));
            assert_eq!(
                cfg.routes["x/m"].default_reasoning_effort.as_deref(),
                Some(effort)
            );
        }
    }

    #[test]
    fn unknown_effort_is_rejected() {
        let err = parse(
            r#"
[providers.x]
base_url = "https://x.com/v1"
api_key = "k"
format = "responses"
[routes]
"x/m" = { model = "m", default_reasoning_effort = "extreme" }
"#,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ConfigError::InvalidReasoningEffort { key, effort }
                if key == "x/m" && effort == "extreme"
        ));
    }
}

#[cfg(test)]
mod provider_params_tests {
    use super::*;

    fn parse(text: &str) -> Result<ValidatedConfig, ConfigError> {
        Config::parse_str(text).unwrap().validate()
    }

    #[test]
    fn extra_and_drop_params_parse() {
        let cfg = parse(
            r#"
[providers.x]
base_url = "https://x.com/v1"
api_key = "k"
format = "chat"
extra_params = { top_k = 50 }
drop_params = ["reasoning_effort"]
[routes]
"x/m" = { model = "m" }
"#,
        )
        .unwrap();
        assert_eq!(
            cfg.providers["x"].extra_params.as_ref().unwrap()["top_k"],
            serde_json::json!(50)
        );
        assert_eq!(
            cfg.providers["x"].drop_params.as_ref().unwrap(),
            &vec!["reasoning_effort".to_string()]
        );
    }

    #[test]
    fn managed_fields_are_rejected() {
        // Every router-managed field (model|messages|tools|stream) is
        // blocked in at least one of the two param kinds.
        let cases = [
            (
                "extra_params",
                r#"{ top_k = 50, model = "other" }"#,
                "model",
            ),
            ("extra_params", r#"{ top_k = 50, tools = [] }"#, "tools"),
            ("extra_params", r#"{ top_k = 50, stream = true }"#, "stream"),
            (
                "drop_params",
                r#"["reasoning_effort", "messages"]"#,
                "messages",
            ),
            ("drop_params", r#"["stream"]"#, "stream"),
        ];
        for (field, body, expected) in cases {
            let err = parse(&format!(
                r#"
[providers.x]
base_url = "https://x.com/v1"
api_key = "k"
format = "chat"
{field} = {body}
[routes]
"x/m" = {{ model = "m" }}
"#
            ))
            .unwrap_err();
            assert!(
                matches!(&err, ConfigError::ManagedParamViolation { key, .. } if key == expected),
                "field {field} with key {expected}: got {err:?}"
            );
        }
    }

    #[test]
    fn empty_keys_are_rejected() {
        let err = parse(
            r#"
[providers.x]
base_url = "https://x.com/v1"
api_key = "k"
format = "chat"
drop_params = [""]
[routes]
"x/m" = { model = "m" }
"#,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ConfigError::ManagedParamViolation { key, .. } if key.is_empty()
        ));
    }
}

#[cfg(test)]
mod quirks_config_tests {
    use super::*;

    fn parse(text: &str) -> Result<ValidatedConfig, ConfigError> {
        Config::parse_str(text).unwrap().validate()
    }

    const BASE: &str = r#"
[providers.x]
base_url = "https://x.com/v1"
api_key = "k"
format = "chat"
[routes]
"x/m" = { model = "m" }
"#;

    // The `tracing::warn!` emission for unknown names is not asserted here
    // (by design): the pure filter is tested in `quirks::tests`, and a
    // tracing-harness assertion is out of scope.
    #[test]
    fn quirks_default_to_all_enabled() {
        let cfg = parse(BASE).unwrap();
        assert!(cfg.quirk_enabled("glm_thinking"));
        assert!(cfg.quirk_enabled("missing_done"));
    }

    #[test]
    fn disabled_quirks_match_case_insensitively() {
        let cfg = parse(&format!(
            "{BASE}\n[quirks]\ndisabled = [\"GLM_Thinking\"]\n"
        ))
        .unwrap();
        assert!(!cfg.quirk_enabled("glm_thinking"));
        assert!(cfg.quirk_enabled("missing_done"));
    }

    #[test]
    fn merge_fragmented_defaults_to_enabled() {
        let cfg = parse(BASE).unwrap();
        assert!(cfg.quirk_enabled("merge_fragmented"));
    }

    #[test]
    fn merge_fragmented_disables_via_list() {
        let cfg = parse(&format!(
            "{BASE}\n[quirks]\ndisabled = [\"merge_fragmented\"]\n"
        ))
        .unwrap();
        assert!(!cfg.quirk_enabled("merge_fragmented"));
        // other quirks unaffected
        assert!(cfg.quirk_enabled("dsml_heal"));
        assert!(cfg.quirk_enabled("think_tags"));
    }
}
use tokio::sync::RwLock;

/// Handle to the currently-active validated config, shared across the app.
///
/// An `Arc<RwLock<ValidatedConfig>>`; clones are passed to every request
/// handler (via `AppState`) and to the hot-reload watcher. Handlers take a
/// read lock to snapshot the current routes; the watcher swaps the whole
/// struct with a write lock on reload. Because each guard is a full
/// snapshot, an in-flight request always sees a consistent config, even
/// while a reload is happening.
pub type SharedConfig = Arc<RwLock<ValidatedConfig>>;

/// Spawn a file watcher that hot-reloads config on change.
/// On parse error, keeps old config and logs error.
///
/// Uses the `notify` crate to watch the config file for `Modify`/`Create`
/// events. On each event the file is re-read, re-validated, and sent over
/// an unbounded channel to an applier task on the tokio runtime:
///
/// * **Success** - the new config atomically replaces the old one in
///   `shared` (the applier awaits the write lock); `config reloaded
///   successfully` is logged at info level.
/// * **Parse/validation error** - the old config is kept and the error is
///   logged at error level; the process keeps running with the last good
///   config (no restart, no crash).
///
/// ### Channel + async applier design
///
/// `notify` invokes its callback on a synchronous thread, which cannot
/// `await` the async `RwLock` (a blocking `write()` there would stall the
/// notify thread behind every in-flight request). Instead the callback only
/// does a non-blocking `send` into an unbounded channel, and the
/// [`spawn_config_applier`] task on the tokio runtime loops over the
/// receiver and awaits the write lock. This keeps the notify thread free,
/// guarantees delivery (an unbounded send cannot fail while the applier
/// holds the receiver), and lets the write wait patiently behind read
/// guards - an update is queued while the lock is busy, never dropped.
///
/// The returned `impl Watcher` must be kept alive for the process lifetime
/// (the caller in `proxy::serve` stores it in `_watcher`); dropping it stops
/// watching and closes the channel, which ends the applier task.
/// Whether any path in a notify event refers to the config file itself
/// (final-component match). The watcher observes the whole parent
/// DIRECTORY, so this filters out editor temporaries (\.swp, 4913,
/// rename sources) and unrelated directory activity (hot-reload-watcher
/// spec §Design "Event filter").
fn event_touches_config(paths: &[std::path::PathBuf], file_name: &std::ffi::OsStr) -> bool {
    paths.iter().any(|p| p.file_name() == Some(file_name))
}

pub fn spawn_watcher(path: &Path, shared: SharedConfig) -> anyhow::Result<impl Watcher> {
    // Resolve symlinks ONCE and watch the REAL file's parent directory
    // with a filename filter (hot-reload-watcher spec §Design). An
    // inode-level file watch dies permanently on the first atomic-rename
    // editor save: the rename unlinks the watched inode, the kernel
    // delivers IN_IGNORED, and notify drops the watch with nothing to
    // re-arm it. The directory's inode is stable across file
    // replacements. Limitation: the symlink is resolved at startup only
    // — re-pointing it later requires a daemon restart.
    let resolved = path.canonicalize().unwrap_or_else(|e| {
        tracing::warn!(
            "config path {} could not be canonicalized ({e}); \
             watching the unresolved path's directory",
            path.display()
        );
        path.to_path_buf()
    });
    let file_name = resolved
        .file_name()
        .map(std::ffi::OsStr::to_owned)
        .ok_or_else(|| anyhow::anyhow!("config path has no file name: {}", resolved.display()))?;
    let watch_dir = resolved.parent().map(|p| p.to_path_buf()).ok_or_else(|| {
        anyhow::anyhow!(
            "config path has no parent directory: {}",
            resolved.display()
        )
    })?;
    let watch_path = resolved;
    // Decouple the synchronous notify callback from the async RwLock: the
    // callback only sends (never blocks, never loses an update), and the
    // applier task awaits the write lock on the tokio runtime.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<ValidatedConfig>();
    let _applier = tokio::spawn(spawn_config_applier(shared, rx));

    let mut watcher = notify::recommended_watcher(move |res: Result<notify::Event, _>| {
        if let Ok(event) = res {
            if matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_))
                && event_touches_config(&event.paths, &file_name)
            {
                // Editors that save atomically (vim `set backupcopy=no`,
                // emacs, many IDEs) write to a temp file then `rename(temp,
                // target)` - the rename briefly unlinks the target before
                // the new inode appears. The notify event lands during that
                // gap, the parse_file fails with ENOENT, and the user sees
                // `config reload failed` followed by a successful reload
                // on the next event. Retry a couple of times with a tiny
                // sleep so the spurious error disappears; non-ENOENT
                // errors and ENOENT past the retry budget still surface
                // (a real file deletion is a meaningful state change).
                for attempt in 0..3 {
                    match Config::parse_file(&watch_path).and_then(|c| c.validate()) {
                        Ok(new_cfg) => {
                            // Unbounded send: cannot fail while the
                            // applier task holds the receiver. If it
                            // has already shut down (daemon exit), the
                            // update is moot - ignore the error.
                            let _ = tx.send(new_cfg);
                            break;
                        }
                        Err(ConfigError::Io(io_err))
                            if io_err.kind() == std::io::ErrorKind::NotFound && attempt < 2 =>
                        {
                            std::thread::sleep(std::time::Duration::from_millis(20));
                            continue;
                        }
                        Err(e) => {
                            tracing::error!("config reload failed, keeping old config: {e}");
                            break;
                        }
                    }
                }
            }
        }
    })?;
    watcher.watch(&watch_dir, RecursiveMode::NonRecursive)?;
    Ok(watcher)
}

/// Applier loop for hot-reloaded configs: receives validated configs from
/// the notify callback's channel and swaps them into `shared` under the
/// write lock. Unlike the previous `try_write` design, an update sent while
/// the lock is busy is queued, not dropped - it is applied as soon as
/// in-flight requests release their read guards.
async fn spawn_config_applier(
    shared: SharedConfig,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<ValidatedConfig>,
) {
    while let Some(new_cfg) = rx.recv().await {
        *shared.write().await = new_cfg;
        tracing::info!("config reloaded successfully");
        invalidate_codex_catalog_cache();
    }
}

/// Codex CLI writes `~/.codex/models_cache.json` after a successful
/// `/v1/models` fetch and reuses it (TTL 300s) on later turns. That cache
/// locks the active TUI session to whichever routes existed at session
/// start - adding a new route to `cxf.toml` does NOT make it appear in
/// the `/model` picker, and the user's `-m` selection stays pinned to the
/// old slug (the router rejects those requests with 400).
///
/// Removing the cache file forces codex's next `list_models(OnlineIfUncached)`
/// to re-fetch `/v1/models` from the now-hot-reloaded router. The TUI
/// picker then shows the new route, and codex's existing thread-settings
/// RPC (`AppEvent::UpdateModel` -> `sync_active_thread_model_setting` in
/// `codex-rs/tui/src/app/thread_settings.rs`) lets the user switch to it
/// via `/model` mid-session - no restart needed.
///
/// No-op in static mode (codex never writes `models_cache.json` when a
/// `model_catalog_json` pin is in effect) and when the file is absent
/// already.
fn invalidate_codex_catalog_cache() {
    let Some(path) = codex_catalog_cache_path() else {
        return;
    };
    match invalidate_path(&path) {
        Ok(()) => tracing::info!("invalidated codex catalog cache at {}", path.display()),
        Err(e) => tracing::warn!(
            "failed to invalidate codex catalog cache ({}): {e}",
            path.display()
        ),
    }
}

/// Resolve `~/.codex/models_cache.json` honoring `CODEX_HOME` (codex
/// default = `$CODEX_HOME`; fallback = `$HOME/.codex`). Returns None
/// when neither env var is set.
fn codex_catalog_cache_path() -> Option<std::path::PathBuf> {
    let base = std::env::var("CODEX_HOME")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| std::path::PathBuf::from(format!("{h}/.codex")))
        })?;
    Some(base.join("models_cache.json"))
}

/// Remove `path`, treating `NotFound` as success. Pure (no env reads) so
/// the unit tests can exercise it without env-var mutation hazards.
pub(crate) fn invalidate_path(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalidate_path_removes_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models_cache.json");
        std::fs::write(&path, "{}").unwrap();
        invalidate_path(&path).expect("removing existing cache should succeed");
        assert!(!path.exists(), "cache file must be gone after invalidation");
    }

    #[test]
    fn invalidate_path_missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        invalidate_path(&path).expect("NotFound is success - no-op");
    }

    #[test]
    fn default_path_env_var_wins_over_both_filenames() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(DEFAULT_CONFIG_FILENAME), "").unwrap();
        std::fs::write(dir.path().join(LEGACY_CONFIG_FILENAME), "").unwrap();
        let (path, legacy) = resolve_default_config(Some("/explicit/cxf.toml"), dir.path());
        assert_eq!(path, std::path::PathBuf::from("/explicit/cxf.toml"));
        assert!(!legacy);
    }

    #[test]
    fn default_path_prefers_cxf_toml_when_both_exist() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(DEFAULT_CONFIG_FILENAME), "").unwrap();
        std::fs::write(dir.path().join(LEGACY_CONFIG_FILENAME), "").unwrap();
        let (path, legacy) = resolve_default_config(None, dir.path());
        assert_eq!(path, dir.path().join(DEFAULT_CONFIG_FILENAME));
        assert!(!legacy);
    }

    #[test]
    fn default_path_falls_back_to_legacy_config_toml_with_flag() {
        // The whole point of the fallback: an installation upgraded in place
        // keeps its routes, but the legacy pickup is flagged so the operator
        // renames the file instead of depending on the fallback forever.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(LEGACY_CONFIG_FILENAME), "").unwrap();
        let (path, legacy) = resolve_default_config(None, dir.path());
        assert_eq!(path, dir.path().join(LEGACY_CONFIG_FILENAME));
        assert!(legacy);
    }

    #[test]
    fn default_path_names_cxf_toml_when_nothing_exists() {
        // No file at all: return the CURRENT default (not the legacy name)
        // so the "file not found" error tells the user what to create.
        let dir = tempfile::tempdir().unwrap();
        let (path, legacy) = resolve_default_config(None, dir.path());
        assert_eq!(path, dir.path().join(DEFAULT_CONFIG_FILENAME));
        assert!(!legacy);
    }

    fn make_config(toml_str: &str) -> ValidatedConfig {
        Config::parse_str(toml_str).unwrap().validate().unwrap()
    }

    const VALID_CONFIG: &str = r#"
[server]
port = 9999

[providers.deepseek]
base_url = "https://api.deepseek.com/v1"
api_key_env = "DEEPSEEK_API_KEY"
format = "chat"

[providers.ark]
base_url = "https://ark.cn-beijing.volces.com/api/v3"
api_key = "sk-test"
format = "chat"

[routes]
"deepseek/flash" = { model = "deepseek-chat" }
"ark/glm-5" = { model = "glm-5", context_window = 131072 }
"#;

    #[test]
    fn valid_config_parses() {
        let cfg = make_config(VALID_CONFIG);
        assert_eq!(cfg.server.port, 9999);
        assert_eq!(cfg.routes.len(), 2);
        let route = cfg.routes.get("deepseek/flash").unwrap();
        assert_eq!(route.provider_name, "deepseek");
        assert_eq!(route.model, "deepseek-chat");
        assert_eq!(route.context_window, 1_048_576);
    }

    #[test]
    fn route_without_slash_fails() {
        let toml_str = r#"
[providers.x]
base_url = "https://x.com/v1"
api_key = "k"
format = "chat"
[routes]
"noflash" = { model = "m" }
"#;
        let err = Config::parse_str(toml_str).unwrap().validate().unwrap_err();
        assert!(matches!(err, ConfigError::MissingSlash(_)));
    }

    #[test]
    fn unknown_provider_fails() {
        let toml_str = r#"
[providers.x]
base_url = "https://x.com/v1"
api_key = "k"
format = "chat"
[routes]
"y/flash" = { model = "m" }
"#;
        let err = Config::parse_str(toml_str).unwrap().validate().unwrap_err();
        assert!(matches!(err, ConfigError::UnknownProvider { .. }));
    }

    #[test]
    fn missing_api_key_fails() {
        let toml_str = r#"
[providers.x]
base_url = "https://x.com/v1"
format = "chat"
[routes]
"x/flash" = { model = "m" }
"#;
        let err = Config::parse_str(toml_str).unwrap().validate().unwrap_err();
        assert!(matches!(err, ConfigError::MissingApiKey(_)));
    }

    #[test]
    fn invalid_format_fails() {
        let toml_str = r#"
[providers.x]
base_url = "https://x.com/v1"
api_key = "k"
format = "xml"
[routes]
"x/flash" = { model = "m" }
"#;
        let err = Config::parse_str(toml_str).unwrap().validate().unwrap_err();
        assert!(matches!(err, ConfigError::InvalidFormat(_, _)));
    }

    #[test]
    fn session_defaults() {
        let cfg = make_config(VALID_CONFIG);
        assert_eq!(cfg.session.ttl_hours, 168);
        assert_eq!(cfg.session.max_sessions, 256);
        assert_eq!(cfg.session.max_memory_mb, 512);
    }

    /// A config change must be applied even when a request holds the read
    /// lock at the moment the notify event fires. The channel-based applier
    /// must queue the update and apply it once the lock is released - this
    /// guards against regressing to a try_write design that would silently
    /// drop the update under contention.
    #[tokio::test]
    async fn config_reload_applies_when_read_lock_held_during_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cxf.toml");
        let config_a = r#"
[providers.x]
base_url = "https://x.com/v1"
api_key = "old-key"
format = "chat"
[routes]
"x/flash" = { model = "m" }
"#;
        let config_b = r#"
[providers.x]
base_url = "https://x.com/v1"
api_key = "new-key"
format = "chat"
[routes]
"x/flash" = { model = "m" }
"#;
        std::fs::write(&path, config_a).unwrap();
        let shared: SharedConfig = Arc::new(RwLock::new(make_config(config_a)));
        let _watcher = spawn_watcher(&path, shared.clone()).unwrap();

        // Simulate an in-flight request: hold the read lock so the notify
        // callback's try_write (or the applier's write) cannot proceed.
        let guard = shared.read().await;
        std::fs::write(&path, config_b).unwrap();
        // Give the notify callback time to fire and fail to acquire the
        // write lock while the read guard is still held.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        drop(guard);

        // The update must eventually land - poll instead of a single sleep
        // so the test is robust on slow filesystems.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let applied = {
                let cfg = shared.read().await;
                cfg.providers
                    .get("x")
                    .and_then(|p| p.api_key.as_deref())
                    .map(|k| k == "new-key")
                    .unwrap_or(false)
            };
            if applied {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "config reload was lost: api_key is still not 'new-key' \
                 after the read lock was released"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
}

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

    #[cfg(test)]
    mod watcher_tests {
        use super::*;

        #[test]
        fn event_touches_config_matches_only_the_exact_filename() {
            let name = std::ffi::OsStr::new("cxf.toml");
            // Direct hit (in-place write on the watched directory).
            assert!(event_touches_config(
                &[std::path::PathBuf::from("/etc/cxf.toml")],
                name
            ));
            // Rename events carry from+to paths; the to-path matches.
            assert!(event_touches_config(
                &[
                    std::path::PathBuf::from("/etc/cxf.toml.editor-tmp"),
                    std::path::PathBuf::from("/etc/cxf.toml"),
                ],
                name
            ));
            // Editor temporaries and unrelated files never match.
            assert!(!event_touches_config(
                &[std::path::PathBuf::from("/etc/.cxf.toml.swp")],
                name
            ));
            assert!(!event_touches_config(
                &[std::path::PathBuf::from("/etc/4913")],
                name
            ));
            // No paths -> not relevant.
            assert!(!event_touches_config(&[], name));
        }
    }
}
