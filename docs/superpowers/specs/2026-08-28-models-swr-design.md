# /v1/models Stale-While-Revalidate Design (P0 De-block)

**Date:** 2026-08-28
**Status:** approved for implementation
**Base:** main @ `c6210c3`
**Source analysis:** `NOTES-thread-view.md` (local, not in git) §3/§6 — P0 +
P1(single-flight) + P1(join subprocesses) + P3(thread naming)

## Problem

`CatalogCache::get` is a **synchronous blocking** call running directly on a
tokio worker inside `handle_models` (`src/proxy/mod.rs:414`), and it holds
the config **read guard** (its parameter is `&RwLockReadGuard`) across the
entire call:

- Fast path (fingerprint hit, recheck not due): pure memory, µs — fine.
- Slow path (60s recheck due with a shell-out template source, route
  fingerprint change, or `hide_bundled_models` on): runs `reload_template()`
  + `discover_bundled_models()` subprocess calls, blocking the worker for up
  to **10 s** (discovery timeout).
- Consequences: N concurrent slow paths block N workers (runtime starvation
  at `worker_count` concurrency); the config applier's **write lock waits
  for all read guards**, so one stuck discovery delays config hot-reload by
  up to 10 s; there is no single-flight, so the N concurrent slow paths also
  spawn N `codex` subprocesses; the two shell-outs run serially.

## Goal & Semantics

The `/v1/models` request path never blocks on a subprocess and never holds
the config lock across one. Expiry is served **stale-while-revalidate**:

1. **Fast path** (unchanged): fingerprint matches, recheck not due → cached
   `(etag, body)` immediately.
2. **Stale path** (new): entry exists but expired/stale-fingerprint → return
   the cached body immediately, ensure exactly one background refresh is
   running.
3. **Cold start** (new): no cached entry at all → the request awaits the
   refresh inline (a daemon that just started must answer correctly).

**Documented behavioral change:** after a config hot-reload, the FIRST
`/models` request may return the pre-change body; the new routes/flag appear
once the background refresh stores (sub-second to seconds later). Codex
refetches per session and the existing integration tests poll-loop, so this
is compatible; it is the accepted cost of zero request-path blocking.

## Design

### Interface (`src/models_cache.rs`, `src/proxy/mod.rs`)

- `CatalogCache::get` becomes
  `async fn get(&self, config: &SharedConfig) -> (String, Bytes)`.
  `handle_models` calls `state.models.get(&state.config).await` and drops
  its own read guard immediately (the fast path acquires/releases its own
  short guard internally).
- `AppState`/`SharedConfig` (`Arc<RwLock<ValidatedConfig>>`) is passed by
  reference so the refresh task can clone the Arc.

### Fast path

Short `config.read().await`: compute fingerprint, stat template mtime,
evaluate the recheck condition exactly as today; on hit, return the cached
`(etag, body)`. The guard is dropped before any `await` on anything else —
no lock is ever held across a subprocess call again.

### Background refresh (single-flight)

- New field `refreshing: tokio::sync::Mutex<()>`. A stale request does
  `try_lock()`:
  - **Acquired** → spawn the refresh task, moving the owned guard in (it
    releases automatically on task end, success or failure — the
    single-flight window).
  - **Not acquired** → a refresh is already running → return the stale entry.
- Refresh task body (`tokio::spawn`):
  1. Read the **current** config under a short guard, clone the
     `ValidatedConfig` (it is `Clone`, `src/config.rs:298`), record its
     fingerprint `f_start`.
  2. `tokio::join!` two `spawn_blocking` closures: `reload_template()` and
     bundled discovery (the injected `discovery` fn seam stays; wall clock
     becomes `max` of the two instead of the sum).
  3. Build the catalog exactly as today (template seeding, hide entries,
     empty-discovery warn).
  4. **Fingerprint-guarded store:** re-read the current fingerprint; if it
     differs from `f_start`, discard the result (config changed mid-refresh;
     the next request sees the stale-fingerprint condition and refreshes
     again). Otherwise replace the `Cached` entry and reset `checked_at`.
  5. Subprocess failures degrade inside the rebuild exactly as today
     (missing template → from-scratch catalog; empty bundled → hide off +
     warn) and the rebuilt entry IS stored. Only a mid-refresh config
     change discards the result, via the fingerprint guard.

### Cold start

If no cached entry exists, there is no stale body to serve, so the request
must end with a fresh one. It first `try_lock`s the single-flight mutex:

- **Acquired** → run the refresh inline (await it) while holding the guard.
- **Not acquired** → another request is already refreshing (cold or stale) →
  `refreshing.lock().await` to WAIT for that refresh to finish, then serve
  the now-cached entry; if the entry is still absent after the wait (the
  in-flight refresh failed), refresh inline while holding the lock.

This closes the cold-start × single-flight combination: waiting beats
erroring or serving nothing.

### Thread naming (P3, ride-along)

The discovery stdout reader thread (`src/catalog.rs:501`) gets a name:
`std::thread::Builder::new().name("bundled-reader")`.

### Shutdown interaction (documented, no change)

A refresh in flight during graceful shutdown delays process exit by at most
the 10 s discovery timeout (`spawn_blocking` tasks are awaited on runtime
shutdown); systemd's `TimeoutStopSec` (default 90 s) covers this. The stream
relay loops already enforce per-chunk idle timeouts (`src/proxy/chat.rs:212`,
`src/proxy/passthrough.rs:176,234`), so in-flight SSE does not hold shutdown
open indefinitely — this is why the broader P2 shutdown-deadline work is out
of scope.

## Testing

- **Unit** (injected `discovery` fn seam, tokio tests):
  - stale-on-expiry: a gated discovery (atomic flag + sleep loop) lets the
    caller assert `get` returns the OLD body before the gate opens;
  - single-flight: N concurrent `get`s while expired → discovery invoked
    exactly once;
  - cold start: no entry + gated discovery → `get` awaits and returns the
    fresh body;
  - fingerprint-guarded store: config flips mid-refresh (gate holds,
    config changed, release gate) → old-body stays until the NEXT refresh;
  - empty-discovery degradation: rebuild proceeds without hide entries
    (the existing hide tests' warn path covers this inline; no separate
    background test needed).
- **Integration:** existing `endpoints_metrics` tests regress unchanged
  (`/models` shape, ETag/304, hot-reload appearance — the poll loops absorb
  the SWR delay; `hide_bundled` toggle test likewise).
- **Docs:** AGENTS §12 gains one sentence on the SWR semantics;
  ARCHITECTURE §11 line counts refreshed.

## Out of Scope

- **P2 graceful-shutdown deadline** (CancellationToken + request deadline):
  premise weakened by the existing per-chunk idle timeouts; different
  subsystem; separate small spec if ever wanted.
- **P3 `wait_timeout` instead of the 20 ms `try_wait` poll**: after this
  change discovery runs on a blocking thread where the sleeping poll costs
  nothing.
- Refresh-observability metrics (duration counters): nothing consumes them
  today.
