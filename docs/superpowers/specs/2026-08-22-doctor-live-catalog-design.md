# Design: doctor Without the Static Catalog — Live-Contract Probes and the Codex Version Tripwire

**Date:** 2026-08-22
**Status:** Design decisions approved in brainstorming; pending spec review
**Related:** `2026-08-22-codexferry-migration-design.md` (terminology rule, Layer 3)
**Replaces:** the offline drift-check half of `doctor` (the static-catalog flow)

## Background

codexferry now serves the model catalog live: Codex sends `client_version`
unconditionally on `GET /v1/models`, and the daemon answers with the
ModelsResponse catalog shape built from the hot-reloaded config
(`models_cache.rs` → `catalog::build_catalog_value`). The recommended setup
(README "Live Model Catalog via /models") has **no** `model_catalog_json`, no
`gen-catalog` step, and no `~/.codex/codexferry-catalog.json` file.

Offline `doctor` is still built entirely around that dead file:

- `doctor.rs:105-116` — reads the installed catalog and returns a single FAIL
  ("catalog readable and parseable") when it is missing, short-circuiting every
  other check. In the recommended setup the file never exists, so
  `codexferry doctor` always exits 1.
- `doctor.rs:142-152` — regenerate-and-compare drift check against that file.
- `doctor.rs:170-209` — wiring hints keyed on `model_catalog_json` presence.

`doctor --live` is self-contained (it synthesizes its own temp catalog,
`doctor_live.rs:108-119`) and works unchanged in the live-catalog world.

The real question this design answers: **with no static catalog, how do we
catch Codex-CLI upgrades that break the wire contract?**

### Decisions from brainstorming (recorded)

- **Rejected** — auto-detect static-vs-live mode by catalog-file presence
  (keeps the dead flow on life support; user rejected).
- **Rejected** — per-Codex-version `~/.codex/config.toml` templates (collides
  with users' personal config; a static version×config table is always one
  incident behind; codex can be configured per-invocation via `-c` overrides,
  so no file distribution is ever needed).
- **Rejected** — daemon auto-running `doctor --live` on version change
  (spawns codex, needs 2×90s probes and local approval environment; a
  reminder is safer than self-help).
- **Settled** — a detection → alarm → verification → remediation loop:
  the daemon detects version changes automatically (tripwire), `doctor`
  verifies the contract empirically with the installed codex (probes),
  failures bisect to a fix layer, and remediation guidance prints exact
  snippets. Catalog content stays **version-agnostic**; version-conditional
  output is an explicitly empty seam (see §6).

## Goals

1. `codexferry doctor` passes — and meaningfully checks — in the recommended
   live-catalog setup.
2. The live `/models` catalog contract is verified end-to-end (today it is
   the only catalog path and is never exercised by any probe).
3. A Codex upgrade is detected and surfaced automatically by the running
   daemon, with zero user configuration.
4. Doctor failures map to a fix layer and print actionable remediation,
   including stopgaps that do not require a codexferry release.
5. Offline checks survive as fast, catalog-free quick-checks.

## Non-goals

- Removing the `gen-catalog` subcommand (kept, repositioned as a stopgap /
  static-catalog tool).
- Auto-scheduling or auto-running doctor from the daemon.
- Populating the version-quirk table (seam only; §6).
- Changing the `/models` response shapes or the catalog generation policy.
- Renaming neutral technical vocabulary ("router" as the role, `axum::Router`,
  test helpers) — governed by the migration spec's Layer 3 rule.

## Threat model: what a Codex upgrade can break

| # | Class | Historical case | Covered today? |
|---|-------|-----------------|----------------|
| 1 | Request-side dialect drift (codex → codexferry): new tool delivery, new input item types | 2026-08-17 `additional_tools` incident | Runtime: `normalize.rs` translates + warns; `doctor --live` asserts shape |
| 2 | Response-side contract (codexferry → codex): codex's SSE consumer becomes stricter | — (latent) | `doctor --live` implicitly (codex must parse SSE, execute tool, exit 0) |
| 3 | Catalog acceptance: codex rejects `/models` entries (missing newly-required fields), `client_version` semantics change | ≥0.147 rejected entries missing `priority` etc. | **Gap — nothing.** Both probes pin a temp `model_catalog_json` |
| 4 | Template evolution: new bundled-template fields | `use_responses_lite` leak | Deny-by-default allowlist makes new fields inert; dropped-fields INFO |

