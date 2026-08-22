# Design: Migration & Rename — `codex-router` → `codexferry`

**Date:** 2026-08-22
**Status:** Design decisions approved in brainstorming; pending spec review
**Old repo:** `~/pilot/codex-router-rs` (238 commits, retained read-only as reference)
**New repo:** `~/pilot/codexferry` (empty, `git init` done, no commits yet)

## Background

`codex-router` is a local proxy daemon that lets Codex CLI (≥0.128) use
Chat-Completions-only upstreams (DeepSeek, Kimi, GLM, SiliconFlow, …) through a
single Responses-API endpoint. The name collides with existing GitHub projects,
making the repo unfindable, and renaming only gets more painful as the project
gains visibility. The 2026-08-22 brainstorming settled the new name:
**`codexferry`** (no hyphen) — "Codex 摆渡" in Chinese, a metaphor that covers
both protocol conversion and multi-provider routing. Both crates.io and GitHub
are clear.

### Decisions from brainstorming (recorded)

1. **Name** — `codexferry`, single binary, **no short alias**. This is a daemon
   invoked 0–1 times a day; every subcommand is copy-pasted from the README with
   long flags, and the high-frequency command in daily use is `codex`, not this
   binary. An abbreviation would buy nothing and cost discoverability.
2. **Rejected alternative** — `codexspan`, the earlier front-runner, was vetoed:
   it is one letter from `codespan` (11.07M downloads) and `codespan-reporting`
   (132M downloads), and the entire target audience is Rust users, so a typo
   lands on a genuinely well-known crate.
3. **Git history — fresh start.** The new repo begins with a single initial
   commit containing already-renamed code. The published repo carries no trace
   of the old name. The 238-commit history and its `blame` stay available in the
   retained old repo.
4. **Env vars — hard rename, no compatibility shim.** `CODEX_ROUTER_*` →
   `CODEXFERRY_*` with no fallback and no deprecation warning. The project is
   unpublished and the only user is the author; a compat layer would be pure
   debt.
5. **Scope — pure migration and rename.** Behavior is frozen. Known defects
   (`normalize.rs` non-injective encoding, `doctor_live.rs` TOCTOU, issue #15
   leftovers) are explicitly **out of scope** and get their own specs against
   the new repo.

## Goals

1. `~/pilot/codexferry` contains the full working tree of `codex-router-rs` at
   `master` (a5d1956), renamed, with no old-repo commit history (see §Commit
   sequence and §Open question 2 for how many commits that is).
2. **Behavior frozen, proven by test count:** `309 passed + 1 ignored = 310`,
   identical before and after — same targets, same per-target counts (see
   §Baseline).
3. Zero occurrences of the brand name `codex-router` / `codex_router` /
   `CODEX_ROUTER_` in code, config, scripts, and the three top-level docs.
   Historical documents under `docs/superpowers/` are deliberately excluded —
   see §Migration mechanics item 4 and §Open question 1.
4. Neutral technical vocabulary is **preserved**, not renamed (see §Layer 3).
5. The author's live environment (`~/.codex/config.toml`) keeps working, updated
   in the same change.
6. The three top-level docs (README, AGENTS, ARCHITECTURE) stay truthful per the
   existing AGENTS.md convention #13.

## Non-goals

- No behavior changes of any kind. No bug fixes, no refactors, no dependency
  changes, no new features. Every known defect listed in §Deferred stays as-is,
  comments and TODOs included.
- No test assertion edits beyond the mechanical identifier rename.
- No renaming of neutral technical terms (§Layer 3).
- No crates.io publish and no GitHub push in this spec — the repo is prepared
  locally; releasing is separate work.
- No history rewrite of the old repo. `~/pilot/codex-router-rs` is not modified,
  not deleted.

## Baseline (measured 2026-08-22, old repo at a5d1956)

`cargo test` on the old repo, to be reproduced exactly on the new one:

| Target | Tests |
|---|---|
| lib (unit, in-source `#[cfg(test)]`) | 269 |
| `src/bin/e2e-mock.rs` | 5 |
| `tests/chat_conversion.rs` | 9 |
| `tests/endpoints_metrics.rs` | 9 (8 passed + **1 ignored**) |
| `tests/healing.rs` | 7 |
| `tests/passthrough.rs` | 8 |
| `tests/sessions.rs` | 3 |
| **Total** | **309 passed + 1 ignored = 310** |

The ignored test is `doctor_live_probe_report_has_no_failures`
(`tests/endpoints_metrics.rs:426`), gated `#[ignore = "requires local codex CLI"]`.
It must remain ignored-not-removed, and it is one of the three
`CARGO_BIN_EXE` sites (§Layer 1).

