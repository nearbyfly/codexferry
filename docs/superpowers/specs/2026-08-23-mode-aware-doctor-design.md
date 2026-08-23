# Design: Mode-Aware Doctor for codexferry

**Date:** 2026-08-23
**Status:** Approved for brainstorming; plan pending
**Supersedes:** `2026-08-22-doctor-live-catalog-design.md` (archived as
`feat/doctor-live-catalog-archived`). That spec was built on a single
(wrong) wiring assumption (`env_key = "CODEXFERRY_DUMMY"`) and concluded
that Goal 2 (live `/models` verified end-to-end) was unachievable.
This spec starts over with the actually-recommended dual wiring
(dynamic auth.command + static env_key+pin) as the baseline and the
fallback (env_key-only) as a degraded legacy case to detect.

## Background

codexferry now ships two supported `~/.codex/config.toml` wirings plus a
degraded fallback that we tell users to migrate away from. The three
modes differ in how codex learns which routes exist and in what fails
when the codexferry ↔ codex contract breaks.

| Mode | Trigger | codex's catalog source | Failure when contract breaks |
|---|---|---|---|
| **pinned** | `model_catalog_json = ...` in `~/.codex/config.toml` | the pinned file (`StaticModelsManager`, `codex-rs/model-provider/src/provider.rs:390`) | codex hard-errors loading the file at session start; **never** runs |
| **dynamic** | no pin + provider has `[X.auth] command` | `GET /v1/models?client_version=...` on session start (`OpenAiModelsManager`, `models-manager/src/manager.rs:437` `has_command_auth`) | codex emits `"Model metadata for X not found. Defaulting to fallback metadata..."` warning AND falls through with fallback metadata — silent and misleading |
| **fallback** | no pin + only `env_key` | none — codex synthesizes a generic fallback `ModelInfo` | as above, plus the user has no working catalog path at all (the pinned file is the only other option) |

The old spec assumed only the fallback case and built the whole design
on the assumption "codex never fetches /models". That is still true
for fallback — but the recommended setup is dynamic (PR #1 flipped
`~/.codex/config.toml` to `auth.command`), where the fetch DOES happen.
The old spec's "Goal 2 is unachievable" conclusion therefore inverts on
the new baseline: Goal 2 is now achievable, and not closing it is a
real gap.

### What does not change

Two layers from the old spec are wire-shape-agnostic and transfer
unchanged:

- **Layer 4 — tripwire** (`observe_client_version` on `/models`,
  `CodexVersionTracker`, `doctor.json` state file): the input arrives on
  every catalog fetch, which dynamic mode does and static mode does not
  (static mode hits the file instead of the endpoint). For static mode
  the tripwire never fires — that is fine, the only thing a codex
  upgrade can break for a pinned codex is the file's shape, which we
  validate in L2.7 below.
- **Layer 3 — live probe of a real codex turn** (tool round-trip,
  responses/chat dialect): the probe always needs codex to talk to
  codexferry; the wiring mode only changes which provider block we put
  in the `-c` overrides.

### What must change

- **Mode detection** — read `~/.codex/config.toml`, classify into
  pinned / dynamic / fallback, branch the checks.
- **Live probe wiring must mirror the user's actual mode**, not be
  hardcoded to env_key (the old probe chose env_key deliberately to test
  the fallback path, but with dynamic being recommended the probe should
  primarily test the dynamic path; env_key stays as a secondary probe).
- **`shadow_check` becomes mode-aware** — for fallback/pinned the pin
  is required and the existing INFO confirms it; for dynamic a pin is a
  shadow that defeats the live fetch (`provider.rs:390` — pin forces
  StaticModelsManager regardless of auth). The dynamic-mode pin shadow
  is the silent-broken-state the doctor should warn about.

## Goals

1. `codexferry doctor` passes — and meaningfully checks — under all three
   modes, detecting mode-specific failure modes without false positives.
2. Dynamic mode is treated as the recommended baseline; the live probe
   exercises the live `/models` fetch path end-to-end, proving class 3
   catalog-acceptance for the recommended wiring.
3. A codex upgrade that breaks the live-catalog contract is detected by
   the daemon tripwire (dynamic) or by L2.7 file-shape validation
   (pinned) — both with actionable remediation.
4. Doctor failures map to a fix layer (router translation, codexferry
   release, or user-side config edit) and print exact snippets.
5. Offline quick-checks stay sub-second and require neither codex nor a
   live router.

## Non-goals

- Removing the `gen-catalog` subcommand (kept as the static-mode
  generator).
- Auto-scheduling or auto-running doctor from the daemon.
- Populating the version-quirk table (still empty; same hard constraint
  that cache fingerprint must include the quirk set when it ever does).
