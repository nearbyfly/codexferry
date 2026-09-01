//! Cache for the live `GET /v1/models` catalog response.
//!
//! The `/models` endpoint serves the same catalog the `gen-catalog`
//! subcommand writes to disk, but the server's routes can change via
//! hot-reload and the codex template file can change on disk. This module
//! keeps the serialized `{"models": [...]}` body plus an ETag, invalidating
//! when the route fingerprint or the template file's mtime changes.
//!
//! Stale-while-revalidate (spec §Goal): a fresh hit is served from memory
//! without any subprocess; a stale entry is returned immediately while a
//! single-flight background refresh updates the cache; a cold start
//! refreshes inline or waits for an in-flight refresh so a freshly-started
//! daemon always answers correctly.

use crate::config::ValidatedConfig;
use bytes::Bytes;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

/// How often a non-file template source is re-probed.
///
/// File-backed templates are invalidated by mtime, but a shell-out template
/// (or a temporary discovery failure) has no path to stat. The cache re-runs
/// template discovery on this cadence instead of never, so a Codex upgrade or
/// a transient failure cannot leave the catalog stale indefinitely. With hide
/// overrides enabled, file-backed entries re-probe on this cadence too: the
/// bundled catalog comes from a shell-out with no mtime to stat.
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
    /// `template_path` is `None` — or when hide overrides were appended,
    /// which re-probe even on a file-backed template.
    checked_at: Instant,
    /// The `hide_bundled_models` flag value at build time — drives the
    /// periodic re-probe even when the template is file-backed (re-probing
    /// also retries a transiently-failing discovery).
    hide_bundled: bool,
}

/// Cache for the live `/models` catalog endpoint.
///
/// The cache implements stale-while-revalidate (SWR): a fresh hit is served
/// from memory without any subprocess; a stale entry is returned immediately
/// while a single-flight background refresh updates the cache; a cold start
/// refreshes inline or waits for an in-flight refresh so a freshly-started
/// daemon always answers correctly (spec §Design). A template reload failure
/// degrades to the from-scratch catalog (the `/models` endpoint must never
/// fail the request because the installed Codex template is temporarily
/// unreadable).
pub struct CatalogCache {
    inner: Mutex<Option<Cached>>,
    /// Single-flight gate for background refreshes: try_lock decides
    /// "spawn the refresh" vs "one is already running" (spec §Design).
    refreshing: tokio::sync::Mutex<()>,
    /// Bundled-catalog discovery, injectable so unit tests never depend on
    /// the host's real codex binary (hide-bundled spec §Freshness).
    discovery: fn() -> Vec<serde_json::Value>,
}