Class 3 is the gap this design closes; classes 1/2/4 keep their existing
defenses, now behind a default-on verification.

## Design overview

```
                 ┌─ layer 1: verification ─────────────────────────────┐
 codexferry doctor (default) = quick-checks (<1s) + live probes
   probe 1: responses route, NO model_catalog_json ─→ forces live /models
   probe 2: chat route, pinned temp catalog ────────→ isolates dialect
                 └───────────────┬────────────────────────────────────┘
                                 │ green → writes last_green to state file
┌─ layer 2: tripwire ────────────┴─────────────────────────────────────┐
 /models handler observes client_version value (currently ignored)
   first-seen version (per process) → info log + metrics
   + compare against state file → warn "unverified" until doctor runs
└───────────────┬─────────────────────────────────────────────────────┘
                │
┌─ layer 3: passive ──────────────────────────────────────────────────┐
 normalize.rs runtime counters/warns (unknown item types, dropped tools)
 — unchanged; production-time visibility between doctor runs
└─────────────────────────────────────────────────────────────────────┘
```

Failure of a verification runs the remediation mapping (§5): bisect by which
probe failed, print fix layer + stopgap.

## Component designs

### 1. doctor CLI reshape

| Invocation | Behavior | codex absent |
|-----------|----------|--------------|
| `codexferry doctor` (default) | quick-checks (§4) **always**, then live probes | INFO "live probe skipped", exit by quick-checks |
| `codexferry doctor --live` | live probes only (current semantics) | exit 2 |
| `codexferry doctor --offline` | quick-checks only | n/a (never spawns codex, never exits 2) |

Exit codes unchanged: 0 all pass, 1 any FAIL, 2 environment unusable
(`--live` only).

Flag changes:

- **Removed:** `--catalog` (the installed-catalog audit is deleted with the
  static flow). Breaking CLI change; documented in "User-visible changes".
- **Kept:** `--codex-models` (explicit template path, feeds quick-check 2).
- **Kept:** `--config` (same default rule as the server:
  `CODEXFERRY_CONFIG` or `./config.toml`).

### 2. Live probes: division of labor

`doctor --live` keeps its topology (real `codex exec` → in-process temporary
codexferry instance → mock upstream), its 90s deadline, fresh `CODEX_HOME`
per probe, and the shape/sentinel/exit assertions. One change:

- **Probe 1** (`doctor/resp`, responses-format upstream): **omits the
  `-c model_catalog_json=…` override entirely.** The fresh `CODEX_HOME`
  contains no static catalog and the probe route exists in no bundled
  catalog, so codex can only resolve `doctor/resp` by fetching
  `/v1/models?client_version=…` from the temporary codexferry instance.
  Probe success therefore proves the whole live-catalog contract: codex
  issued the fetch, accepted the ModelsResponse shape, and resolved the
  route. (Refinement of the brainstorming variant "point at a nonexistent
  file": a missing file risks codex hard-erroring on the *file read* rather
  than falling back to the live fetch — a false red. Omission exactly
  simulates the README setup.)
- **Probe 2** (`doctorchat/chat`, chat-format upstream): unchanged — pinned
  temp catalog, isolating dialect conversion from catalog concerns.

Bisection table (drives §5):

| Observed pattern | Diagnosis | Fix layer |
|------------------|-----------|-----------|
| Probe 1 fails (codex error / no upstream request recorded), probe 2 passes | Class 3: live catalog contract | `catalog.rs` — add the missing field(s), release |
| Both probes fail shape checks (no function tools / `additional_tools` leak / unknown item types) | Class 1: request dialect drift | `normalize.rs` — translate the new dialect, release |
| Shape checks pass, sentinel/exit fail | Class 2: response contract (or local exec) | usually `convert/response.rs`; inspect the printed codex stderr tail |
| `codex --version` fails / spawn error | Environment | n/a — exit 2 |