Source scale: 14,877 lines across 38 `.rs` files in `src/`, plus 3,244 lines
across 6 files in `tests/` (18,121 total).

Rename surface, scanned 2026-08-22. Counted as **occurrences**, and split by
whether historical `docs/superpowers/` documents are included — they are excluded
from the grep gate per §Migration mechanics item 4, so the "code + top-level
docs" column is the actual work:

| Token | Code + top-level docs | Incl. `docs/superpowers/` |
|---|---|---|
| `codex-router` (brand) | 54 in 14 files | 98 in 26 files |
| `CODEX_ROUTER_*` | 32 in 12 files | 43 in 17 files |
| `codex_router` (crate name) | 8 in 3 files | 10 in 5 files |

(The brainstorming memory recorded 102/44/10 — those figures included
`docs/superpowers/` and counted slightly differently. The numbers above are
re-measured at a5d1956 and are what the gates assert against.)

Only 3 distinct env vars exist: `CODEX_ROUTER_CONFIG`, `CODEX_ROUTER_DUMMY`,
`CODEX_ROUTER_TRACE_BODY`.

## The core risk, and the layering that manages it

The word "router" appears **368 times in `src/` + `tests/`** (510 across the
whole repo excluding `target/` and `docs/superpowers/`), but **only 22 of those
368 are the brand name**. `axum::Router`, `build_router`, `router_port`,
`router_url` are ordinary technical vocabulary; renaming them would produce
unidiomatic Rust and a diff too large to review. A blanket
`sed -i s/router/ferry/g` would corrupt the codebase.

So the rename is split into three layers, applied in order, each its own commit.
**Layer 3 is a non-goal made explicit** — it exists in this spec precisely so the
implementation knows what to leave alone.

### Layer 1 — Must change: brand identity

Mechanical, no compatibility concerns. Exact sites:

| Site | Current | New |
|---|---|---|
| `Cargo.toml:2` | `name = "codex-router"` | `name = "codexferry"` |
| `src/main.rs:59` | `#[command(name = "codex-router")]` | `name = "codexferry"` |
| `src/main.rs:1,52,70` | doc comments naming the binary | `codexferry` |
| `src/proxy/mod.rs:359` | `info!("codex-router listening on …")` | `codexferry listening on …` |
| `src/bin/e2e-mock.rs:26` | `about = "… for codex-router E2E scripts"` | `codexferry` |
| `src/logging.rs:11,34` | `EnvFilter::new("codex_router=info")` | `codexferry=info` |
| `src/logging.rs:17,19,21` | `RUST_LOG=codex_router=trace codex-router` | `codexferry` |
| `src/upstream.rs:578` | `format!("codex_router_key_{}", pid)` | `codexferry_key_{}` |
| `config.toml:2` | `# codex-router 配置文件` | `# codexferry 配置文件` |
| `scripts/*.sh` | binary path `target/debug/codex-router`, temp dir `codex-router-e2e.XXXXXX`, build `--bin codex-router` | `codexferry` |

The crate name `codex_router` → `codexferry` follows automatically from
`package.name`; the `codex_router` source occurrences are `RUST_LOG` filter
strings and the temp-dir prefix above, not `use` paths. The crate is
binary-only — no `src/lib.rs`, no `[lib]`/`[[bin]]` sections in `Cargo.toml`,
and zero `use codex_router::` paths — so renaming `package.name` has no
module-path fallout.

**Compile-time coupling that `package.name` does *not* handle automatically.**
Three integration-test sites hard-code the binary name inside a macro:

| Site | Current |
|---|---|
| `tests/endpoints_metrics.rs:315` | `env!("CARGO_BIN_EXE_codex-router")` |
| `tests/endpoints_metrics.rs:438` | `env!("CARGO_BIN_EXE_codex-router")` |
| `tests/common/mod.rs:725` | `env!("CARGO_BIN_EXE_codex-router")` |

Plus a doc comment naming it at `tests/common/mod.rs:24`. These become
`env!("CARGO_BIN_EXE_codexferry")`. This is the single most failure-prone item in
Layer 1: `env!` is resolved at compile time against the Cargo-injected variable,
so renaming `package.name` without these edits is a **hard compile error in the
test targets only** — `cargo build` still succeeds, and the breakage surfaces
only at `cargo test`. Conversely, editing these without `Cargo.toml` fails the
same way. They must land in the same commit as `Cargo.toml`.

