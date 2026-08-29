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

    /// Return `(etag, body)`, never blocking on a subprocess on the request
    /// path and never holding a config guard across one (spec §Goal).
    /// - fast path: fresh cache hit;
    /// - stale path: return the old entry, spawn a single-flight background
    ///   refresh;
    /// - cold start (no entry): refresh inline, or WAIT for an in-flight
    ///   refresh (a daemon that just started must answer correctly).
    pub async fn get(self: &Arc<Self>, config: &crate::config::SharedConfig) -> (String, Bytes) {
        let fingerprint = fingerprint_config(&*config.read().await);
        if let Some(hit) = self.fast_hit(fingerprint) {
            return hit;
        }
        if self.inner.lock().unwrap().is_some() {
            // Stale-while-revalidate. The single-flight try_lock lives
            // INSIDE the spawned task (an Arc owns everything, so the task
            // is 'static); losers exit immediately.
            let cache = Arc::clone(self);
            let config = Arc::clone(config);
            tokio::spawn(async move {
                if let Ok(_guard) = cache.refreshing.try_lock() {
                    cache.refresh(&config).await;
                }
            });
            self.stale_entry()
        } else if let Ok(_guard) = self.refreshing.try_lock() {
            // Cold start, we are first: refresh inline while holding the
            // single-flight lock.
            self.refresh(config).await;
            self.stale_entry()
        } else {
            // Cold start, someone else is refreshing: wait for them, then
            // serve their entry — or refresh ourselves if theirs failed to
            // produce one (spec §Cold start).
            let _guard = self.refreshing.lock().await;
            if let Some(hit) = self.fast_hit(fingerprint_config(&*config.read().await)) {
                return hit;
            }
            self.refresh(config).await;
            self.stale_entry()
        }
    }

    /// Fast-path check: cached entry fresh under this fingerprint and the
    /// template-mtime / recheck conditions. Pure memory, std-Mutex only.
    fn fast_hit(&self, fingerprint: u64) -> Option<(String, Bytes)> {
        let inner = self.inner.lock().unwrap();
        inner.as_ref().and_then(|cached| {
            let template_mtime = cached.template_path.as_ref().and_then(file_mtime);
            let recheck_due = cached.checked_at.elapsed() >= TEMPLATE_RECHECK_INTERVAL
                && (cached.template_path.is_none() || cached.hide_bundled);
            if cached.fingerprint == fingerprint
                && cached.template_mtime == template_mtime
                && !recheck_due
            {
                Some((cached.etag.clone(), cached.body.clone()))
            } else {
                None
            }
        })
    }

    /// The cached entry as `(etag, body)`. Only called on paths that have
    /// just stored an entry.
    fn stale_entry(&self) -> (String, Bytes) {
        self.inner
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| (c.etag.clone(), c.body.clone()))
            .expect("stale/cold path requires a cached entry after rebuild")
    }

    /// One refresh run: read the CURRENT config, join the two subprocess
    /// calls on blocking threads, build, and store only if the config has
    /// not changed since the read (fingerprint guard, spec §Design step 4).
    /// Subprocess failures degrade inside the rebuild exactly as the inline
    /// path always did and the result IS stored (spec §Design step 5).
    async fn refresh(&self, config: &crate::config::SharedConfig) {
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
                tokio::task::spawn_blocking(move || discovery()),
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
        if fingerprint_config(&*config.read().await) != fingerprint {
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
/// context window, default reasoning effort (empty string when unset), and
/// the `hide_bundled_models` flag.
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

    #[tokio::test]
    async fn stale_entry_returns_immediately_and_refreshes_in_background() {
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
                                                // Fingerprint change -> stale; close the gate so refresh cannot finish.
        GATE.store(false, std::sync::atomic::Ordering::SeqCst);
        *cfg.write().await = config_with_flag("ds/b", true);
        let start = std::time::Instant::now();
        let (etag2, _) = cache.get(&cfg).await;
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "stale get must not wait for the gated refresh (took {:?})",
            start.elapsed()
        );
        assert_eq!(etag2, etag1, "stale get must return the OLD body's etag");
        // Let the background refresh finish, then the next get is fresh.
        GATE.store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = cache.refreshing.lock().await;
        let (etag3, body3) = cache.get(&cfg).await;
        assert_ne!(etag3, etag1);
        assert!(
            body3.windows(4).any(|w| w == b"ds/b"),
            "refreshed body must contain ds/b"
        );
    }

    #[tokio::test]
    async fn single_flight_merges_concurrent_stale_gets() {
        static GATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
        static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        fn counting_gated() -> Vec<Value> {
            CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            while !GATE.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            marker_discovery()
        }
        let cfg = Arc::new(RwLock::new(config_with_flag("ds/a", true)));
        let cache = Arc::new(CatalogCache {
            inner: Mutex::new(None),
            refreshing: Default::default(),
            discovery: counting_gated,
        });
        let _ = cache.get(&cfg).await; // cold build: CALLS == 1
        GATE.store(false, std::sync::atomic::Ordering::SeqCst);
        *cfg.write().await = config_with_flag("ds/b", true);
        let mut joins = Vec::new();
        for _ in 0..8 {
            let c = Arc::clone(&cache);
            let cfg2 = Arc::clone(&cfg);
            joins.push(tokio::spawn(async move { c.get(&cfg2).await }));
        }
        for j in joins {
            let _ = j.await.unwrap();
        }
        assert_eq!(
            CALLS.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "exactly one background refresh may run (1 cold + 1 refresh)"
        );
        GATE.store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = cache.refreshing.lock().await;
        assert_eq!(CALLS.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn refresh_result_discarded_when_config_changes_mid_refresh() {
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
        let (etag1, _) = cache.get(&cfg).await; // ds/a entry
        GATE.store(false, std::sync::atomic::Ordering::SeqCst);
        *cfg.write().await = config_with_flag("ds/b", true); // triggers refresh #1
        let _ = cache.get(&cfg).await; // spawns refresh (gated)
                                       // Yield so the spawned task polls and reads the config snapshot
                                       // (ds/b) BEFORE the test's mid-refresh write; under current_thread
                                       // the spawned task is not polled between the spawn and this yield.
        tokio::task::yield_now().await;
        *cfg.write().await = config_with_flag("ds/c", true); // mid-refresh change
        GATE.store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = cache.refreshing.lock().await; // refresh #1 completes -> discarded
                                               // Entry still holds ds/a; this get returns stale and triggers
                                               // refresh #2 (gate now open, completes fast).
        let (etag_stale, _) = cache.get(&cfg).await;
        assert_eq!(
            etag_stale, etag1,
            "discarded refresh must leave the old entry"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = cache.refreshing.lock().await;
        let (_, body3) = cache.get(&cfg).await;
        assert!(
            body3.windows(4).any(|w| w == b"ds/c"),
            "second refresh must converge to ds/c"
        );
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
}