A codex too old to fetch `/models` fails probe 1 **by design**: the
live-catalog flow is the only supported catalog path, so "this codex cannot
use it" is a correct red. The report wording for this failure names that
interpretation explicitly.

### 3. Codex version observation (the tripwire)

**Source:** the `client_version` query value on `/models` — parsed today
(`proxy/mod.rs:113`) but matched only for presence (`:391`). The value
arrives on every codex turn (ETag revalidation included), so observation
needs no extra requests.

**Component:** a `CodexVersionTracker` in `AppState`, peer of `Metrics` and
`CatalogCache`:

```rust
pub struct CodexVersionTracker {
    seen: Mutex<HashSet<String>>,     // distinct versions this process
    current: Mutex<Option<String>>,   // most recent
}
impl CodexVersionTracker {
    /// Updates `current` on EVERY call; returns Some(Transition{from, to})
    /// only when the version is newly added to `seen` (first sighting this
    /// process). `from` is the previous `current` (None at process start).
    pub fn observe(&self, version: &str) -> Option<Transition>;
}
```

`handle_models` calls `observe` before building the catalog. On
`Some(transition)` (re-sightings of an already-seen version return `None`
and do nothing):

1. `info!("codex client {from}→{to} detected — rerun `codexferry doctor`")`
   (from = `None` on first sighting after daemon start).
2. Metrics: set `codex_client_info{version=to} = 1` (a `Family` with a
   `version` label, gauge), increment `codex_client_changes` counter. Wire
   names follow the existing `metrics.rs` field-name convention.
3. Read the state file (§3.1); if `normalize_version(last_green)` differs
   from `normalize_version(to)`, `warn!("codex {to} not verified by doctor
   (last green: {last_green:?})")`.

Logging-convention note: this is a once-per-version **event** log, not a
per-request line. It joins the documented anomaly-only exceptions in
AGENTS.md §11 (the doc must be updated to list it).

**Flapping/multi-version:** logging fires once per distinct version per
process (`seen` set), so two alternating codex versions produce two lines,
not a stream. Metric label cardinality is bounded by distinct versions seen
— negligible for a personal local daemon.

#### 3.1 State file

Path: `$XDG_STATE_HOME/codexferry/doctor.json`, defaulting to
`~/.local/state/codexferry/doctor.json` when `XDG_STATE_HOME` is unset.
Deliberately **not** under `~/.codex/` — daemon-owned state lives on the
daemon's side (the static-catalog confusion, resolved).

Schema (single bounded object, rewritten atomically per doctor run):

```json
{
  "last_green": "0.158.0",
  "last_run": {
    "status": "green",
    "codex_version": "0.158.0",
    "at": "2026-08-22T12:34:56Z",
    "summary": "all checks passed"
  }
}
```

Rules:

- **Green run** → `last_green = normalize_version(codex --version)`, and
  `last_run` updated.
- **Red run** → `last_run.status = "red"` (+ version, time, failure summary);
  `last_green` **untouched** — the unverified warning persists until a green
  run.
- Doctor is the only writer; the daemon only reads. Missing/malformed file ⇒
  treated as never-green (warn path, never an error).
- Write failures log a warning and never affect the doctor verdict.

#### 3.2 `normalize_version`

`codex --version` output (`"codex-cli 0.158.0"`) and the `client_version`
parameter (`"0.158.0"`) must compare equal. Rule: trim; return the **last**
whitespace-separated token containing an ASCII digit; `None` when no token
qualifies (both sides then compare as "unverified", never as equal). No
semver parsing — versions are opaque strings.

### 4. Offline quick-checks (`offline_checks` rewrite)

All catalog-file-independent; the function keeps its name and the
`Check`/`print_report` machinery.

1. **config loads** — unchanged from today's check 2 (parse + validate +
   route count via `generate_catalog`'s loading path).