Verified empirically on a scratch crate (rename `package.name` while leaving
`env!("CARGO_BIN_EXE_oldname")` in a test): `cargo build` exits 0, `cargo test`
fails with `environment variable CARGO_BIN_EXE_oldname not defined at compile
time`. Note this also means **verification step 1 (build) cannot catch it** —
only step 2 can, which is a further reason the test-count gate is
non-negotiable.

### Layer 2 — Must change, but touches a live contract

These break the author's working environment if changed without a matching edit
elsewhere. Each pairs a repo change with an environment change.

**2a. Env var prefix (hard rename, per decision 4):**

| Old | New | Notable sites |
|---|---|---|
| `CODEX_ROUTER_CONFIG` | `CODEXFERRY_CONFIG` | `src/main.rs:158`, `src/proxy/mod.rs:251`, `src/config.rs:4`, `tests/endpoints_metrics.rs:317,441`, `tests/common/mod.rs:716,727`, `scripts/e2e-lib.sh:74`, `scripts/e2e-real.sh:51`, `config.toml:4`, README:212,355 |
| `CODEX_ROUTER_TRACE_BODY` | `CODEXFERRY_TRACE_BODY` | `src/proxy/mod.rs:74,77,82,497`, `src/proxy/chat.rs:77,425`, `src/logging.rs:23,25`, AGENTS:184,299, README:61,213,463 |
| `CODEX_ROUTER_DUMMY` | `CODEXFERRY_DUMMY` | README:73,79,248,254 — **and `~/.codex/config.toml`** |

`CODEXFERRY_DUMMY` is the one env var that lives outside the repo: Codex CLI
requires a non-empty API key, and the proxy supplies the real one. Renaming it
in the repo without editing `~/.codex/config.toml` silently breaks every local
Codex invocation.

**2b. Codex-side provider key — and a documentation drift found while scanning.**

The README documents the Codex-side provider as `[model_providers.codex-router]`
(README:244), but the author's actual `~/.codex/config.toml` uses:

```toml
model_provider = "router"
model_catalog_json = "router-catalog.json"

[model_providers.router]           # ← key is "router", not "codex-router"
name = "codex-router"              # ← only the display name matches the README
base_url = "http://127.0.0.1:8787/v1"
wire_api = "responses"
env_key = "CODEX_ROUTER_DUMMY"
```

This drift predates the rename and is worth recording rather than silently
"fixing": the provider **key** is arbitrary and local, while the README shows a
canonical form. The spec resolves it by making both consistent under the new
name:

```toml
model_provider = "codexferry"
model_catalog_json = "codexferry-catalog.json"

[model_providers.codexferry]
name = "codexferry"
base_url = "http://127.0.0.1:8787/v1"
wire_api = "responses"
env_key = "CODEXFERRY_DUMMY"
```

Note `model_catalog_json` is *not* required when Codex points at a running
codexferry — the live `/models` catalog covers it, served automatically because
Codex always sends `client_version`. Per README, the static file exists for
`doctor`'s offline checks and for users who cannot point Codex at the daemon.
Why the author's config sets it anyway is not recorded; it is renamed here for
consistency, not necessity (see §Open question 3).

**2c. Default catalog filename `router-catalog.json` → `codexferry-catalog.json`.**

This is a *default path computed in code* (`src/doctor.rs:87,94`, defaulting to
`$HOME/.codex/router-catalog.json`, documented at `src/main.rs:86,112` and
README:275,336), with 3 more references in `doctor.rs` tests. Renaming the
default means the existing `~/.codex/router-catalog.json` (265KB, generated
2026-08-19) is no longer found. Since `~/.codex/config.toml` references it by
name too, 2b and 2c must land together with the file copied to its new name.

**Ordering constraint:** Layer 2 changes are only safe once applied *atomically
with* the environment edits. §Verification step 5 (live environment gate) is
what proves the live environment still works — no in-repo test can, since none
of them read `~/.codex/`.

### Layer 3 — Must NOT change: neutral technical terms

Of the 368 `router` occurrences in `src/` + `tests/`, the following are ordinary
vocabulary and stay verbatim. Renaming them is a **defect**, not thoroughness:

| Identifier | Count | Why it stays |
|---|---|---|
| `Router`, `build_router` | 16 + 4 | `axum::Router` is the framework's own type |
| `router_url`, `router_port` | 87 + 8 | describes "the HTTP server under test" |
| `start_router*`, `spawn_router*`, `router_task`, `router_config`, `test_router_with_routes` | 31 | test-harness helpers naming the process being spawned |
| bare `router` in prose | 172 | "the router forwards…", "router-managed fields" — the *role*, not the product |

