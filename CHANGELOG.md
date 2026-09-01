# Changelog

All notable changes to codexferry are documented here, newest first.
Format follows [Keep a Changelog](https://keepachangelog.com/); versions
follow semver. Releases prior to v0.1.3 are documented on the
[GitHub Releases](https://github.com/nearbyfly/codexferry/releases) page.

Each release's section is drafted with `scripts/release.sh vX.Y.Z
--prep-changelog`, curated by hand, and committed to `main` **before** the
release is cut, so both remotes receive it through the normal push flow.

## [v0.1.4] — 2026-09-01

**`merge_fragmented` heal pass for upstream Responses SSE fragmentation.**

### Features

- `[quirks] merge_fragmented` (default ON): when an upstream Responses
  gateway emits one logical output item as N consecutive
  `output_item.added` events (observed with MiniMax M3 in
  `NOTES-2026-08-28`, typical 5–14 fragments), the daemon merges them
  into a single Responses-conformant item. All deltas in the run are
  rewritten to the first fragment's `item_id` / `output_index`, and a
  synthesized `content_part.done` + `output_item.done` is emitted at
  the run boundary. Applies to consecutive same-type `message` /
  `reasoning` items, and to consecutive `function_call` items with
  matching `call_id` (OpenAI Responses contract: same call must live
  in the same item). Runs of length 1 (every healthy stream) pass
  through byte-identical — suppression/rewriting activate only once a
  second same-type fragment actually arrives, and the rewritten
  `response.completed.output` collapses the run's fragments in place
  while keeping all unmerged sibling items. Items of uncovered types
  (e.g. `web_search_call`) break the run and keep their own events.
  Off via `[quirks] disabled = ["merge_fragmented"]`.
  Hot-reloadable. Responses-format path only — chat-format path is
  naturally unfragmented.

### Changed

- **Model-picker descriptions name the upstream wire format**: catalog
  entries now describe a route as `CodexFerry chat to <key>` or
  `CodexFerry responses to <key>` (plus the optional configured
  description), so the picker shows which interface — converting or
  passthrough — a model sits on. Regenerate the pinned catalog or let the
  live endpoint refresh to see the new wording.

### Fixed

- **Removed routes stayed listed in codex's picker for ~5 minutes**: a
  `/v1/models` fetch racing a hot reload received the pre-change catalog and
  codex persisted it into its own 300s cache. Reads arriving after a config
  change now wait for the refreshed catalog (and the hot-reload applier
  refreshes it proactively), so route add/remove/change is reflected in every
  post-edit response; the stale-while-revalidate trade remains only for the
  60s time-based recheck.

- **Long codex sessions rejected with 413**: the daemon now accepts request
  bodies up to 64 MiB (axum's 2 MiB default rejected long `store: false`
  transcript replays). The same cap bounds non-streaming upstream response
  reads, which previously had no size limit.
- **`/models` catalog stale after description-only config edit**: the cache
  fingerprint now includes each route's `description`; previously such an
  edit never invalidated the served catalog (indefinitely, with a
  file-backed template and hiding off).
- **Empty-name tool calls no longer poison session replay**: a streamed
  tool call whose function name never arrived is dropped at stream end
  instead of being emitted and persisted with `name: ""` (which strict Chat
  upstreams reject on the next turn).
- **`total_tokens` overflow**: computed as u64; two individually valid u32
  upstream counts no longer overflow (debug panic / release wrap).
- **`ttl_hours = 0` warns at startup**: it silently disabled the session
  store (every session expires immediately).

### Performance

- **Session store**: the memory budget no longer re-serializes every cached
  session on every save — entries cache their size estimate and the store
  keeps a running byte total (O(1) budget check per turn; previously O(total
  stored bytes) of JSON serialization per request).
- **SSE parsers**: event-boundary scans are incremental — after a chunk
  arrives, only the small overlap window before the previously-scanned end
  is re-examined, instead of rescanning the whole buffer (O(n²) for one
  huge event).
- **Passthrough fast path**: first-content (TTFT) marker detection no longer
  allocates per relayed chunk.
- **Chat path**: when a provider configures neither `drop_params` nor
  `extra_params`, the outbound request body is serialized directly from the
  typed request — no intermediate `serde_json::Value` tree of the
  (potentially transcript-sized) body.

### Docs & tests

- **Coverage measurement**: new `scripts/coverage.sh unit|integration|e2e`
  wraps cargo-llvm-cov per test layer (HTML reports under `coverage/<mode>/`;
  see README-DETAILS "Test coverage"). Built on the integration harness now
  stopping the router subprocess the way production does — SIGTERM into the
  graceful-shutdown path, SIGKILL only as a bounded fallback — instead of a
  bare SIGKILL, which lost the subprocess's LLVM counters and would report
  the handlers at ~0%.
- **Models TDD audit**: closed the coverage gaps found in the PR #7–#12
  review sweep of the models cache and catalog.
- **README slimmed to quick-start shape**; the deep config/endpoint
  reference moved to README-DETAILS.md.

### Spec

`docs/superpowers/specs/2026-08-30-fragmented-items-merger-design.md`.

## [v0.1.3] — 2026-08-28

**Dynamic-mode picker cleanup (hide bundled GPT models), richer error
logging, hot-reload robustness.**

### Features

- `[server] hide_bundled_models = true` (dynamic mode): the live `/models`
  catalog additionally returns `visibility: "hide"` copies of every
  picker-visible model in Codex's bundled catalog (discovered via
  `codex debug models --bundled`), suppressing the bundled GPT entries
  from the Codex model picker while configured routes stay selectable.
  The flag is part of the catalog cache fingerprint (toggling it via
  config hot-reload rebuilds the catalog) and the bundled catalog is
  re-probed on the 60-second cadence. Degrades silently — with a warn in
  the daemon log — when no `codex` binary is on the proxy's `PATH`.
  `gen-catalog` output is never affected.
- Route-level `description` config: catalog entries render as
  `CodexFerry route <key> - <description>`, falling back to
  `CodexFerry route <key>`.
- The per-request log line now includes the upstream error message for
  4xx/5xx responses.

### Fixes

- Config hot-reload can no longer drop an update that arrives while a
  request holds the read lock: the watcher callback now hands the parsed
  config to an async applier task via an unbounded channel.
- Bundled-catalog discovery drains the `codex debug models --bundled`
  subprocess stdout concurrently — a large bundled catalog can no longer
  fill the pipe buffer and deadlock discovery.
- Oversized upstream error bodies are no longer truncated before error
  detail extraction, so the 4xx/5xx log line keeps the real message.

### Docs & tests

- Full test ladder for `hide_bundled_models`: unit tests, endpoints
  integration (fake `codex` on `PATH` + hot-reload toggle), and an e2e
  scenario driving the real Codex CLI through the dynamic-mode merge
  (`scripts/e2e.sh hide_bundled`).
- README / ARCHITECTURE / AGENTS doc sync; design spec + implementation
  plan in the internal repo's `docs/superpowers/`.

### Internal

- `release.sh` fast-forwards the github mirror's `main` with
  `--force-with-lease` (an argument-order bug in the lease was found
  during this very release and fixed immediately after tagging; see
  main).
- Internal `main` now carries `version = "0.0.0-main"` between releases
  so dev builds self-identify; `release.sh --bump-cargo` overwrites the
  whole version line when cutting a release.