2. **template fields dropped** — unchanged INFO tripwire (still meaningful:
   the live catalog inherits through the same allowlist).
3. **codex wiring (upgraded)** — parse `~/.codex/config.toml` with the `toml`
   crate (replacing the `contains("127.0.0.1")` text heuristic at
   `doctor.rs:186-192`); for each `[model_providers.X]` with a `base_url`,
   PASS when it equals `http://{host}:{port}/v1`, where `host:port` comes
   from the codexferry config's `[server]` (defaults `127.0.0.1:8787`);
   mismatch → INFO printing the exact corrected snippet. File or table
   absent → INFO "no codex provider configured (skipped)". Never FAIL —
   wiring is user-side and best-effort, as today.
4. **static-catalog shadow** — `model_catalog_json` set in
   `~/.codex/config.toml` → INFO "shadows the live catalog; remove unless a
   deliberate stopgap (see doctor failure guidance)". Unset → no check
   emitted.
5. **version status** — spawn `codex --version` (absent → INFO skip);
   compare `normalize_version` with state `last_green`: equal → PASS;
   differs or never-green → INFO "codex {v} not verified (last green
   {last_green:?})". Never FAIL — visibility, not exit code.

**Deleted:** the installed-catalog read + regenerate-and-compare checks,
`--catalog`, and `default_catalog_path()`.

### 5. Failure handling & remediation guidance

- Doctor failure never touches the running daemon (diagnostic only).
- Exit codes per §1.
- On any FAIL, the report is followed by a **Remediation** section derived
  from the bisection table (§2), each entry naming the fix layer and a
  stopgap:
  - **Class 3 (catalog)** → fix: release adding the field(s). Stopgap:
    `gen-catalog` a static file, hand-add the missing fields, point
    `model_catalog_json` at it; or lock the codex version.
  - **Class 1/2 (dialect/response)** → fix: release. Stopgap: lock the
    codex version (if auto-update can be disabled).
  - **Codex-side config change (rare)** → print the minimal TOML snippet to
    add/change in `~/.codex/config.toml`.
  - The report includes `codex --version` and `codexferry --version` lines
    for issue filing.
- Stopgap note: hand-patching a generated catalog is viable precisely
  because `gen-catalog` remains available; this is its repositioned role.

### 6. Version-quirk seam (empty, with a hard constraint)

Rationale — the backward-compatibility asymmetry: codex's ModelInfo is
**strict about missing fields** (rejects) and **tolerant of unknown fields**
(serde ignores). Therefore *adding* a field to catalog output is safe for
all versions simultaneously: the normal adaptation is an unconditional
addition, no version branch. Version-conditional output is needed only for
mutually-exclusive requirements (new behavior vs. old requirement) — rare,
and none known today.

Seam reserved in `catalog.rs`:

```rust
fn catalog_quirks_for(client_version: &str) -> CatalogQuirks {
    CatalogQuirks::default() // empty today; populate only on a real conflict
}
```

**Hard constraint (recorded so a future population cannot silently break
caching):** if quirks ever become non-empty, the `CatalogCache` fingerprint
(`models_cache.rs:133-145`, currently route triples only) MUST include the
quirk set — otherwise two codex versions alternating requests would receive
each other's cached body and each other's ETag. Unknown/future versions use
`CatalogQuirks::default()` and log a warn; they are never refused service.

### 7. Terminology scope

