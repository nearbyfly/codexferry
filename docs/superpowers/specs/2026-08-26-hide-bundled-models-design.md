# Hide Bundled Models (Dynamic Mode) — Design

**Date:** 2026-08-26
**Status:** approved for implementation
**Branch:** `cxf-improvements-1`

## Problem

In dynamic catalog mode (no `model_catalog_json` pin, `auth.command` present),
Codex CLI merges its **bundled** model catalog (compiled into the codex binary:
`gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna`, `gpt-5.5`, `gpt-5.2`, …)
underneath the models codexferry serves from `/v1/models`. The bundled GPT
entries therefore appear in the Codex model picker even though selecting them
fails with "no route for model". Pinned mode avoids the merge but is not an
option here: dynamic mode is required.

## Mechanism (verified against codex 0.147 source)

Codex's `OpenAiModelsManager::apply_remote_models`
(`codex-rs/models-manager/src/manager.rs:446`) treats the `/v1/models` response
as an *overlay* for non-ChatGPT auth: the bundled catalog is the base, and each
fetched entry **replaces** the bundled entry with the same slug
(`manager.rs:465`). Picker visibility is `visibility == "list"` only
(`codex-rs/protocol/src/openai_models.rs:809`,
`show_in_picker: info.visibility == ModelVisibility::List`).

Therefore: if codexferry's catalog response includes, for every bundled
`visibility: "list"` slug — skipping any slug that collides with a configured
route key, since the route must stay selectable — a **clone of the bundled
entry with `visibility` flipped to `"hide"`**, appended after the route
entries in slug order, the merged catalog hides the bundled GPT entries while
codexferry routes stay listed. Cloning (rather than synthesizing from
scratch) round-trips codex's own serialization, so every field required by
codex's strict `ModelInfo` deserialization stays present and all metadata
survives.

## Decisions

1. **Gate — opt-in config flag.** `[server] hide_bundled_models = true`
   (default `false`). Matches the repo's deny-by-default instincts; changing
   what clients see should be a conscious choice.
2. **Scope — live `/models` only.** `gen-catalog` output never contains hide
   entries: pinned mode replaces the whole catalog (`StaticModelsManager` uses
   exactly the file's contents), so hide entries there would be dead weight.
   The chat-list shape (no `client_version` query param) is also untouched —
   it reads `config.routes` directly and never sees hide entries.
3. **Discovery — shell-out only.** The bundled catalog is discovered by
   running `codex debug models --bundled` (the same command `load_template`
   already uses as its tier-2 source, `src/catalog.rs:390`). The installed
   codex binary is the source of truth for its own bundled slugs, so a codex
   upgrade automatically updates the hide list — no version tracking inside
   codexferry. The file fallback tiers of `load_template`
   (`~/.codex/models.json`, …) are deliberately NOT used: they may be
   user-managed and are not guaranteed to equal the binary's bundled list.
4. **Degradation — silent, doctor later.** When discovery fails (no codex on
   PATH, non-zero exit, unparseable output), hiding is off and the catalog is
   served exactly as today (bundled GPT models reappear); a `warn!` log fires
   per rebuild. A doctor check for "flag on but no codex found" is future
   work, explicitly out of scope here.

## Freshness

`CatalogCache` invalidation gains two rules:

- The route fingerprint additionally hashes the `hide_bundled_models` flag —
  toggling it via config hot-reload must rebuild the body (otherwise the
  fast path would keep serving the stale cached body).
- The bundled catalog comes from a shell-out with no mtime to stat, so when
  hide entries were used at build time the existing 60-second re-probe
  (`TEMPLATE_RECHECK_INTERVAL`) applies **regardless of whether the template
  came from a file**. This covers a codex upgrade between requests.

Determinism: unit tests must not depend on the host's real codex binary.
`CatalogCache` carries a private, test-overridable discovery function instead
of env/PATH mutation (in-process PATH mutation races with parallel tests).

## Contract risk

The overlay-replace semantics of `apply_remote_models` are implementation
behavior of codex 0.147, not a documented public contract — a future codex
could merge differently and hide entries would silently stop working.
Mitigation: the cargo integration test pins the wire behavior, and the e2e
scenario (`scripts/e2e.sh hide_bundled`) drives the real Codex CLI through
`codex debug models`, which prints the merged catalog produced by exactly
this code path (`codex-rs/cli/src/main.rs:2150` → `raw_model_catalog` →
`apply_remote_models`), so an overlay-semantics change in a future codex
fails the scenario. The existing version tripwire (`version.rs`) +
mode-aware doctor surface remains the natural place for a future
regression check.

## Out of scope

- `gen-catalog` changes (none).
- Doctor WARN / live check for the degradation case (future work).
- Sharing one subprocess invocation between `load_template` tier 2 and
  discovery (two `codex debug models --bundled` runs per rebuild at most, at
  the 60s re-probe cadence — accepted).