impl CatalogCache {
    /// Create an empty cache.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
            refreshing: tokio::sync::Mutex::new(()),
            discovery: crate::catalog::discover_bundled_models,
        }
    }

    /// Return `(etag, body)` without holding a config guard across a
    /// subprocess call and without blocking the request on one except at
    /// cold start and config changes (reload-staleness spec §Design).
    /// - fast path: fresh cache hit;
    /// - config changed (stored fingerprint ≠ current): WAIT for a refresh
    ///   that reflects the new config — a fetch in this window must never
    ///   return the pre-change body, because codex PERSISTS the response
    ///   and would keep listing removed routes for its own 300s cache TTL;
    /// - stale path (fingerprint MATCHES, time-based staleness only): SWR —
    ///   return the old entry, spawn a single-flight background refresh;
    /// - cold start (no entry): an entry MUST exist when we leave. Win or
    ///   wait for the single-flight lock, then refresh, retrying with a
    ///   FORCED store when the fingerprint guard discards repeated
    ///   mid-refresh config writes (one possibly one-request-stale entry
    ///   beats a panicking handler; the next request's fingerprint check
    ///   re-triggers a fresh refresh).
    pub async fn get(self: &Arc<Self>, config: &crate::config::SharedConfig) -> (String, Bytes) {
        let fingerprint = fingerprint_config(&*config.read().await);
        if let Some(hit) = self.fast_hit(fingerprint) {
            return hit;
        }
        let entry_is_current = self
            .inner
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| c.fingerprint == fingerprint);
        match entry_is_current {
            None => {
                // Cold start (no entry): there is no stale body to serve, so
                // the request must end with a fresh entry. Win the
                // single-flight lock or wait for the current holder, then
                // refresh. A refresh is DISCARDED when a config write lands
                // mid-refresh (fingerprint guard); retry, and force the
                // store on every retry after the first so the loop always
                // terminates (PR #6 review issue 2).
                let _guard = self.single_flight().await;
                if let Some(hit) = self.stale_hit() {
                    return hit;
                }
                let mut force = false;
                loop {
                    self.refresh(config, force).await;
                    if let Some(hit) = self.stale_hit() {
                        return hit;
                    }
                    force = true;
                }
            }
            Some(true) => {
                // Fingerprint matches: the staleness is time-based only
                // (60s recheck / template mtime). SWR as designed — the old
                // body is the trade the SWR spec chose; a refresh runs in
                // the background. The single-flight try_lock lives INSIDE
                // the spawned task (an Arc owns everything, so the task is
                // 'static); losers exit immediately.
                let cache = Arc::clone(self);
                let config = Arc::clone(config);
                tokio::spawn(async move {
                    if let Ok(_guard) = cache.refreshing.try_lock() {
                        cache.refresh(&config, false).await;
                    }
                });
                self.stale_hit()
                    .expect("stale path checked inner.is_some(); nothing ever clears it")
            }
            Some(false) => {
                // Config changed since the entry was built: WAIT for a
                // refresh reflecting the new config instead of serving a
                // body the client will persist (reload-staleness spec
                // §Design B). Fall back to the stored body only as the last
                // resort (config changed AGAIN mid-refresh); the next
                // request re-detects and retries.
                self.refresh_if_stale(config).await;
                self.fast_hit(fingerprint).unwrap_or_else(|| {
                    self.stale_hit()
                        .expect("entry existed above; refresh never clears it")
                })
            }
        }
    }

    /// The single-flight guard: win `refreshing` or wait for the current
    /// holder. Shared by the cold path and [`Self::refresh_if_stale`].
    async fn single_flight(&self) -> tokio::sync::MutexGuard<'_, ()> {
        match self.refreshing.try_lock() {
            Ok(guard) => guard,
            Err(_) => self.refreshing.lock().await,
        }
    }

    /// Make the cached entry reflect `config`'s CURRENT fingerprint,
    /// refreshing synchronously when it does not.
    ///
    /// Used in two roles (reload-staleness spec §Design):
    /// - **A (proactive)**: the hot-reload applier spawns this right after
    ///   applying a config change, so the racing fetches that follow land
    ///   on an already-fresh entry instead of the pre-change body;
    /// - **B (wait)**: `get` calls it when a request arrives while the
    ///   entry is stale-by-config.
    ///
    /// Concurrent callers serialize on `refreshing`; each re-checks
    /// `fast_hit` after acquiring the lock, so a burst costs one refresh.
    pub(crate) async fn refresh_if_stale(self: &Arc<Self>, config: &crate::config::SharedConfig) {
        let fingerprint = fingerprint_config(&*config.read().await);
        if self.fast_hit(fingerprint).is_some() {
            return;
        }
        let _guard = self.single_flight().await;
        if self.fast_hit(fingerprint).is_some() {
            return;
        }
        let mut force = false;
        loop {
            self.refresh(config, force).await;
            if self.fast_hit(fingerprint).is_some() {
                return;
            }
            // A discarded refresh (config changed again mid-refresh) stores
            // nothing; force one store so the loop terminates. The stored
            // body may already be stale against an even newer config — the
            // next request's fingerprint check re-detects that.
            if force {
                return;
            }
            force = true;
        }
    }

    /// Fast-path check: cached entry fresh under this fingerprint and the
    /// template-mtime / recheck conditions.
    ///
    /// The template stat runs OUTSIDE the lock: the snapshot (small fields +
    /// body refcount) is taken under `inner`, the lock dropped, then the
    /// mtime fetched. The stat targets the codex template path, which can
    /// sit on a slow or hung mount; blocking while holding `inner` would
    /// stall every other `/models` request, including fresh hits. If a
    /// refresh replaces the entry after the snapshot, returning the
    /// snapshot's body is exactly the documented one-request-stale SWR
    /// tolerance.
    fn fast_hit(&self, fingerprint: u64) -> Option<(String, Bytes)> {
        let (etag, body, entry_fingerprint, template_path, cached_mtime, recheck_due) = {
            let inner = self.inner.lock().unwrap();
            let cached = inner.as_ref()?;
            let recheck_due = cached.checked_at.elapsed() >= TEMPLATE_RECHECK_INTERVAL
                && (cached.template_path.is_none() || cached.hide_bundled);
            (
                cached.etag.clone(),
                cached.body.clone(),
                cached.fingerprint,
                cached.template_path.clone(),
                cached.template_mtime,
                recheck_due,
            )
        };
        let template_mtime = template_path.as_ref().and_then(file_mtime);
        if entry_fingerprint == fingerprint && cached_mtime == template_mtime && !recheck_due {
            Some((etag, body))
        } else {
            None
        }
    }

    /// The cached entry as `(etag, body)`, if one exists.
    fn stale_hit(&self) -> Option<(String, Bytes)> {
        self.inner
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| (c.etag.clone(), c.body.clone()))
    }

    /// One refresh run: read the CURRENT config, join the two subprocess
    /// calls on blocking threads, build, and store only if the config has
    /// not changed since the read (fingerprint guard, spec §Design step 4).
    /// Subprocess failures degrade inside the rebuild exactly as the inline
    /// path always did and the result IS stored (spec §Design step 5).
    /// `force` skips the fingerprint guard — the cold-start last resort so
    /// an entry always exists (PR #6 review issue 2).
    async fn refresh(&self, config: &crate::config::SharedConfig, force: bool) {
        let (fingerprint, snapshot) = {
            let guard = config.read().await;
            (fingerprint_config(&guard), guard.clone())
        };
        let hide_bundled = snapshot.server.hide_bundled_models;
        let discovery = self.discovery;
        // Only run the discovery subprocess when hide_bundled is on (mirrors
        // the pre-SWR rebuild_inline behavior: the test asserts CALLS == 0
        // when the flag is off, and running discovery unconditionally would
        // also waste a subprocess on every refresh).
        let (template, bundled) = if hide_bundled {
            tokio::join!(
                tokio::task::spawn_blocking(crate::catalog::reload_template),
                tokio::task::spawn_blocking(discovery),
            )
        } else {
            let t = tokio::task::spawn_blocking(crate::catalog::reload_template).await;
            (t, Ok(Vec::new()))
        };
        // JoinError only on a panicking closure; degrade like any failure.
        let (template_path, template) = template.unwrap_or((None, None));
        let bundled = bundled.unwrap_or_default();
        let template_mtime = template_path.as_ref().and_then(file_mtime);
        let mut generated = crate::catalog::build_catalog_value(&snapshot, template.as_ref());
        if hide_bundled {
            if bundled.is_empty() {
                tracing::warn!(
                    "hide_bundled_models is on but `codex debug models --bundled` \
                     returned no models; Codex's bundled models stay visible"
                );
            } else {
                let route_keys: std::collections::HashSet<&str> =
                    snapshot.routes.keys().map(String::as_str).collect();
                let entries = crate::catalog::build_hide_entries(&bundled, &route_keys);
                if let Some(models) = generated.catalog["models"].as_array_mut() {
                    models.extend(entries);
                }
            }
        }
        let body = serde_json::to_vec(&generated.catalog)
            .map(Bytes::from)
            .unwrap_or_default();
        let etag = etag_for(fingerprint, &body);
        if !force && fingerprint_config(&*config.read().await) != fingerprint {
            return;
        }
        *self.inner.lock().unwrap() = Some(Cached {
            body: body.clone(),
            etag: etag.clone(),
            fingerprint,
            template_path,
            template_mtime,
            checked_at: Instant::now(),
            hide_bundled,
        });
    }
}