Governed by the migration spec's Layer 3 rule: rename a token when it refers
to the *product*; keep neutral vocabulary (`axum::Router`, "the router
forwards…", test helpers). Within this work:

- `catalog.rs:275` — the user-visible description string
  `"Router route {route_key}"` (shown in codex's model picker) becomes
  `"codexferry route {route_key}"`. Catalog content changes ⇒ ETag changes ⇒
  codex refetches once; benign.
- New doctor/tripwire user-facing strings use "codexferry" naming.
- No sweep of neutral internal vocabulary.

## Testing strategy

- **Unit** (`doctor.rs` tests, rewritten):
  - quick-checks: fresh-config pass; broken-config FAIL on "config loads";
    wiring mismatch yields INFO + corrected snippet (temp TOMLs); shadow
    notice present/absent; version-status equal/differs/never-green/codex-
    absent.
  - the two catalog-file tests (`fresh_catalog_passes_all_offline_checks`,
    `hand_edited_catalog_fails_equality_check`,
    `missing_or_unparsable_catalog_fails`) are deleted with the feature;
    `broken_config_fails_config_load_check` survives.
- **Unit** (new): `CodexVersionTracker::observe` transitions (first sighting,
  repeat, second version, flapping); `normalize_version` cases; state-file
  read/write round-trip + malformed-file handling.
- **Integration** (`tests/endpoints_metrics.rs`): `GET /v1/models` twice
  with different `client_version` values → `/metrics` scrape shows the
  `codex_client_info` family with both version labels and the change
  counter ≥ 1; the 304/ETag path still works alongside observation.
- **doctor --live**: unchanged unit surface (`pick_tool` etc.); the probe
  split itself is manual verification (needs real codex), per the matrix in
  Verification.
- **CI note**: quick-checks must pass in environments without codex and
  without `~/.codex/` (all such checks are INFO-skips, never FAIL).

## Verification (post-implementation matrix)

1. `cargo test` — full suite green.
2. `codexferry doctor --offline` in the repo — green without `~/.codex/`.
3. `codexferry doctor` with codex installed — green; state file written;
   rerun → version-status PASS.
4. Simulate an upgrade on a **freshly started** daemon (the tripwire fires
   only on a version's first sighting per process): set the state file's
   `last_green` to another version, hit `/models?client_version=<other>` →
   one info line + one unverified warn; `/metrics` shows the family.
   Repeat the request → no second log line.
5. Failure path (best effort): point `~/.codex/config.toml` at a wrong port
   → wiring INFO with snippet. Probe-level reds are verified by temporarily
   breaking the mock (dev-only).

## User-visible changes

- `doctor` default now also runs live probes (needs codex for full
  verification; slower by ~2×90s worst case). `--offline` is the fast path.
- `--catalog` flag removed; the regenerate-and-compare audit is gone.
  Static-catalog users: `gen-catalog` still writes the file; doctor no
  longer audits it — contract assurance comes from `--live`.
- New daemon log lines: version-change info + unverified warn (once per
  version per process).
- New `/metrics` family + counter.
- Catalog `description` field changes text ("codexferry route …").
- New state file under `$XDG_STATE_HOME`/`~/.local/state/codexferry/`.

## Docs sync (repo convention §13)

- **README**: rewrite the doctor section + upgrade runbook (default doctor,
  `--offline`/`--live` split, failure guidance, stopgaps); reposition
  `gen-catalog` as stopgap/static tool; document the tripwire log lines and
  state file.
- **ARCHITECTURE.md**: doctor module descriptions; §11 line counts.
- **AGENTS.md**: §11 exception list gains the version-change event log;
  module-responsibility rows for `doctor.rs` / `doctor_live.rs` /
  `models_cache.rs` / `metrics.rs` updated.

## Risks & mitigations

| Risk | Mitigation |
|------|-----------|
| Probe 1 depends on codex resolving routes via live `/models` | That dependency **is** the contract under test; failure wording states the "codex too old / fetch broken" interpretation explicitly |
| `client_version` format varies / unparseable | `normalize_version` degrades to "unverified" — never blocks serving, never false-green |
| Two codex versions alternate (flapping) | Once-per-version-per-process logging; bounded metric cardinality |
| State file unwritable / malformed | Warn and proceed; treated as never-green |
| Users relying on `--catalog` | Breaking change is documented; `gen-catalog` output remains valid for codex itself |

## Deferred (explicitly out of scope)

- Populating `catalog_quirks_for` (seam + cache-key constraint recorded).
- Exposing `normalize.rs` counters as `/metrics` families (layer-3
  extension; current log-based visibility suffices).
- Auto-scheduled doctor runs.
- Removing `gen-catalog`.
