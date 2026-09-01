# Design: /models cache — close the config-change staleness window

**Date:** 2026-09-01
**Status:** approved for implementation (brainstorm Q1–Q4 recapped below)
**Base:** main @ `1e3c481`
**Branch:** `fix/models-cache-reload-staleness`
**Related:**
- `docs/superpowers/specs/2026-08-28-models-swr-design.md` (the SWR design this amends)
- Incident evidence: daemon log 2026-09-01T05:54:07 + `~/.codex/models_cache.json` (`fetched_at = 05:54:11`, stale slugs present)

## Problem

Route removal + hot reload, then restarting codex, still listed the removed
model in the picker for ~5 minutes; selecting it 400s (`no route for model`).

Root cause chain (all steps verified from the incident):

1. Daemon reloads, deletes codex's `models_cache.json` — working as designed.
2. The still-running codex's background worker completes a `/v1/models` fetch
   that landed **inside the SWR window**: `CatalogCache::get` served the
   pre-change body (one-request-stale tolerance) while the single-flight
   background refresh ran.
3. Codex **persisted** that stale response back to `models_cache.json`
   (`fetched_at = 05:54:11`, 3.7s after the deletion), recreating the file.
4. codex's own cache TTL is 300s — every codex process started within that
   window (including fresh ones) reads the stale catalog without fetching.

So the daemon's millisecond-scale one-request-stale tolerance is amplified to
codex's 5-minute TTL whenever a client fetch races the reload and persists the
answer. The SWR spec's contract ("visible on the request AFTER the refresh
completes") holds per-request but is not enough for persisting clients.

## Goal

**Any `/v1/models` request that arrives after the config write completes must
never receive the pre-change body.** Time-based staleness (60s recheck,
template mtime) keeps the SWR behavior — that staleness is not tied to a
config change the client could have observed, and serving it stale is the
latency trade the SWR spec chose deliberately.

## Design

Semantic boundary: **fingerprint mismatch ⇒ wait; fingerprint match ⇒ SWR.**

### B — `get()` waits on fingerprint mismatch (models_cache.rs)

New public method `refresh_if_stale(&Arc<Self>, config)`: compute the current
fingerprint; if `fast_hit` matches, return; else take the `refreshing`
single-flight lock (try, then wait), re-check `fast_hit` (a concurrent waiter
may have refreshed), then run the existing forced-refresh loop (mirror of the
cold path: `refresh(force)` → `fast_hit` → `force = true`; two iterations
bound it).

`get()` changes: on `fast_hit` miss with an EXISTING entry whose stored
fingerprint ≠ current fingerprint → `refresh_if_stale` (wait), then return
`fast_hit`, falling back to `stale_hit` only as the last resort (config
changed AGAIN mid-refresh — next request re-detects). The SWR branch
(background refresh + stale body) now applies **only** when the stored
fingerprint matches (time-based recheck/mtime staleness). Cold start
unchanged.

Latency: first request after an edit waits out one refresh (typically <1s;
worst ~10s with `hide_bundled_models = true` discovery). Concurrent waiters
serialize on `refreshing` and each re-checks `fast_hit` after acquiring the
lock, so a burst costs one refresh, not N. No additional explicit timeout:
the wait is bounded by the discovery kill-on-deadline (10s, the C1 fix) and
the two-iteration force loop.

### A — applier kicks a proactive refresh (config.rs + proxy)

`spawn_watcher` gains an optional `ReloadHook` (`Arc<dyn Fn() + Send + Sync>`)
invoked by the applier after the config write + codex-cache invalidation.
`serve()` installs a hook that spawns `models.refresh_if_stale(&shared)` —
detached, so the applier loop never blocks on discovery. With A, the racing
fetch from step 2 of the incident arrives to an already-refreshed entry and
gets the NEW body even without B's wait; B covers any residue (in-flight
fetches that started before the write, hook not yet polled).

`spawn_watcher` callers without a cache (config tests) pass `None`.

## Decisions

Recap brainstorming Q1–Q4 (2026-09-01, all resolved to the recommended
options):

