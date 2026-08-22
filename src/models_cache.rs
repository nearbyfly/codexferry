//! Cache for the live `GET /v1/models` catalog response.
//!
//! The `/models` endpoint serves the same catalog the `gen-catalog`
//! subcommand writes to disk, but the server's routes can change via
//! hot-reload and the codex template file can change on disk. This module
//! keeps the serialized `{"models": [...]}` body plus an ETag, invalidating
//! when the route fingerprint or the template file's mtime changes.

use crate::config::ValidatedConfig;
use bytes::Bytes;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

/// How often a non-file template source is re-probed.
///
/// File-backed templates are invalidated by mtime, but a shell-out template
/// (or a temporary discovery failure) has no path to stat. The cache re-runs
/// template discovery on this cadence instead of never, so a Codex upgrade or
/// a transient failure cannot leave the catalog stale indefinitely.
const TEMPLATE_RECHECK_INTERVAL: Duration = Duration::from_secs(60);

/// Compute a hex ETag from bytes using `DefaultHasher` (no sha2 dependency).
pub fn weak_etag(bytes: &[u8]) -> String {
    let mut h = DefaultHasher::new();
    h.write(bytes);
    format!("{:016x}", h.finish())
}

/// A single cached `/models` response body plus its invalidation metadata.
struct Cached {
    /// Serialized `{"models": [...]}` catalog body.
    body: Bytes,
    /// Hex of route fingerprint + body hash (used as the ETag).
    etag: String,
    /// `DefaultHasher` over sorted (key, context, effort) route triples.
    fingerprint: u64,
    /// Resolved template source path, when the template came from a file.
    template_path: Option<PathBuf>,
    /// Modification time of `template_path`, when it exists.
    template_mtime: Option<SystemTime>,
    /// When this entry was last built; drives periodic re-discovery when
    /// `template_path` is `None`.
    checked_at: Instant,
}

/// Cache for the live `/models` catalog endpoint.
///
/// The cache is intentionally cheap and synchronous: `get` never awaits
/// inside the mutex, and a template reload failure degrades to the
/// from-scratch catalog (the `/models` endpoint must never fail the request
/// because the installed Codex template is temporarily unreadable).
pub struct CatalogCache {
    inner: Mutex<Option<Cached>>,
}

impl CatalogCache {
    /// Create an empty cache.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    /// Return `(etag, body)`, rebuilding when the route fingerprint or the
    /// template file mtime changed. Never fails; template load errors
    /// degrade to the from-scratch catalog with a warning log.
    pub fn get(
        &self,
        config: &tokio::sync::RwLockReadGuard<'_, ValidatedConfig>,
    ) -> (String, Bytes) {
        let fingerprint = fingerprint_config(config);

        // Fast path: reuse the cached body when neither the routes nor the
        // template file changed. The template_path from the previous call is
        // also what we stat; if the template was served by `codex debug
        // models` (no file), mtime is always None and only the fingerprint
        // keeps the cache fresh.
        let cached_hit = {
            let inner = self.inner.lock().unwrap();
            inner.as_ref().and_then(|cached| {
                let template_mtime = cached.template_path.as_ref().and_then(file_mtime);
                let recheck_due = cached.template_path.is_none()
                    && cached.checked_at.elapsed() >= TEMPLATE_RECHECK_INTERVAL;
                if cached.fingerprint == fingerprint
                    && cached.template_mtime == template_mtime
                    && !recheck_due
                {
                    Some((cached.etag.clone(), cached.body.clone()))
                } else {
                    None
                }
            })
        };
        if let Some(hit) = cached_hit {
            return hit;
        }

        // Slow path: reload the template (same discovery as gen-catalog),
        // rebuild the catalog, and store the result.
        let (template_path, template) = crate::catalog::reload_template();
        let template_mtime = template_path.as_ref().and_then(file_mtime);
        let generated = crate::catalog::build_catalog_value(config, template.as_ref());
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
        };
        *self.inner.lock().unwrap() = Some(cached);
        (etag, body)
    }
}