Also unchanged: config-error text `"must not name a router-managed field"`
(`src/config.rs:259`) and the `router-managed` phrasing at `src/config.rs:142,257,352,371,375,591`
— asserted by tests, and correct English regardless of product name.

The judgment call: rename a token when it refers to *the product*; keep it when
it refers to *a routing proxy in general* or to *axum*. Borderline cases resolve
toward keeping, since Layer 1/2 already guarantees goal 3 (no brand-name
occurrences) — `router` alone is not the brand name.

## Migration mechanics

Fresh-start history (decision 3) means the new repo is not a `git clone`. The
working tree is copied, `.git` excluded:

1. Copy the old working tree to `~/pilot/codexferry`, excluding `.git/`,
   `target/`, `.worktrees/`.
2. Carry `.gitignore` (`/target`, `.worktrees/`) unchanged.
3. **Do not** carry `.claude/settings.local.json` — it contains stale
   permission entries naming `target/release/codex-router` and one-off `curl`
   allowances from the naming research. A fresh file is written with the
   equivalent entries under the new binary name, or omitted entirely.
4. `docs/superpowers/specs/` and `plans/` carry over. Two filenames contain the
   old name (`2026-08-13-codex-router-design.md`,
   `2026-08-13-codex-router.md`) — they are renamed via `git mv`-equivalent to
   `…-codexferry-design.md` / `…-codexferry.md`.
   **Historical spec/plan bodies are left textually intact**: they are dated
   records of decisions made under the old name, and rewriting them would
   falsify history. Goal 3 is scoped to exclude these historical documents; a
   note at the top of each records the rename. *(This is the one deliberate
   exception to "zero occurrences" — see §Open question 1.)*
5. This spec, and the AGENTS.md `Spec:`/`Plan:` pointers, reference the renamed
   filenames.

### Commit sequence

The new repo's history is intentionally short. Layers land as separate commits
so review can follow them, then the question of whether to squash to a single
initial commit is settled by §Open question 2.

| # | Commit | Contents |
|---|---|---|
| 1 | `chore: import codex-router-rs working tree at a5d1956` | Verbatim copy, pre-rename. Establishes the diff base so layers 1–3 are reviewable. |
| 2 | `refactor: rename brand identity to codexferry (layer 1)` | §Layer 1 table |
| 3 | `refactor!: rename env vars to CODEXFERRY_* (layer 2a)` | §2a |
| 4 | `refactor!: rename Codex provider key and catalog default (layer 2b/2c)` | §2b, §2c |
| 5 | `docs: sync README/AGENTS/ARCHITECTURE and spec filenames` | doc updates, `docs/` renames |

Commits 3 and 4 are marked `!` (breaking) even though nothing external depends
on them yet — they change a documented interface, and the marker is what makes
that legible if the project is later published.

## Verification

Each step is a hard gate; a failure stops the migration rather than being worked
around.

1. **Build:** `cargo build --release` clean, no warnings introduced relative to
   the old repo's baseline.
2. **Test count invariant:** `cargo test` yields exactly `309 passed, 1 ignored`
   with the same per-target breakdown as §Baseline. A changed count means the
   rename touched behavior — the whole point of this gate.