- Verifying class 3 catalog-acceptance in the fallback mode (codex never
  fetches `/models` there — no observable signal on our side; the
  fallback mode's whole point is the user should migrate away).

## Mode detection

`~/.codex/config.toml` is parsed with the `toml` crate (already a
dependency). For each `[model_providers.X]` we look at:

- `model_catalog_json` set at top level → **pinned** if that table's
  `base_url` is ours.
- `[X.auth].command` set → **dynamic**.
- otherwise (no pin, only `env_key`) → **fallback**.

If multiple providers exist and differ, the check uses the active one
(`model_provider` at top level, default `codexferry`).

## Check layers

### L1 — offline config checks (always, <1s)

These never depend on mode and stay largely as in the old spec:

1. **config loads** — `Config::parse_file` + `validate` succeeds.
2. **router has routes** — `validated.routes` non-empty.
3. **codex wiring points here** — parse `~/.codex/config.toml`, for the
   active provider find `[model_providers.X]` whose `base_url` matches
   `http://{server.host}:{server.port}/v1`. Mismatch → INFO with the
   corrected snippet.
4. **mode classification** — see "Mode detection" above; INFO line
   states the detected mode.
5. **codex version vs last green** — `codex --version` parse + compare
   with `~/.local/state/codexferry/doctor.json`'s `last_green`.
   Absent → INFO "never verified". Mismatch → INFO "not verified".

### L2 — mode-specific checks (always, <1s, branch on L1.4)

**Common to all modes:**

6. **codex version vs codexferry version** — if the installed codex is
   newer than any version codexferry has tested against, INFO
   "codex {v} is newer than the latest version codexferry has been
   verified against; run `codexferry doctor --live` to re-verify". This
   closes the tripwire's "silent upgrade" gap when no `/models` request
   has arrived yet to surface it.

**Pinned mode (L1.4 = pinned):**

7. **pin exists + parses** — `model_catalog_json` points at a readable
   file that deserializes as `ModelsResponse`. FAIL on this is the
   only legitimate FAIL of doctor in any mode: the user can't run codex
   until the pin is regenerated.
8. **pin ⊇ router routes** — every route key in `validated.routes`
   appears in the pin's `models[].slug`. Missing → FAIL listing the
   missing slugs (stale router or stale pin; remediation: rerun
   `codexferry gen-catalog`).
9. **pin ⊆ router routes** — every slug in the pin's `models[]`
   appears in `validated.routes`. Mismatch → FAIL listing the orphans
   (stale pin; remediation: rerun `gen-catalog`).
10. **pin entry shape** — each `models[i]` passes a check against the
    field set codex 0.147's `ModelInfo` requires (`slug`,
    `display_name`, `supported_reasoning_levels`, `shell_type`,
    `visibility`, `supported_in_api`, `priority`,
    `truncation_policy`, `support_verbosity`,
    `experimental_supported_tools` — see `catalog.rs::set_catalog_fields`
    for the full pinned set). Missing field → FAIL with the field name
    + the offending entry. This is the static-mode equivalent of the
    live class-3 check: a future codex that requires a new field will
    surface here as soon as the user runs doctor, even without `--live`.

**Dynamic mode (L1.4 = dynamic):**

7'. **no stale pin shadow** — if `~/.codex/config.toml` has BOTH
    `model_catalog_json` AND a provider with `[X.auth] command`, WARN:
    "pin forces StaticModelsManager; the live fetch enabled by
    `auth.command` will never run. Either remove the pin or remove
    `auth.command` — pick one mode." This is the mode-aware reverse of
    the old `shadow_check`'s env_key advice.
8'. **router `/v1/models` reachable** — quick HEAD/GET smoke against
    the daemon; FAIL if it 500s (the dynamic-mode class-3 endpoint is
    down; remediation: check `models_cache.rs::CatalogCache::get`).
9'. **`/v1/models` shape** — parse the response as `ModelsResponse`,
    assert the same field set as L2.10. This is the offline part of
    class 3: it does not prove codex accepts the response, but it
    catches our own output regression before codex has to.

**Fallback mode (L1.4 = fallback):**

7''. WARN — "no `model_catalog_json` and no `auth.command`: codex
    resolves routes with fallback metadata, which is degraded. Generate
    a pin (static mode) or switch to `auth.command` (dynamic mode).
    See `scripts/codex-config-{static,dynamic}.toml.example`." Never
    FAIL — the legacy wiring is supported, just not recommended.

### L3 — live probes (`--live` only, ≤2×90s, costs nothing if codex is
unavailable)

The probe's wiring must mirror the user's actual mode so it tests what
they run, not a parallel wiring.