| | Decision | Rejected |
|---|---|---|
| **Q1** | Fingerprint line, NO extra timeout: mismatch ⇒ wait (bounded by the 10s discovery deadline + two-iteration force loop), fingerprint-match ⇒ SWR | Explicit 15s wait timeout (a magic number + an untestable degradation path); drop SWR entirely (60s recheck/mtime would block requests that have nothing to do with the user's edit) |
| **Q2** | ReloadHook callback (`Arc<dyn Fn()>`), applier fires it after write + invalidation; serve() installs a detached `refresh_if_stale` spawn | No proactive refresh (first racing fetch would wait out a cold refresh); pass `Arc<CatalogCache>` into config.rs (couples the config module to the catalog cache) |
| **Q3** | Residual mismatch (config changed AGAIN mid-refresh) ⇒ serve the stored body, next request retries | 502 (a reload storm would take the picker down; codex's 5xx retry behavior is uncontrollable); empty model list (a different lie) |
| **Q4** | NO delayed second deletion of codex's `models_cache.json` | Timer-based re-deletes (5s/30s): A+B already ensures any racing fetch receives the NEW body, so what codex writes back is clean — the second deletion has nothing left to catch and only adds timer complexity + log noise |

## Scenario matrix

Every config-edit shape, its fingerprint visibility, and the required
outcome. Fingerprint inputs: hide flag + per-route `{key, context_window,
effort, description}` — the upstream `model=` name and provider connection
fields are NOT catalog-visible (verified: `set_catalog_fields` writes
slug/display_name from the route key; `route.model` never enters the body),
so edits that change only those must NOT invalidate.

| # | Scenario | Fingerprint | Picker/catalog outcome | Covered by |
|---|---|---|---|---|
| S1 | Add model (new route) | changes | new slug appears; A makes it near-immediate, B guarantees any post-write request sees it | `models_endpoint_reflects_hot_reload` (e2e) + unit Δ |
| S2a | Change route's upstream `model=` only | **unchanged** | catalog body identical — the upstream mapping is invisible to codex; refresh is a fingerprint-matched no-op | new unit pin (`fingerprint_visibility_matrix`) |
| S2b | Change route's context_window / effort / description / hide flag | changes | entry rebuilt (E2 guarantees description edits invalidate) | unit Δ + E2 regression |
| S3 | Delete model (route) — the incident | changes | slug disappears for any post-write request; codex's write-back is clean | new e2e `models_endpoint_reflects_route_removal` |
| S4 | Add provider WITHOUT routes | unchanged | providers never appear in the body; reload is a no-op refresh | unit pin (same matrix) |
| S5 | Add provider WITH routes | changes | same as S1 (fingerprint sees the new route keys) | unit Δ |

Cross-cutting: in every "changes" row, ANY request arriving after the config
write gets the new body (wait path), and codex's persisted write-back is
therefore clean — the incident cannot recur for any of the shapes above.

## Testing

Reworked (old semantics were pinned by tests — their intent moves with the
boundary):

- `stale_entry_returns_immediately_and_refreshes_in_background` → becomes
  `config_change_waits_for_fresh_body`: fingerprint change + gate OPEN →
  `get` returns the NEW body (waited for). Immediacy moves to the
  time-staleness case.
- `single_flight_merges_concurrent_stale_gets` → gate removed (under wait
  semantics a closed gate deadlocks the join); counting discovery only: 8
  concurrent gets on a changed config → exactly one refresh, all bodies NEW.
- `refresh_result_discarded_when_config_changes_mid_refresh` → the "returns
  stale after discard" assertion becomes "waits and converges to ds/c".
- `aged entry with hide on must rebuild` stays valid (same fingerprint ⇒
  SWR branch).

New pins:

- `refresh_if_stale_syncs_without_a_request`: entry built for cfg A;
  `refresh_if_stale(cfg B)` → `fast_hit(fp_B)` matches (the A-behavior
  contract: the applier hook alone makes the next request fresh).
- `fingerprint_visibility_matrix`: add-route / description / effort /
  context_window / hide each change the fingerprint; model=-only change and
  a routeless provider leave it unchanged (S1–S5 unit coverage).
- e2e: `models_endpoint_reflects_route_removal` (S3, the incident shape —
  rewrite config without a route, first post-reload request must already
  lack the slug); `models_endpoint_reflects_hot_reload` (S1) stays as the
  add-path e2e and now converges through the wait path.

## Docs sync

- AGENTS.md #12 + README-DETAILS (Two config modes / Clients sections):
  "the first read after an edit may return the pre-change body" → "reads
  after a config change wait for the refreshed catalog (typically <1s);
  only time-based rechecks (60s) serve stale-while-revalidate".
- `2026-08-28-models-swr-design.md`: add an amending note pointing here.
- CHANGELOG: Fixed entry (stale catalog after route removal could persist
  ~5 min via codex's cache).

## Out of scope

- codex-side behavior (its 300s TTL and persist-on-fetch are client-internal).
- `upstream_non_2xx`-style guarantees elsewhere; this is catalog-only.

## Risks

| Risk | Mitigation |
|---|---|
| First request after an edit blocks on discovery (~10s, hide on) | Single-flight + `fast_hit` re-check per waiter; proactive kick (A) means the wait is usually already satisfied |
| Reload storm serializes waiters | Same single-flight as the cold path; each waiter re-checks before refreshing |
| Waiters pile up behind a hung discovery | Discovery is kill-on-deadline (10s, C1 fix) — the wait is bounded by the same bound |

## Verification

1. `cargo test` green (reworked + new fixtures).
2. `RUST_LOG=debug` + mock upstream: edit config (remove a route) while a
   fetch loop runs → every response after the edit reflects the new route set.
3. Manual: reproduce the incident shape — delete a route, restart codex
   within 60s → removed model absent from the picker.