3. **Grep gate:** zero matches for `codex-router`, `codex_router`,
   `CODEX_ROUTER_` outside `docs/superpowers/` (per §Migration mechanics item 4):

   ```
   grep -rn "codex-router\|codex_router\|CODEX_ROUTER_" \
     --include=*.rs --include=*.toml --include=*.sh --include=*.md \
     --exclude-dir=target --exclude-dir=superpowers --exclude-dir=.git .
   ```

   Expected: empty. Use `--exclude-dir`, **not** a `| grep -v '^./docs/…'`
   pipe: `grep -r .` emits paths without a `./` prefix, so an anchored `^./`
   filter silently matches nothing and the gate would pass vacuously.

   On the old repo this command returns **84 matching lines across 17 files** —
   the exact work list for layers 1–2. (Line counts, not occurrence counts: a
   line containing the name twice counts once, which is why these do not sum to
   the 94 occurrences in §Baseline's first column.)

   | File | Matches |
   |---|---|
   | `README.md` | 27 |
   | `src/proxy/mod.rs` | 7 |
   | `src/main.rs` | 7 |
   | `src/logging.rs` | 7 |
   | `AGENTS.md` | 6 |
   | `tests/endpoints_metrics.rs` | 5 |
   | `tests/common/mod.rs` | 5 |
   | `scripts/e2e-real.sh` | 4 |
   | `ARCHITECTURE.md` | 4 |
   | `src/proxy/chat.rs` | 2 |
   | `scripts/e2e.sh` | 2 |
   | `scripts/e2e-lib.sh` | 2 |
   | `config.toml` | 2 |
   | `src/upstream.rs` | 1 |
   | `src/config.rs` | 1 |
   | `src/bin/e2e-mock.rs` | 1 |
   | `Cargo.toml` | 1 |
   | **Total** | **84** |

   (`README.md` dominates because it documents every env var and CLI invocation.
   8 of the 84 lines contain two or more of the tokens, which is the whole of
   the 94-vs-84 gap.)
4. **Layer 3 preservation gate:** the neutral identifiers survive at their exact
   §Layer 3 counts. Guards against an over-eager `sed` — the failure this catches
   is the opposite of step 3's, and neither gate implies the other:

   ```
   for t in '\bRouter\b:16' '\bbuild_router\b:4' \
            '\brouter_url\b:87' '\brouter_port\b:8'; do
     pat="${t%:*}"; exp="${t##*:}"
     got=$(grep -rho "$pat" --include=*.rs src tests | wc -l)
     [ "$got" = "$exp" ] && echo "OK   $pat = $got" \
                         || echo "FAIL $pat expected $exp got $got"
   done
   ```

   All four must print `OK` (verified against the old repo at a5d1956).
5. **Live environment gate:** with `~/.codex/config.toml` updated and the
   catalog copied, start the daemon and confirm a real `codex` invocation
   completes end-to-end (`codex -m siliconflow/deepseek-v4-flash`, the author's
   configured default). This is the only check that covers the Layer 2 contract,
   since no repo test can see `~/.codex/`.
6. **E2E scripts:** `scripts/e2e.sh` passes (real Codex CLI against the scripted
   `e2e-mock` upstream, offline, zero tokens). `scripts/e2e-real.sh` is *not* a
   gate — it spends real tokens against live upstreams; run at the author's
   discretion.
7. **doctor:** `codexferry doctor --config config.toml` passes offline, and
   `doctor --live` passes, confirming the Codex ↔ codexferry contract survived
   the catalog-filename change.

## Deferred (explicitly out of scope)

Recorded here so they are not silently lost, each needing its own spec against
the new repo:

- `src/normalize.rs:56` — namespaced-tool encoding is **not injective**: `a`/`b-c`
  and `a-b`/`c` collide. A real latent bug.
- `src/normalize.rs:106,174,225,262` — spec §9 TODOs on the Responses hoist path.
- `src/doctor_live.rs:110` — `free_port()` TOCTOU race.
- `src/doctor_live.rs:112` — `pick_tool` should prefer `exec_command`.
- `src/doctor_live.rs:36` — on failure, print the last 20 lines of the daemon log.
- Issue #15 leftovers and issue #14 streaming-timeout notes: the fixes are
  already merged; the remaining items are documented follow-ups.
- The old repo's open issues are not migrated mechanically — issue *numbers* are
  referenced throughout the source comments (`issue #14`, `issue #15 item 2`).
  Those references stay pointing at the old repo's numbering, which the retained
  old repo keeps resolvable. Renumbering them would be a large, error-prone,
  behavior-adjacent edit for no gain.

## Open questions

1. **Historical spec/plan bodies** — §Migration mechanics item 4 proposes leaving
   old-name text intact inside `docs/superpowers/` as dated records, making goal 3
   scoped rather than absolute. The alternative is rewriting them for a
   uniformly clean repo at the cost of falsifying dated decisions. *Recommend
   leaving intact.*
2. **Squash to one initial commit?** Decision 3 says the published repo starts
   fresh, and the §Commit sequence above gives 5 reviewable commits. These are
   compatible if commits 1–5 are squashed once verification passes, or if "fresh
   start" is read as "no *old-repo* history" and the 5 stay. *Recommend keeping
   the 5* — they are the migration's audit trail, contain no old-name-era
   history, and commit 1 is what makes the rename diff reviewable at all.
3. **`model_catalog_json` retention** — §2b renames it, but per README it is only
   needed for `doctor`'s offline checks or when Codex cannot reach the daemon,
   and a stale static catalog can mask live-catalog changes. Worth dropping from
   the author's config rather than renaming? Behavior-adjacent, so flagged rather
   than decided. Note §Verification step 7 exercises `doctor` both ways, so
   whichever choice is made is covered.
