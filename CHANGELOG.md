# Changelog

All notable changes to codexferry are documented here, newest first.
Format follows [Keep a Changelog](https://keepachangelog.com/); versions
follow semver. Releases prior to v0.1.3 are documented on the
[GitHub Releases](https://github.com/nearbyfly/codexferry/releases) page.

Each release's section is drafted with `scripts/release.sh vX.Y.Z
--prep-changelog`, curated by hand, and committed to `main` **before** the
release is cut, so both remotes receive it through the normal push flow.

## [v0.1.4] — Unreleased

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
