# Code Review — Global Pass: Concurrency / Performance / Edge Cases

**Date:** 2026-09-01
**Branch:** `feat/fragmented-items-merger` @ `93ecdcd` (post merge_fragmented landing)
**Scope:** whole repo (~19k LOC src/), with line-level reading of the core
request paths (session store, both proxy handlers, SSE parser, hot-reload
watcher) and a parallel sweep of the periphery (catalog, models_cache,
metrics, convert, normalize, config validation).
**Status:** §1 (Thread / Concurrency, C1–C4) fixed 2026-09-01; §2 and §3
open.

Overall: the architecture is clean, degradation paths are well covered, and
the docs/code sync is unusually good. The hot-reload channel+applier design,
the fast-path gate, the `InFlightGuard` Drop pairing, and the SSE parser's
byte-level discipline are all solid. The items below are the ones worth
acting on.

---

## 1. Thread / Concurrency — FIXED 2026-09-01

### C1 (medium) — catalog template discovery shells out with no timeout; can wedge all `/models` cold-starts

`catalog.rs:391` tier-2 runs `Command::new("codex").args(["debug","models","--bundled"]).output()`
with no deadline. This runs inside `CatalogCache::refresh`'s single-flight
`refreshing` Mutex (`models_cache.rs:196`), which every cold-start `/models`
request queues on (`models_cache.rs:130`). If the `codex` on PATH hangs (a
wrapper script prompting on stdin, a frozen network call), every `/models`
request blocks forever and a blocking-pool thread leaks. The sibling
`discover_bundled_models` path has a 10s `BUNDLED_DISCOVERY_TIMEOUT` whose own
comment says "a hung `codex` must not wedge the `/models` request path" —
tier-2 skips that protection.

**Fix (applied):** extracted `bundled_command_stdout(cmd, timeout)` — the
single bounded runner (reader-thread drain, poll, kill-on-deadline) — and
routed BOTH shell-out call sites through it. `bundled_from_command_with_timeout`
parses the `models` array from its output; `load_template` tier 2 parses the
raw stdout as the whole template. Bonus hardening: the reader-spawn-failure
path now reaps the child instead of leaking it.

### C2 (low) — `handle_responses` holds the config read guard across `sessions.get().await` — FIXED

`proxy/mod.rs:683-718`: route lookup and session-history merge happen inside
one config read-lock guard, and `get()` internally takes the session store's
**write** lock and deep-clones the whole history. The two locks are separate,
so there is no deadlock; but tokio's RwLock is write-preferring — a queued
hot-reload write blocks new readers, and during that window the enlarged
history clone stretches the hold. Latency spikes during reload + big history.

**Fix (applied):** the route lookup happens under a short guard that is
dropped before `sessions.get`, which now runs outside the config lock.

### C3 (low) — blocking I/O on the async runtime — FIXED

Two spots:

- `upstream.rs:530` — `resolve_api_key` does a synchronous
  `fs::read_to_string` per request for `api_key_file` providers.
- `models_cache.rs:151` — `fast_hit` calls `fs::metadata` while holding the
  synchronous `inner` Mutex on a tokio worker thread. Local stats are
  microseconds, but a stat on a hung autofs/NFS mount can block for seconds,
  stalling every `/models` request (including fresh hits) on that Mutex.

**Fix (applied):** ① new `resolve_api_key_async` parks the `api_key_file`
read on `spawn_blocking` (inline/env stay synchronous); the request handler
calls it. ② `fast_hit` snapshots the entry under `inner` and stats the
template OUTSIDE the lock — a refresh landing between snapshot and return is
the documented one-request-stale SWR tolerance. ③ the hot-reload applier's
catalog-cache unlink moved to `spawn_blocking` (`invalidate_codex_catalog_cache`
is now async).

### C4 (low) — two process-lifetime unbounded sets keyed by client-controlled strings — FIXED