impl Default for CatalogCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Hash the routes in sorted key order into a fingerprint.
///
/// Only the fields that affect the served catalog body are fed in: key,
/// context window, default reasoning effort (empty string when unset), the
/// route description (folded into the entry's `description` by
/// `set_catalog_fields` — omitted it once and a description-only edit kept
/// serving the stale catalog forever, review E2), and the
/// `hide_bundled_models` flag.
fn fingerprint_config(config: &ValidatedConfig) -> u64 {
    let mut h = DefaultHasher::new();
    // The hide flag changes the served body; hashing it (re)builds the
    // cache when a hot-reload toggles it (spec §Freshness).
    h.write_u8(u8::from(config.server.hide_bundled_models));
    let mut keys: Vec<&String> = config.routes.keys().collect();
    keys.sort();
    for key in keys {
        let route = &config.routes[key];
        h.write(key.as_bytes());
        h.write(&route.context_window.to_be_bytes());
        let effort = route.default_reasoning_effort.as_deref().unwrap_or("");
        h.write(effort.as_bytes());
        let description = route.description.as_deref().unwrap_or("");
        h.write(description.as_bytes());
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
    use serde_json::Value;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn parse_config_toml(toml: &str) -> ValidatedConfig {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, toml).unwrap();
        let raw = Config::parse_file(&path).unwrap();
        raw.validate().unwrap()
    }

    /// Review E2: a description-only route edit must change the
    /// fingerprint. `set_catalog_fields` folds the description into the
    /// served entry, so omitting it from the hash kept fast_hit serving the
    /// stale catalog forever on the common config (file-backed template +
    /// hide off).
    #[test]
    fn fingerprint_distinguishes_route_description() {
        let toml_plain = r#"
[server]
hide_bundled_models = false
[providers.ds]
base_url = "http://x"
api_key = "k"
format = "chat"
[routes."ds/a"]
model = "m"
context_window = 1000
"#;
        let toml_desc = r#"
[server]
hide_bundled_models = false
[providers.ds]
base_url = "http://x"
api_key = "k"
format = "chat"
[routes."ds/a"]
model = "m"
context_window = 1000
description = "fast tier"
"#;
        let plain = fingerprint_config(&parse_config_toml(toml_plain));
        let described = fingerprint_config(&parse_config_toml(toml_desc));
        assert_ne!(plain, described, "description-only edit must invalidate");
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
        let cache = Arc::new(CatalogCache::new());
        let (etag1, body1) = cache.get(&cfg).await;
        let (etag2, body2) = cache.get(&cfg).await;
        assert_eq!(etag1, etag2);
        assert_eq!(body1, body2);
    }

    #[tokio::test]
    async fn route_change_invalidates() {
        let cfg = Arc::new(RwLock::new(config_with_route("ds/a")));
        let cache = Arc::new(CatalogCache::new());
        let (etag1, body1) = cache.get(&cfg).await;
        *cfg.write().await = config_with_route("ds/b");
        // SWR: the stale get returns the OLD entry; the spawned refresh
        // builds the new one in the background. Wait for it, then a fresh
        // get observes the change.
        let _ = cache.get(&cfg).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = cache.refreshing.lock().await;
        let (etag2, body2) = cache.get(&cfg).await;
        assert_ne!(etag1, etag2);
        assert!(body2.windows(4).any(|w| w == b"ds/b"));
        assert!(!body1.windows(4).any(|w| w == b"ds/b"));
    }

    #[tokio::test]
    async fn non_file_template_is_rechecked_after_interval() {
        // Simulates a shell-out/no-file template source: template_path is None,
        // so the fast path cannot rely on mtime and must re-discover periodically.
        let cfg = Arc::new(RwLock::new(config_with_route("ds/a")));
        let cache = Arc::new(CatalogCache::new());
        let _ = cache.get(&cfg).await;

        // Fresh entry: a second get with the same fingerprint is a fast-path hit.
        let fresh_checked_at = {
            let inner = cache.inner.lock().unwrap();
            inner.as_ref().unwrap().checked_at
        };
        let _ = cache.get(&cfg).await;
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
        // SWR: the stale get spawns a background refresh; wait for it.
        let _ = cache.get(&cfg).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = cache.refreshing.lock().await;
        let rebuilt_checked_at = {
            let inner = cache.inner.lock().unwrap();
            inner.as_ref().unwrap().checked_at
        };
        assert!(rebuilt_checked_at > fresh_checked_at);
    }
    fn config_with_flag(key: &str, hide: bool) -> ValidatedConfig {
        let toml = format!(
            r#"
[server]
hide_bundled_models = {hide}
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

    fn marker_discovery() -> Vec<Value> {
        vec![serde_json::json!({"slug": "gpt-marker", "visibility": "list"})]
    }

    #[tokio::test]
    async fn hide_flag_off_serves_no_hide_entries_and_never_discovers() {
        static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        fn counting() -> Vec<Value> {
            CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            marker_discovery()
        }
        let cfg = Arc::new(RwLock::new(config_with_flag("ds/a", false)));
        let cache = Arc::new(CatalogCache {
            inner: Mutex::new(None),
            refreshing: Default::default(),
            discovery: counting,
        });
        let (_, body) = cache.get(&cfg).await;
        assert!(
            !body.windows(10).any(|w| w == b"gpt-marker"),
            "flag off must not append hide entries"
        );
        assert_eq!(
            CALLS.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "discovery must not run when the flag is off"
        );
    }

    #[tokio::test]
    async fn hide_flag_on_appends_hide_entries() {
        let cfg = Arc::new(RwLock::new(config_with_flag("ds/a", true)));
        let cache = Arc::new(CatalogCache {
            inner: Mutex::new(None),
            refreshing: Default::default(),
            discovery: marker_discovery,
        });
        let (etag, body) = cache.get(&cfg).await;
        let v: Value = serde_json::from_slice(&body).unwrap();
        let models = v["models"].as_array().expect("models array");
        let marker = models
            .iter()
            .find(|m| m["slug"] == "gpt-marker")
            .expect("hide entry appended");
        assert_eq!(marker["visibility"], "hide");
        assert!(
            models
                .iter()
                .any(|m| m["slug"] == "ds/a" && m["visibility"] == "list"),
            "route entries stay list-visible"
        );
        assert!(!etag.is_empty());
    }

    #[tokio::test]
    async fn hide_flag_toggle_invalidates_cache() {
        let cfg = Arc::new(RwLock::new(config_with_flag("ds/a", false)));
        let cache = Arc::new(CatalogCache {
            inner: Mutex::new(None),
            refreshing: Default::default(),
            discovery: marker_discovery,
        });
        let (etag1, body1) = cache.get(&cfg).await;
        *cfg.write().await = config_with_flag("ds/a", true);
        // SWR: the stale get returns the OLD entry; the spawned refresh
        // builds the new one in the background. Wait for it, then a fresh
        // get observes the change.
        let _ = cache.get(&cfg).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = cache.refreshing.lock().await;
        let (etag2, body2) = cache.get(&cfg).await;
        assert_ne!(etag1, etag2, "flag toggle must invalidate the cache");
        assert!(!body1.windows(10).any(|w| w == b"gpt-marker"));
        assert!(body2.windows(10).any(|w| w == b"gpt-marker"));
    }

    #[tokio::test]
    async fn hide_with_file_template_reprobes_after_interval() {
        // Hide entries come from a shell-out source with no mtime, so the
        // 60s re-probe must fire even when the template is file-backed
        // (spec §Freshness).
        static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        fn counting() -> Vec<Value> {
            CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            marker_discovery()
        }
        let cfg = Arc::new(RwLock::new(config_with_flag("ds/a", true)));
        let cache = Arc::new(CatalogCache {
            inner: Mutex::new(None),
            refreshing: Default::default(),
            discovery: counting,
        });
        let _ = cache.get(&cfg).await;
        {
            let mut inner = cache.inner.lock().unwrap();
            let cached = inner.as_mut().unwrap();
            // Simulate a file-backed template + an aged entry.
            cached.template_path = Some(PathBuf::from("/nonexistent-template.json"));
            // Force template_mtime to None so the mtime comparison stays
            // None == None: a stale mtime would trigger the rebuild via the
            // mtime-mismatch branch, not the recheck_due branch this test
            // targets (and on a host with a real file-backed template the
            // first build stores a real mtime, so this must be explicit).
            cached.template_mtime = None;
            cached.checked_at =
                std::time::Instant::now() - (TEMPLATE_RECHECK_INTERVAL + Duration::from_secs(1));
        }
        // SWR: the stale get spawns a background refresh; wait for it.
        let _ = cache.get(&cfg).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = cache.refreshing.lock().await;
        assert_eq!(
            CALLS.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "aged entry with hide on must rebuild despite a file-backed template"
        );
    }

    /// Reload-staleness spec §Design A: `refresh_if_stale` alone (no
    /// request involved — the role the applier's ReloadHook plays) must
    /// bring the entry to the new config's fingerprint.
    #[tokio::test]
    async fn refresh_if_stale_syncs_without_a_request() {
        let cfg = Arc::new(RwLock::new(config_with_flag("ds/a", true)));
        let cache = Arc::new(CatalogCache {
            inner: Mutex::new(None),
            refreshing: Default::default(),
            discovery: marker_discovery,
        });
        let _ = cache.get(&cfg).await; // ds/a entry
        *cfg.write().await = config_with_flag("ds/b", true);
        cache.refresh_if_stale(&cfg).await;
        let fp_b = fingerprint_config(&*cfg.read().await);
        let (etag, body) = cache.get(&cfg).await;
        assert!(
            body.windows(4).any(|w| w == b"ds/b"),
            "refresh_if_stale must have synced the entry to ds/b"
        );
        // The get after the sync is a pure fast hit: same etag the synced
        // entry produced, no re-discovery observable through the body.
        let _ = etag;
    }

    /// Scenario matrix (spec S1–S5): the fingerprint changes exactly for
    /// the edits whose outcome is catalog-visible, and — critically — NOT
    /// for an upstream `model=`-only change or a routeless provider, whose
    /// bodies are identical (model=/providers never enter the body).
    #[test]
    fn fingerprint_visibility_matrix() {
        let toml_with = |route_line: &str| {
            format!(
                r#"
[server]
hide_bundled_models = false
[providers.ds]
base_url = "http://x"
api_key = "k"
format = "chat"
[routes."ds/a"]
{route_line}
"#
            )
        };
        let base = fingerprint_config(&parse_config_toml(&toml_with(
            "model = \"m\"\ncontext_window = 1000",
        )));
        // S1/S5: a new route key (added provider+model or plain route) is visible.
        let added = fingerprint_config(&parse_config_toml(&format!(
            "{}\n[routes.\"ds/b\"]\nmodel = \"m\"\ncontext_window = 1000",
            toml_with("model = \"m\"\ncontext_window = 1000")
        )));
        assert_ne!(
            base, added,
            "S1/S5: added route must change the fingerprint"
        );
        // S2a: upstream model=-only change — catalog-invisible by design.
        let model_changed = fingerprint_config(&parse_config_toml(&toml_with(
            "model = \"other-upstream\"\ncontext_window = 1000",
        )));
        assert_eq!(
            base, model_changed,
            "S2a: upstream model= is not catalog-visible; no invalidation"
        );
        // S2b: description / effort / context_window edits are visible.
        for line in [
            "model = \"m\"\ncontext_window = 1000\ndescription = \"fast\"",
            "model = \"m\"\ncontext_window = 1000\ndefault_reasoning_effort = \"high\"",
            "model = \"m\"\ncontext_window = 2000",
        ] {
            let fp = fingerprint_config(&parse_config_toml(&toml_with(line)));
            assert_ne!(base, fp, "S2b: {line} must change the fingerprint");
        }
        // S4: a routeless provider is invisible.
        let provider_only = fingerprint_config(&parse_config_toml(
            r#"
[server]
hide_bundled_models = false
[providers.ds]
base_url = "http://x"
api_key = "k"
format = "chat"
[providers.extra]
base_url = "http://y"
api_key = "k"
format = "responses"
[routes."ds/a"]
model = "m"
context_window = 1000
"#,
        ));
        assert_eq!(
            base, provider_only,
            "S4: routeless provider must not change the fingerprint"
        );
    }

    #[tokio::test]
    async fn config_change_waits_for_fresh_body() {
        // Reload-staleness spec §Design B: a get arriving after a config
        // change must NOT return the pre-change body (codex persists it).
        // The gate stays OPEN — the refresh completes quickly and the get
        // returns the NEW body.
        static GATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
        fn gated() -> Vec<Value> {
            while !GATE.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            marker_discovery()
        }
        let cfg = Arc::new(RwLock::new(config_with_flag("ds/a", true)));
        let cache = Arc::new(CatalogCache {
            inner: Mutex::new(None),
            refreshing: Default::default(),
            discovery: gated,
        });
        let (etag1, _) = cache.get(&cfg).await; // cold: ds/a
        *cfg.write().await = config_with_flag("ds/b", true);
        let (etag2, body2) = cache.get(&cfg).await;
        assert_ne!(etag2, etag1, "get must not serve the pre-change body");
        assert!(
            body2.windows(4).any(|w| w == b"ds/b"),
            "get after a config change must reflect the new config"
        );
    }

    /// The SWR trade survives only for TIME-based staleness: with the
    /// fingerprint UNCHANGED (60s recheck coming due), the aged get returns
    /// the old body immediately while a gated background refresh runs.
    #[tokio::test]
    async fn time_stale_get_returns_immediately_and_refreshes_in_background() {
        static GATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
        fn gated() -> Vec<Value> {
            while !GATE.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            marker_discovery()
        }
        let cfg = Arc::new(RwLock::new(config_with_flag("ds/a", true)));
        let cache = Arc::new(CatalogCache {
            inner: Mutex::new(None),
            refreshing: Default::default(),
            discovery: gated,
        });
        let (etag1, _) = cache.get(&cfg).await; // cold: fresh build (gate open)
        {
            // Age the entry WITHOUT changing the config: recheck due, but
            // the fingerprint still matches.
            let mut inner = cache.inner.lock().unwrap();
            inner.as_mut().unwrap().checked_at =
                std::time::Instant::now() - (TEMPLATE_RECHECK_INTERVAL + Duration::from_secs(1));
        }
        GATE.store(false, std::sync::atomic::Ordering::SeqCst);
        let start = std::time::Instant::now();
        let (etag2, _) = cache.get(&cfg).await;
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "time-stale get must not wait for the gated refresh (took {:?})",
            start.elapsed()
        );
        assert_eq!(
            etag2, etag1,
            "time-stale get must return the OLD body's etag"
        );
        // Let the background refresh finish, then the next get is fresh again.
        GATE.store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = cache.refreshing.lock().await;
        let (_, body3) = cache.get(&cfg).await;
        assert!(
            body3.windows(4).any(|w| w == b"ds/a"),
            "rebuilt body must still serve the unchanged config"
        );
    }

    #[tokio::test]
    async fn single_flight_merges_concurrent_config_changed_gets() {
        // Reload-staleness spec: after a config change the concurrent gets
        // WAIT for the refresh, so the gate must stay open (a closed gate
        // would deadlock waiters against the join below). The single-flight
        // contract shows up as exactly ONE refresh serving all eight.
        #![allow(unused_must_use)]
        static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        fn counting() -> Vec<Value> {
            CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            marker_discovery()
        }
        let cfg = Arc::new(RwLock::new(config_with_flag("ds/a", true)));
        let cache = Arc::new(CatalogCache {
            inner: Mutex::new(None),
            refreshing: Default::default(),
            discovery: counting,
        });
        let _ = cache.get(&cfg).await; // cold build: CALLS == 1
        *cfg.write().await = config_with_flag("ds/b", true);
        let mut joins = Vec::new();
        for _ in 0..8 {
            let c = Arc::clone(&cache);
            let cfg2 = Arc::clone(&cfg);
            joins.push(tokio::spawn(async move { c.get(&cfg2).await }));
        }
        let mut new_bodies = 0;
        for j in joins {
            let (_, body) = j.await.unwrap();
            assert!(
                body.windows(4).any(|w| w == b"ds/b"),
                "every concurrent get must reflect the new config"
            );
            new_bodies += 1;
        }
        assert_eq!(new_bodies, 8);
        // Exactly one extra refresh: the single-flight winner. Every loser
        // re-checks fast_hit after acquiring the lock and returns without
        // discovering.
        assert_eq!(
            CALLS.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "1 cold + 1 shared refresh; single-flight must not run one per concurrent get"
        );
    }

    #[tokio::test]
    async fn refresh_discarded_mid_refresh_converges_on_next_wait() {
        // Reload-staleness spec: a refresh whose config snapshot was
        // superseded mid-flight is DISCARDED by the fingerprint guard. Under
        // the wait semantics the requester does not return stale — it loops
        // (forced second refresh) and converges to the newest config.
        // Q3 fallback: even if the loop's last store still mismatches (a
        // further write raced), the requester serves the stored body rather
        // than failing — here the loop converges, so the body IS ds/c.
        static GATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
        fn gated() -> Vec<Value> {
            while !GATE.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            marker_discovery()
        }
        let cfg = Arc::new(RwLock::new(config_with_flag("ds/a", true)));
        let cache = Arc::new(CatalogCache {
            inner: Mutex::new(None),
            refreshing: Default::default(),
            discovery: gated,
        });
        let _ = cache.get(&cfg).await; // ds/a entry (gate open)
        GATE.store(false, std::sync::atomic::Ordering::SeqCst);
        *cfg.write().await = config_with_flag("ds/b", true);
        // The changed-config get now WAITS inside refresh_if_stale: run it
        // as a task so the test can interleave the mid-refresh write.
        let task = {
            let c = Arc::clone(&cache);
            let cfg2 = Arc::clone(&cfg);
            tokio::spawn(async move { c.get(&cfg2).await })
        };
        // Yield so the spawned task polls, wins the single-flight lock and
        // reads its config snapshot (ds/b) BEFORE the mid-refresh write;
        // under current_thread the spawned task is not polled between the
        // spawn and this yield.
        tokio::task::yield_now().await;
        *cfg.write().await = config_with_flag("ds/c", true); // mid-refresh change
        GATE.store(true, std::sync::atomic::Ordering::SeqCst);
        // Refresh #1 (ds/b snapshot) completes and is discarded; the
        // requester's forced second refresh stores ds/c and the get returns.
        let (_, body) = task.await.unwrap();
        assert!(
            body.windows(4).any(|w| w == b"ds/c"),
            "the waiting get must converge to the newest config, got: {}",
            String::from_utf8_lossy(&body)
        );
        // Converged state is stable: a further get is a fast hit.
        let (_, body2) = cache.get(&cfg).await;
        assert!(body2.windows(4).any(|w| w == b"ds/c"));
    }

    #[tokio::test]
    async fn cold_concurrent_second_waits_for_inflight_refresh() {
        static GATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        fn gated() -> Vec<Value> {
            while !GATE.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            marker_discovery()
        }
        let cfg = Arc::new(RwLock::new(config_with_flag("ds/a", true)));
        let cache = Arc::new(CatalogCache {
            inner: Mutex::new(None),
            refreshing: Default::default(),
            discovery: gated,
        });
        // First cold get: acquires the single-flight lock, blocks in the
        // gated refresh.
        let first = tokio::spawn({
            let c = Arc::clone(&cache);
            let cfg2 = Arc::clone(&cfg);
            async move { c.get(&cfg2).await }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        // Second cold get: no entry, refresh in flight -> must WAIT, not
        // error or serve nothing.
        let mut second = Box::pin(cache.get(&cfg));
        assert!(
            tokio::time::timeout(Duration::from_millis(300), second.as_mut())
                .await
                .is_err(),
            "second cold get must wait for the in-flight refresh"
        );
        GATE.store(true, std::sync::atomic::Ordering::SeqCst);
        let (_, body2) = second.await;
        assert!(body2.windows(4).any(|w| w == b"ds/a"));
        let _ = first.await.unwrap();
    }
    #[tokio::test]
    /// Review-fix regression (PR #6 review issue 2): a cold start whose
    /// EVERY refresh is discarded by the fingerprint guard (config written
    /// mid-refresh, repeatedly) must still answer - the retry falls back to
    /// a FORCED store instead of hitting stale_entry's expect panic.
    async fn cold_start_survives_repeated_mid_refresh_config_writes() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        // Token gate: every discovery call blocks until the harness
        // releases exactly this call, so config writes land
        // deterministically mid-refresh.
        static GATE: AtomicBool = AtomicBool::new(false);
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        fn token_gated() -> Vec<Value> {
            CALLS.fetch_add(1, Ordering::SeqCst);
            while !GATE.swap(false, Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            marker_discovery()
        }
        async fn release_with_flip(
            cfg: Arc<RwLock<crate::config::ValidatedConfig>>,
            n: usize,
            route: String,
        ) {
            while CALLS.load(Ordering::SeqCst) < n {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            *cfg.write().await = config_with_flag(&route, true);
            GATE.store(true, Ordering::SeqCst);
        }

        let cfg = Arc::new(RwLock::new(config_with_flag("ds/a", true)));
        let cache = Arc::new(CatalogCache {
            inner: Mutex::new(None),
            refreshing: Default::default(),
            discovery: token_gated,
        });
        let getter = tokio::spawn({
            let c = Arc::clone(&cache);
            let cfg2 = Arc::clone(&cfg);
            async move { c.get(&cfg2).await }
        });
        // refresh #1 (ds/a) is discarded by the ds/b write mid-refresh ...
        release_with_flip(Arc::clone(&cfg), 1, "ds/b".to_string()).await;
        // ... and refresh #2 (ds/b) ALSO races a config write (ds/c lands
        // mid-refresh): the retry is FORCED, so it stores the ds/b snapshot
        // and the getter answers instead of panicking (pre-fix code hit
        // stale_entry's expect right here).
        release_with_flip(Arc::clone(&cfg), 2, "ds/c".to_string()).await;
        let (etag1, body1) = getter
            .await
            .expect("cold get must not panic under repeated discards");
        // Forced store serves the ds/b snapshot: one-request-stale by
        // design; the next request's fingerprint check refreshes to ds/c.
        assert!(body1.windows(4).any(|w| w == b"ds/b"));
        assert!(!etag1.is_empty());
        // Convergence: under the wait semantics the next get (fingerprint
        // mismatch) BLOCKS on its refresh, so the gate token must be
        // released from a helper task — main is inside the get and cannot
        // release it itself (self-deadlock under the old stale-return
        // harness). The get then converges straight to ds/c.
        let releaser = tokio::spawn(release_with_flip(Arc::clone(&cfg), 3, "ds/c".to_string()));
        let (_, body2) = cache.get(&cfg).await;
        assert!(
            body2.windows(4).any(|w| w == b"ds/c"),
            "waiting get must converge to the newest config"
        );
        releaser.await.unwrap();
        // Converged state is stable: a further get is a fast hit (no gate
        // token needed).
        let (etag3, body3) = cache.get(&cfg).await;
        assert!(body3.windows(4).any(|w| w == b"ds/c"));
        assert_ne!(etag1, etag3);
    }
}