```
                user ~/.codex/
                config mode
                     |
                     v
    L3 dispatcher picks run_codex_dynamic | run_codex_pinned | run_codex_fallback
    based on the detected mode. Each is a lib helper analogous to
    scripts/e2e-lib.sh::run_codex but keyed to the detected wiring.

probe A:    codex --skip-git-repo-check -m <route> "<prompt>"
            fresh CODEX_HOME, route resolves through the active catalog path

probe B:    same conversation, switch -m to a different route key
            (always chat -> responses or vice versa, exercising the
            translation layer; both routes from validated.routes)

assertions on probe output:
- assistant section contains the prompt's reply marker
- router /metrics has +1 for the route's upstream_request counter
- for dynamic: the router log shows `codex client X detected` BEFORE
  the upstream request, proving the live /models fetch happened
- for pinned: the same `codex client detected` line never fires
  (intended, documented in the run)
```

`codexferry doctor --live` exit codes unchanged: 0 all pass, 1 any FAIL,
2 if codex is missing or fails to spawn.

### L4 — tripwire (passive, always running in the daemon)

Unchanged from the old spec: `observe_client_version` on `/models`
requests, `CodexVersionTracker` first-sighting detection, state file
under `$XDG_STATE_HOME/codexferry/doctor.json`. The new design only
adds L2.6 (version-age warning) — L4 still surfaces via the same logs,
but L2 catches the case where L4 has never been triggered yet.

## Failure bisection

| Observed failure | Diagnosis | Fix layer | Remediation snippet |
|---|---|---|---|
| L2.7 pin missing/parse fail | static-mode hard error | user config | rerun `codexferry gen-catalog --config cxf.toml --out ~/.codex/codexferry-catalog.json` |
| L2.8 / L2.9 pin ⊄ router routes | stale pin OR stale router | gen-catalog or router config | see L2.7 |
| L2.10 pin entry shape | codex upgrade added a required field | codexferry release | upgrade; stopgap hand-add the field in the generated file |
| L2.7' pin shadow in dynamic mode | user mixed static + dynamic | user config | remove one of the two |
| L3 probe A/B fails shape checks | codex upgrade changed dialect | `normalize.rs` / `convert/` | upgrade codexferry |
| L3 probe A/B fails sentinel | response contract changed | `convert/response.rs` | upgrade codexferry |
| L3 dynamic: no `codex client detected` in router log | class 3 broken — live fetch silently failing | router (`models_cache.rs`) | upgrade codexferry |
| L3 dynamic: `codex client detected` then probe fails on shape | codex accepts newer shape we don't emit | `catalog.rs::set_catalog_fields` | upgrade codexferry |

## Mode-specific shadow_check replacement

The old `shadow_check` (one INFO line, always) is replaced with three
mode-keyed checks:

- **pinned**: confirm the pin (existing message, mostly unchanged).
- **dynamic**: warn when both `model_catalog_json` and `auth.command`
  are set (the reverse advice, see L2.7').
- **fallback**: warn that the user is on the degraded wiring (see L2.7'').

The check is still never FAIL — it informs, never blocks.

## Testing strategy

- **Unit** (`src/doctor.rs`):
  - mode detection: parses sample `~/.codex/config.toml` shapes, returns
    pinned / dynamic / fallback correctly.
  - shadow mode-aware: per-mode fixture, asserts the right INFO text.
  - L2.8 / L2.9 reconciliation: stub `validated.routes` and a pin with
    matching/mismatching slugs; assert FAIL/WARN as appropriate.
  - L2.10 field-shape check: fixture with one entry missing each
    required field; assert the missing-field name appears in the FAIL.
- **Unit** (existing tests preserved): `cargo test` stays green;
  `quick_checks_never_fail_on_a_good_config` keeps its current
  guarantee against a representative dynamic-mode wiring.
- **End-to-end** (`scripts/e2e.sh`):
  - add `scenario_doctor_dynamic`, `scenario_doctor_pinned`,
    `scenario_doctor_fallback` — each spawns a temp router, drops a
    synthetic `~/.codex/config.toml` matching the mode, runs `doctor`,
    asserts the expected PASS / FAIL line set.
- **End-to-end real** (`scripts/e2e-real.sh`): unchanged — the live
  probe is what `doctor --live` does, and e2e-real already covers
  the dynamic + static wirings via `E2E_REAL_MODE`.

## Verification matrix

| | offline doctor | doctor --live | doctor (default = offline + live) |
|---|---|---|---|
| L1 mode detection (3 fixtures) | PASS | (run as part of either) | PASS |
| L2.7 pin missing (pinned mode) | FAIL with rerun-gen-catalog hint | FAIL | FAIL |
| L2.8 stale router (pin ⊋ router) | FAIL | FAIL | FAIL |
| L2.9 stale pin (pin ⊂ router) | FAIL | FAIL | FAIL |
| L2.10 field shape regression | FAIL | (probe will also fail) | FAIL |
| L2.7' dynamic + pin shadow | WARN | (probe green but warn present) | WARN |
| L3 dynamic: live fetch works | (not exercised by offline) | PASS | PASS |
| L3 pinned: tool round-trip works | (not exercised by offline) | PASS | PASS |
| L3 fallback: works with fallback metadata | (not exercised by offline) | PASS with WARN | PASS with WARN |

Default `codexferry doctor` runs L1 + L2 + L3 if codex is available (slow,
~2×90s worst case); `codexferry doctor --offline` is the fast path and
returns 0/1 only.

## User-visible changes (vs old spec)

- `doctor` adds mode-aware check set; the keyword "shadow_check" is
  gone from user output (replaced by mode-keyed INFO/WARN).
- `doctor --offline` is now also an explicit flag (the old spec said
  "default + live" — we keep the default-with-live behavior to preserve
  e2e parity, but `--offline` skips the L3 probes for the fast path).
- No new flags required to enable mode-aware checks — they're
  automatic from the detected mode.
- Mode detection emits a single INFO line per run naming the mode.

## Risks & mitigations

| Risk | Mitigation |
|---|---|
| Mode detection mis-classifies (e.g. dynamic + pin shadow falls into pinned if pin file is missing) | Test the mode detector with a fixture for each combination; L2.7' specifically catches the "I thought I was dynamic" case |
| `~/.codex/config.toml` not parseable | L1 degrades: codex wiring check is INFO-skipped, mode defaults to fallback, L3 still runs against the daemon's own view |
| The user has multiple `[model_providers.X]` with different modes (e.g. one dynamic, one pinned) | The check uses the active provider; the others are ignored. INFO note when multiple are configured. |
| Adding a new codex ModelInfo field breaking L2.10 | The fix is always `codexferry gen-catalog` + release; document the field in `catalog.rs::set_catalog_fields`. L2.10 is the offline tripwire — much faster signal than waiting for codex to error at session start. |
| `--live` probe takes 90s+ and is annoying for users who just want a sanity check | `--offline` flag; default remains live-aware for parity with the old spec. |

## Out of scope (explicitly deferred)

- **Verifying class 3 catalog acceptance in fallback mode.** codex
  never fetches `/models` there; no observable signal. Users on
  fallback should migrate (L2.7'' WARN); static users get L2.10; dynamic
  users get L3.A. The fallback case is supported, just not advised.
- **Live probe of class 3 shape in dynamic mode without `--live`.**
  The endpoint shape check (L2.9') is fast but does not prove codex
  accepts the response — only the live probe does. If `--live` is too
  slow for routine use, a future "shallow live" mode that does a
  5-second `codex debug models` instead of a full turn is a possible
  compromise; not in scope for the first cut.
- **Cross-mode parity testing.** We test each mode in isolation; a
  user who flips modes mid-session can hit cache TTL behavior
  (`models_cache.json` rewrites) that doctor does not model.
- **Auto-remediation.** Doctor prints fixes; it does not apply them.
  `gen-catalog` is the only automation available, and it's a user-
  invoked subcommand.

## Supersession of `2026-08-22-doctor-live-catalog-design.md`

| Old section | New location |
|---|---|
| Background (env_key assumption) | Replaced by Background above (mode matrix) |
| §Correction (env_key fetch gate) | Becomes L1.4 + L2.7'' (still relevant for fallback) |
| Goals 1, 3, 4, 5 | Unchanged, re-stated above |
| ~~Goal 2~~ (struck through) | Re-opened: dynamic mode makes it achievable; covered by L3.A |
| Threat model class 3 "Gap" | Re-classified: closed in dynamic by L3.A + L2.9', closed in pinned by L2.10; still a gap in fallback (deferred) |
| §2 Probe 1 "fallback-metadata path" | Reinterpreted: probe mode follows user mode; for dynamic the probe exercises live fetch (intent of the original spec); for fallback it's the same fallback path |
| §4 quick-check #4 "shadow_check" | Replaced by mode-keyed L2.7 / L2.7' / L2.7'' |
| §Threat model rows | Same rows, different "Covered today?" answers per mode |
| §Risks probe-1 row | Withdrawn; replaced by dynamic-mode "live fetch fails" row |
| §Deferred class 3 entry | Withdrawn; L2.10 + L3.A close it in dynamic and pinned |