- `CodexVersionTracker.seen: HashSet<String>` (`version.rs:47`): one
  permanent entry per distinct `client_version`. Length + charset are gated
  (`proxy/mod.rs:520`), but there is **no cap on the number of distinct
  values** — each also adds a permanent Prometheus series via
  `record_codex_client`.
- `normalize.rs:394` — `UNKNOWN_TYPE_COUNTS: LazyLock<Mutex<HashMap<String, u64>>>`
  inserts every distinct unknown item `type` / tool `type` / conflicting tool
  name and never evicts. No length gate at all.

**Fix (applied):** `CodexVersionTracker::observe` caps `seen` at
`MAX_TRACKED_VERSIONS` (64) — past the cap new labels stay silent (no new
log/metric series). `normalize::bump_and_warn` caps `UNKNOWN_TYPE_COUNTS` at
`MAX_UNKNOWN_TYPE_KEYS` (256) — past the cap unknown keys are still warned
about per occurrence but not persisted (`(untracked: counter map at cap)`).

Follow-up on the fix: the models_cache single-flight test's first assertion
depended on the winner's blocking discovery closure having *started* by the
time the concurrent gets joined — an implicit blocking-pool timing dependency
that the shorter `fast_hit` lock hold exposed (it then hung the solo run, because
a panic with the gate closed leaves the gated discovery spinning forever).
The first assert is now the intent-preserving `CALLS <= 2` (gate still closed:
a second refresh cannot have started); the deterministic post-barrier `== 2`
assert is unchanged.

## 2. Performance

### P1 (high) — session store re-serializes the ENTIRE store on every save

`session.rs:211` → `enforce_limits` → `estimated_bytes()` (`session.rs:221`)
runs `to_string()` over every item of every session on every `save` — i.e.
once per turn. At the default 256 sessions with long conversations that is
potentially hundreds of MB of serde serialization per request, and the
byte-budget `while` loop re-runs the full scan after each eviction. This is
the single largest performance item in the repo.

**Fix:** maintain a running byte counter on `SessionState`, updated
incrementally on insert/remove; compute only the new entry's size at save
time.

### P2 (low-medium) — SSE boundary scan restarts from buffer index 0 on every chunk

`upstream.rs:238-250` `find_event_boundary` window-scans the whole buffer
(twice: LF and CRLF) each time a chunk arrives. A single huge event with no
boundary makes this O(n²). `trim_completed_prefix` in `proxy/capture.rs`
already implements the correct incremental discipline (resume from
`prev_len - marker_len + 1`); the parser should do the same. Low daily impact
since events are small, but the fix pattern already exists in-repo.

### P3 (low) — per-chunk heap allocations on the "zero-overhead" verbatim fast path

`proxy/mod.rs:160-179` `first_content_event_bytes` allocates a haystack Vec
plus a new carry Vec per chunk, then window-scans for 3 markers. This is the
only per-chunk allocation on the verbatim relay path.

**Fix:** scan in place over carry (capped at 37 bytes) + chunk, or use memchr.

### P4 (low) — chat path triple-transforms the request body

`proxy/chat.rs:62-77`: `serde_json::to_value(&chat_req)` (full Value tree) →
field surgery → `serde_json::to_vec(&chat_body)` (re-serialize). With codex
replaying full transcripts inline, that is two MB-scale temporary
materializations per request.

**Fix:** let `send_upstream` accept a `Value` and serialize once.

## 3. Edge Cases

### E1 (medium-high) — axum's default 2 MB body limit is not raised; long codex sessions will 413

No `DefaultBodyLimit` override anywhere. axum 0.8 defaults to a 2 MB request
body cap, and codex sends `store: false` while **replaying the full transcript
inline every turn** (AGENTS.md §6). A long session with large tool outputs
crosses 2 MB → the daemon answers 413 and that session is unusable here, with
no degradation path. Local proxy, so the risk calculus is simple.

**Fix:** one line — `DefaultBodyLimit::max(N)` in `build_router`.

