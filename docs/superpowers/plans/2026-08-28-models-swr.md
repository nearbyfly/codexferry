# /v1/models Stale-While-Revalidate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the `/v1/models` request path never block on a subprocess and never hold the config lock across one, serving stale entries while a single-flight background refresh rebuilds.

**Architecture:** `CatalogCache::get` becomes async taking `&SharedConfig` (Task 1: snapshot scoping, rebuild still inline; Task 2: stale-while-revalidate state machine + `tokio::spawn`'d refresh guarded by a `tokio::sync::Mutex` single-flight, fingerprint-guarded store, concurrent `spawn_blocking` joins; Task 3: thread naming + docs + regression).

**Tech Stack:** Rust, tokio (`spawn_blocking`, `join!`, `sync::Mutex`), serde_json; no new dependencies.

**Spec:** `docs/superpowers/specs/2026-08-28-models-swr-design.md` — read it first; this plan argues from it.

## Global Constraints

- No new crate dependencies (spec: existing tokio features only).
- The injected `discovery: fn() -> Vec<serde_json::Value>` seam stays; unit tests never depend on the host's real `codex` binary.
- Degradation semantics unchanged: missing template → from-scratch catalog; empty bundled → hide off + `warn!`; these rebuilds ARE stored (spec §Design step 5).
- Cold start must never error: no entry + refresh in flight → wait, then serve or refresh inline (spec §Cold start).
- Tracing format strings use `{field}` placeholders, never positional `{}` (AGENTS.md #3).
- Comments in English; update AGENTS.md §12 and ARCHITECTURE.md §11 in the same change (AGENTS.md #13).
- Every task ends with `cargo test` for the touched module green + a commit; branch off main.

---

### Task 1: Async `get` with snapshot scoping (behavior-preserving)

**Files:**
- Modify: `src/models_cache.rs` (`get` at ~line 70, `Cached` unchanged)
- Modify: `src/proxy/mod.rs:405-431` (`handle_models` Some-branch)

**Interfaces:**
- Consumes: `crate::proxy::SharedConfig` (= `Arc<tokio::sync::RwLock<ValidatedConfig>>`, already `pub`).
- Produces: `pub async fn get(&self, config: &SharedConfig) -> (String, Bytes)`; private `fn fast_hit(&self, fingerprint: u64) -> Option<(String, Bytes)>`, `fn stale_entry(&self) -> (String, Bytes)`, `fn rebuild_inline(&self, fingerprint: u64, snapshot: &ValidatedConfig)`. Task 2 replaces `rebuild_inline` with `async fn refresh`.

- [ ] **Step 1: Convert the existing tests to the new call form (red = compile error)**

In `src/models_cache.rs` `mod tests`, every `cache.get(&cfg.read().await)` becomes `cache.get(&cfg).await` (the guard moves inside `get`). The affected tests: `stable_etag_and_body_when_nothing_changes`, `route_change_invalidates`, `non_file_template_is_rechecked_after_interval`, `hide_flag_off_serves_no_hide_entries_and_never_discovers`, `hide_flag_on_appends_hide_entries`, `hide_flag_toggle_invalidates_cache`, `hide_with_file_template_reprobes_after_interval`. Example (from `stable_etag_and_body_when_nothing_changes`):

```rust
        let (etag1, body1) = cache.get(&cfg).await;
        let (etag2, body2) = cache.get(&cfg).await;
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test models_cache::`
Expected: FAIL — `get` still takes `&RwLockReadGuard` (compile error).

- [ ] **Step 3: Implement the async signature + snapshot scoping**

Replace `CatalogCache::get` in `src/models_cache.rs` with:

```rust
    /// Return `(etag, body)`. Fast path: pure memory. Miss/expired: an
    /// inline rebuild from a config SNAPSHOT — the read guard is dropped
    /// before any subprocess call, so the config applier's write lock is
    /// never queued behind discovery (spec §Problem). NOTE: in this
    /// interim form the rebuild still blocks the worker; Task 2 moves it
    /// to a background task.
    pub async fn get(&self, config: &crate::proxy::SharedConfig) -> (String, Bytes) {
        let (fingerprint, snapshot) = {
            let guard = config.read().await;
            (fingerprint_config(&guard), guard.clone())
        };
        if let Some(hit) = self.fast_hit(fingerprint) {
            return hit;
        }
        self.rebuild_inline(fingerprint, &snapshot);
        self.stale_entry()
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
```

And `rebuild_inline` — today's slow-path body, but reading from the snapshot instead of a guard:

```rust
    /// Inline rebuild (Task 1 interim form; Task 2 replaces this with the
    /// background `refresh`). Blocking; caller must not hold any config
    /// guard (spec §Design).
    fn rebuild_inline(&self, fingerprint: u64, snapshot: &ValidatedConfig) {
        let (template_path, template) = crate::catalog::reload_template();
        let template_mtime = template_path.as_ref().and_then(file_mtime);
        let mut generated = crate::catalog::build_catalog_value(snapshot, template.as_ref());
        let hide_bundled = snapshot.server.hide_bundled_models;
        if hide_bundled {
            let bundled = (self.discovery)();
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
```

In `src/proxy/mod.rs` `handle_models`, the Some-branch becomes (drop its own guard usage):

```rust
        Some(v) => {
            observe_client_version(&state, &v);
            let (etag, body) = state.models.get(&state.config).await;
```

(Delete the now-unused `let config = state.config.read().await;` in that branch; the None branch keeps its own guard.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test models_cache::` — Expected: PASS (7 tests, semantics unchanged).
Run: `cargo test --test endpoints_metrics` — Expected: PASS (integration unaffected).

- [ ] **Step 5: Commit**

```bash
git add src/models_cache.rs src/proxy/mod.rs
git commit -m "refactor(models-cache): async get over a config snapshot - read guard never spans a subprocess"
```

---

### Task 2: Stale-while-revalidate + single-flight background refresh

**Files:**
- Modify: `src/models_cache.rs` (struct + `get`; delete `rebuild_inline`, add `refresh`)
- Modify: `src/proxy/mod.rs:214` and `:359` (AppState field `Arc`-wraps the cache)
- Test: `src/models_cache.rs` `mod tests`

**Interfaces:**
- Consumes: Task 1's `fast_hit` / `stale_entry` / snapshot pattern.
- Produces: `pub async fn get(self: &Arc<Self>, config: &SharedConfig) -> (String, Bytes)`; private `async fn refresh(&self, config: &SharedConfig)`; struct field `refreshing: tokio::sync::Mutex<()>`. `AppState.models: Arc<crate::models_cache::CatalogCache>`.

- [ ] **Step 1: Write the failing tests**

Append to `mod tests` (keep the existing `use` lines; the gated-discovery pattern keeps each test's statics inside its own fn body so parallel tests cannot interfere):

```rust
    #[tokio::test]
    async fn stale_entry_returns_immediately_and_refreshes_in_background() {
        static GATE: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(true);
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
        cache.refreshing.lock().await;
        let (etag3, body3) = cache.get(&cfg).await;
        assert_ne!(etag3, etag1);
        assert!(
            body3.windows(4).any(|w| w == b"ds/b"),
            "refreshed body must contain ds/b"
        );
    }

    #[tokio::test]
    async fn single_flight_merges_concurrent_stale_gets() {
        static GATE: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(true);
        static CALLS: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);
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
        cache.refreshing.lock().await;
        assert_eq!(CALLS.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn refresh_result_discarded_when_config_changes_mid_refresh() {
        static GATE: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(true);
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
        *cfg.write().await = config_with_flag("ds/c", true); // mid-refresh change
        GATE.store(true, std::sync::atomic::Ordering::SeqCst);
        cache.refreshing.lock().await; // refresh #1 completes -> discarded
        // Entry still holds ds/a; this get returns stale and triggers
        // refresh #2 (gate now open, completes fast).
        let (etag_stale, _) = cache.get(&cfg).await;
        assert_eq!(etag_stale, etag1, "discarded refresh must leave the old entry");
        cache.refreshing.lock().await;
        let (_, body3) = cache.get(&cfg).await;
        assert!(
            body3.windows(4).any(|w| w == b"ds/c"),
            "second refresh must converge to ds/c"
        );
    }

    #[tokio::test]
    async fn cold_concurrent_second_waits_for_inflight_refresh() {
        static GATE: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
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
```

Note: `refreshing` and `marker_discovery`/`config_with_flag` already exist from Task 1's file state; if `marker_discovery` was removed in Task 1's conversion, keep it — it is still used by these tests.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test models_cache::`
Expected: FAIL — `no field refreshing on type CatalogCache` (compile error).

- [ ] **Step 3: Implement the SWR state machine**

In `src/models_cache.rs`:

1. Struct gains the single-flight lock; `get` takes `self: &Arc<Self>`:

```rust
pub struct CatalogCache {
    inner: Mutex<Option<Cached>>,
    /// Single-flight gate for background refreshes: try_lock decides
    /// "spawn the refresh" vs "one is already running" (spec §Design).
    refreshing: tokio::sync::Mutex<()>,
    /// Bundled-catalog discovery, injectable so unit tests never depend on
    /// the host's real codex binary (hide-bundled spec §Freshness).
    discovery: fn() -> Vec<serde_json::Value>,
}
```

Update `new()` to add `refreshing: Default::default()`.

2. Replace Task 1's `get` + `rebuild_inline` with:

```rust
    /// Return `(etag, body)`, never blocking on a subprocess on the request
    /// path and never holding a config guard across one (spec §Goal).
    /// - fast path: fresh cache hit;
    /// - stale path: return the old entry, spawn a single-flight background
    ///   refresh;
    /// - cold start (no entry): refresh inline, or WAIT for an in-flight
    ///   refresh (a daemon that just started must answer correctly).
    pub async fn get(self: &Arc<Self>, config: &crate::proxy::SharedConfig) -> (String, Bytes) {
        let fingerprint = fingerprint_config(&config.read().await);
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
            if let Some(hit) = self.fast_hit(fingerprint_config(&config.read().await)) {
                return hit;
            }
            self.refresh(config).await;
            self.stale_entry()
        }
    }

    /// One refresh run: read the CURRENT config, join the two subprocess
    /// calls on blocking threads, build, and store only if the config has
    /// not changed since the read (fingerprint guard, spec §Design step 4).
    /// Subprocess failures degrade inside the rebuild exactly as the inline
    /// path always did and the result IS stored (spec §Design step 5).
    async fn refresh(&self, config: &crate::proxy::SharedConfig) {
        let (fingerprint, snapshot) = {
            let guard = config.read().await;
            (fingerprint_config(&guard), guard.clone())
        };
        let discovery = self.discovery;
        let (template, bundled) = tokio::join!(
            tokio::task::spawn_blocking(crate::catalog::reload_template),
            tokio::task::spawn_blocking(move || discovery()),
        );
        // JoinError only on a panicking closure; degrade like any failure.
        let (template_path, template) = template.unwrap_or((None, None));
        let bundled = bundled.unwrap_or_default();
        let template_mtime = template_path.as_ref().and_then(file_mtime);
        let mut generated = crate::catalog::build_catalog_value(&snapshot, template.as_ref());
        let hide_bundled = snapshot.server.hide_bundled_models;
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
        // Fingerprint guard: the config changed mid-refresh; discard so the
        // next stale request refreshes against the newer config.
        if fingerprint_config(&config.read().await) != fingerprint {
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
```

3. `src/proxy/mod.rs`: `AppState.models` becomes `pub models: Arc<crate::models_cache::CatalogCache>` (line ~214) and the constructor at ~359 becomes `models: Arc::new(crate::models_cache::CatalogCache::new())`. The Task 1 call site `state.models.get(&state.config).await` keeps working (`&Arc<CatalogCache>` method receiver).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test models_cache::` — Expected: PASS (7 converted + 4 new).
Run: `cargo test --test endpoints_metrics` — Expected: PASS (hot-reload poll loops absorb the SWR delay).

- [ ] **Step 5: Commit**

```bash
git add src/models_cache.rs src/proxy/mod.rs
git commit -m "feat(models-cache): stale-while-revalidate with single-flight background refresh"
```

---

### Task 3: Thread naming + docs + full regression

**Files:**
- Modify: `src/catalog.rs:500-508` (reader thread name)
- Modify: `AGENTS.md` §12 (one sentence)
- Modify: `ARCHITECTURE.md` §11 (line counts)

**Interfaces:**
- Consumes: Tasks 1–2 via the full test suite.
- Produces: documentation only.

- [ ] **Step 1: Name the reader thread**

In `src/catalog.rs`, replace:

```rust
    let stdout_pipe = child.stdout.take();
    let reader = std::thread::spawn(move || {
```

with:

```rust
    let stdout_pipe = child.stdout.take();
    // Named for /proc/<pid>/task visibility (SWR spec §Thread naming);
    // spawn failure degrades like every other discovery failure.
    let reader = match std::thread::Builder::new()
        .name("bundled-reader".to_string())
        .spawn(move || {
```

and the closure body stays identical through `buf`), closing with:

```rust
        }) {
        Ok(handle) => handle,
        Err(_) => return Vec::new(),
    };
```

(i.e. the full statement becomes `let reader = match Builder...spawn(move || { ...existing body... }) { Ok(h) => h, Err(_) => return Vec::new() };`)

- [ ] **Step 2: AGENTS.md §12 — append one sentence to the hide_bundled bullet:**

```markdown
Catalog serving is stale-while-revalidate: an expired entry is served
immediately while a single-flight background task refreshes it, so a
config change becomes visible on the request AFTER the refresh completes
(see docs/superpowers/specs/2026-08-28-models-swr-design.md).
```

- [ ] **Step 3: ARCHITECTURE.md §11 — refresh line counts**

Run: `wc -l src/models_cache.rs src/proxy/mod.rs src/catalog.rs` and update those rows.

- [ ] **Step 4: Full verification**

Run: `cargo fmt` then `cargo test`
Expected: whole suite PASS (including `models_cache::`, `endpoints_metrics`, and the SSE suites).

- [ ] **Step 5: Commit**

```bash
git add src/catalog.rs AGENTS.md ARCHITECTURE.md
git commit -m "chore(models-cache): name the bundled-reader thread + SWR docs sync"
```