impl Default for CatalogCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Hash the routes in sorted key order into a fingerprint.
///
/// Only the fields that affect the generated catalog are fed in: key,
/// context window, and default reasoning effort (empty string when unset).
fn fingerprint_config(config: &ValidatedConfig) -> u64 {
    let mut h = DefaultHasher::new();
    let mut keys: Vec<&String> = config.routes.keys().collect();
    keys.sort();
    for key in keys {
        let route = &config.routes[key];
        h.write(key.as_bytes());
        h.write(&route.context_window.to_be_bytes());
        let effort = route.default_reasoning_effort.as_deref().unwrap_or("");
        h.write(effort.as_bytes());
    }
    h.finish()
}

/// File modification time, if the path exists and the OS reports one.
fn file_mtime(path: &PathBuf) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

/// Hex ETag: DefaultHasher over fingerprint + body bytes.
fn etag_for(fingerprint: u64, body: &[u8]) -> String {
    let mut h = DefaultHasher::new();
    h.write(&fingerprint.to_be_bytes());
    h.write(body);
    format!("{:016x}", h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ValidatedConfig};
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn parse_config_toml(toml: &str) -> ValidatedConfig {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, toml).unwrap();
        let raw = Config::parse_file(&path).unwrap();
        raw.validate().unwrap()
    }

    fn config_with_route(key: &str) -> ValidatedConfig {
        let toml = format!(
            r#"
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

    #[tokio::test]
    async fn stable_etag_and_body_when_nothing_changes() {
        let cfg = Arc::new(RwLock::new(config_with_route("ds/a")));
        let cache = CatalogCache::new();
        let guard = cfg.read().await;
        let (etag1, body1) = cache.get(&guard);
        let (etag2, body2) = cache.get(&guard);
        assert_eq!(etag1, etag2);
        assert_eq!(body1, body2);
    }

    #[tokio::test]
    async fn route_change_invalidates() {
        let cfg = Arc::new(RwLock::new(config_with_route("ds/a")));
        let cache = CatalogCache::new();
        let (etag1, body1) = cache.get(&cfg.read().await);
        *cfg.write().await = config_with_route("ds/b");
        let (etag2, body2) = cache.get(&cfg.read().await);
        assert_ne!(etag1, etag2);
        assert!(body2.windows(4).any(|w| w == b"ds/b"));
        assert!(!body1.windows(4).any(|w| w == b"ds/b"));
    }

    #[tokio::test]
    async fn non_file_template_is_rechecked_after_interval() {
        // Simulates a shell-out/no-file template source: template_path is None,
        // so the fast path cannot rely on mtime and must re-discover periodically.
        let cfg = Arc::new(RwLock::new(config_with_route("ds/a")));
        let cache = CatalogCache::new();
        let _ = cache.get(&cfg.read().await);

        // Fresh entry: a second get with the same fingerprint is a fast-path hit.
        let fresh_checked_at = {
            let inner = cache.inner.lock().unwrap();
            inner.as_ref().unwrap().checked_at
        };
        let _ = cache.get(&cfg.read().await);
        let after_fast_path = {
            let inner = cache.inner.lock().unwrap();
            inner.as_ref().unwrap().checked_at
        };
        assert_eq!(fresh_checked_at, after_fast_path);

        // Age the entry past the recheck interval; the next get must rebuild.
        {
            let mut inner = cache.inner.lock().unwrap();
            let cached = inner.as_mut().unwrap();
            cached.checked_at = std::time::Instant::now() - (std::time::Duration::from_secs(61));
        }
        let _ = cache.get(&cfg.read().await);
        let rebuilt_checked_at = {
            let inner = cache.inner.lock().unwrap();
            inner.as_ref().unwrap().checked_at
        };
        assert!(rebuilt_checked_at > fresh_checked_at);
    }
}