### E2 (medium) — `/models` cache fingerprint omits `route.description`; description-only edits serve stale catalogs forever

`models_cache.rs:253-268` hashes only `hide_bundled_models`, route key,
`context_window`, and `default_reasoning_effort` — but the served body also
contains `description` (written by `build_catalog_value`). A hot-reload that
changes only a description produces an identical fingerprint, so `fast_hit`
keeps serving the old body. The escape hatches don't help on a common
configuration: `recheck_due` requires `template_path.is_none() ||
hide_bundled`, so with a file-backed template and `hide_bundled_models =
false` (defaults), the stale description is served **indefinitely** —
violating the documented "config change becomes visible after refresh"
contract (AGENTS.md §12).

**Fix:** fold `route.description` into `fingerprint_config`.

### E3 (medium) — a function_call with an empty `name` is emitted and persisted, poisoning session replay

`convert/response.rs:724-728`: the liveness filter keeps accumulator entries
with `!name.is_empty() || !args.trim().is_empty()`. AGENTS.md §8 documents
dropping entries with *neither* name nor args, but the sibling case —
arguments streamed with no `function.name` fragment ever arriving — passes
the filter, ships `name: ""`, and is stored. On the next turn the session
replays it as `tool_calls[].function.name: ""`, which strict Chat upstreams
reject — the same failure family as the documented empty-`call_id` case
(§8b).

**Fix:** apply the same drop rule to empty-name entries at finish (or at
minimum drop them from `acc.items` so they never replay).

### E4 (low-medium) — `total_tokens` computed as plain `u32 + u32`; overflow on upstream-controlled values

`convert/response.rs:247` and `:830`. Two individually-valid u32 token counts
(e.g. 3_000_000_000 + 2_000_000_000) panic the handler task in debug builds
and silently wrap to a negative-looking total in release, written into
`response.completed` which codex consumes.

**Fix:** `u64::from(a) + u64::from(b)`.

### E5 (low) — `ttl_hours = 0` silently disables the session store

`config.rs:206` has no validation. The sliding TTL becomes zero, so the
cutoff is `now` and every session expires in the gap between `save` and the
next turn's `get` — the store is permanently miss with no warning. The docs
only describe the zero semantics of `max_sessions` / `max_memory_mb`.

**Fix:** WARN at validate time, or document the zero semantics.

### E6 (low) — non-streaming upstream body reads have no size cap

`upstream_resp.bytes().await` (`proxy/chat.rs:410`, passthrough equivalent)
is bounded only by the total timeout, not by size — a broken/malicious
upstream can push unbounded bytes within `timeout_ms`. Low risk in the local
trust model; noting for completeness.

---

## Suggested fix order

1. **E1** (DefaultBodyLimit — one line, prevents a real user-facing failure)
   + **P1** (session byte counter — biggest perf win)
2. **C1** (tier-2 timeout) + **E2** (fingerprint + description) — `/models`
   correctness & availability
3. **E3** (empty-name function_call) + **E4** (u64 sum) — session-poisoning
   family
4. Batch: C2 / C3 / C4, P2 / P3 / P4, E5 / E6

## Checked and found clean

- **Hot-reload design** (channel + async applier, directory watch with
  filename filter, ENOENT retry): matches AGENTS.md §7, no lost-update or
  deadlock paths found.
- **metrics.rs**: label cardinality bounded by validated config;
  in-flight gauge paired via Drop guard.
- **Streaming teardown**: client disconnect → `tx.send` failure → loop
  break → upstream dropped, on both handlers; idle timeout, `missing_done`
  non-rescue on timeout, and terminal `response.failed` injection all
  verified consistent.
- **SSE parser**: UTF-8 split-chunk reassembly, CRLF, multi-line data,
  trailing flush — fixture coverage is thorough.
- **convert/request.rs**: no reachable panics; indexing guarded.
- **catalog.rs discovery runner** (the bounded one): reader-thread drain and
  kill-on-timeout are correct.
