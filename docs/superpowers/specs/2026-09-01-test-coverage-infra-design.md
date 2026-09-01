# Design: Test-coverage infrastructure (cargo-llvm-cov) — measure all three layers

**Date:** 2026-09-01
**Status:** approved for implementation (brainstorm Q1–Q4 recapped below)
**Base:** main @ `6cd96e3`
**Branch:** `fix/test-coverage-infra` (created, no code yet)
**Related:**
- `tests/common/mod.rs` RouterGuard (the SIGKILL teardown this design replaces)
- `scripts/e2e-lib.sh:381` (e2e router/mock teardown — already SIGTERM)
- `docs/superpowers/specs/2026-09-01-models-cache-reload-staleness-design.md` (motivating incident cadence: coverage is how we notice untested paths BEFORE incidents, not after)

## Problem

The repo has three test layers (unit / integration / e2e) and NO coverage
measurement. Two consequences:

1. **Integration coverage is unmeasurable as-is.** The integration harness
   spawns the router as a SUBPROCESS (`CARGO_BIN_EXE_codexferry`,
   tests/common/mod.rs) and `RouterGuard::drop` tears it down with
   `child.kill()` = SIGKILL. LLVM counters flush to `.profraw` only at
   graceful process exit (atexit), so a SIGKILLed router loses everything:
   an integration coverage run would report the server's handlers at ~0%
   while counting only the in-test mock and test code — actively misleading.
2. **No documented way to answer "is this PR's change covered?"** After the
   2026-09-01 audit (PR #7–#12) needed a manual per-PR test-sufficiency
   review, coverage data would make such gaps visible mechanically.

## Goal

Accurate, per-layer coverage for unit / integration / e2e, with a combined
view — and a teardown that is **production-faithful**: tests stop the router
the way systemd does (SIGTERM → graceful → counters flush), with SIGKILL
only as a bounded fallback.

## Verified facts (spec inputs)

- The router handles SIGINT/SIGTERM via `with_graceful_shutdown(signal_shutdown())`
  (proxy/mod.rs:295–306, 419): in-flight requests finish, `main` returns,
  LLVM counters flush. Production systemd restarts already rely on this.
- `RouterGuard::drop` (tests/common/mod.rs:837) currently does
  `child.kill()` (SIGKILL) + `wait()` — exactly the flush-loss above. It is
  the SINGLE drop implementation (endpoints_metrics.rs imports the same
  struct).
- `scripts/e2e-lib.sh:381` tears down router+mock with plain `kill` =
  SIGTERM, then `wait` — the e2e router already flushes when instrumented;
  only the env/report recipe is missing.
- `src/bin/e2e-mock.rs` has NO signal handler (bare `axum::serve().await`) —
  SIGTERM default-kills it WITHOUT flush. Accepted: the mock is
  scaffolding; e2e coverage targets the ROUTER's code, not the mock's.
- `libc` is not a direct dependency (SIGTERM delivery needs it, or a
  shell-out to `kill(1)`).
- `tracing_subscriber::fmt` logs to STDOUT (discovered in the E5 test);
  coverage collection is unaffected.
- cargo-llvm-cov instruments the whole workspace via RUSTFLAGS, so
  `CARGO_BIN_EXE_codexferry` used by the integration harness IS
  instrumented; the only missing piece is the flush (fixed by the drop
  change) and the env/report recipe (fixed by the script).

## Decisions

Recap brainstorming Q1–Q4 (2026-09-01, all resolved to the recommended
options):

| | Decision | Rejected |
|---|---|---|
| **Q1** | SIGTERM → bounded poll (`try_wait`, 25ms, ~10s cap) → SIGKILL only as fallback → reap. Normal exit is milliseconds; production-faithful (systemd-style) | Pure SIGTERM + unconditional wait (an abandoned slow stream would hang the suite); keep SIGKILL (integration coverage stays ~0% — the problem statement) |
| **Q2** | `[target.'cfg(unix)'.dev-dependencies] libc = "0.3"` — test-only, unix-gated; `kill(2)`/`waitpid` are the stable APIs | Shell out to `kill(1)` (PATH dependency, extra process); tokio::process (its kill is still SIGKILL — does not solve the problem) |
| **Q3** | `scripts/coverage.sh unit\|integration\|e2e` + no-args help — the e2e recipe (LLVM_PROFILE_FILE subshell + report) is exactly the part that gets mistyped from docs | Docs-only commands (e2e recipe too easy to mis-copy); cargo aliases (cannot encapsulate the multi-line env dance) |
| **Q4** | Per-layer HTML (`coverage/unit\|integration\|e2e`) + terminal summary; CI-facing formats (lcov/codecov) added when a CI exists | lcov.info now (no CI to consume it); summary-only (loses the where-to-look view) |

## Design

### Part 1 — graceful RouterGuard teardown (the 5-line-class fix)

`RouterGuard::drop` becomes: SIGTERM → bounded wait (poll `try_wait`, ~25ms
granularity, hard cap ~10s) → SIGKILL only if still alive → `wait()` reap.
Normal case: the idle router exits in milliseconds. The cap bounds a
pathological in-flight-stream hang; losing counters in that rare fallback is
acceptable (the run's coverage for handlers may be partial, never silently
zero across the whole suite).

### Part 2 — measurement flows (scripts/coverage.sh + docs)

Three subcommands + a combined mode, wrapping cargo-llvm-cov:

> Descope note (2026-09-01, SDD final review): the "combined mode" is
> deferred until a workflow consumes it — same spirit as Q4's deferral of
> CI-facing formats. Per-layer reports remain the actionable views; adding a
> merged view later is additive (share one profraw dir across flows, then
> `cargo llvm-cov report`).

- `coverage.sh unit` — `cargo llvm-cov --bin codexferry --html
  --output-dir coverage/unit` (unit tests live inside the bin crate).
- `coverage.sh integration` — `cargo llvm-cov --test chat_conversion --test
  endpoints_metrics --test healing --test passthrough --test sessions
  --html --output-dir coverage/integration`. Valid ONLY with Part 1 landed
  (documented in the script's output).
- `coverage.sh e2e` — instrument, run `scripts/e2e.sh` with
  `LLVM_PROFILE_FILE` pointing at a shared profraw dir, then
  `cargo llvm-cov report --html --output-dir coverage/e2e` (the subshell/env
  recipe from the investigation, so normal builds stay clean).
- `coverage.sh` (no args) — summary + pointers; `-h` prints usage.

Artifacts: `coverage/` (gitignored). HTML for humans; `--summary-only`
available by env passthrough.

### Part 3 — best-practice notes (documented, not enforced)

- cargo-llvm-cov is the Rust standard (source-based; kcov/tarpaulin are
  legacy).
- Coverage is a **gap-finder, not a gate**: no absolute-number threshold in
  CI. The DSML-leak class of bug was a semantic gap no line-coverage number
  would have caught. Review-time patch coverage (did the lines I touched
  get exercised?) is the actionable signal.
- Per-layer reports matter because each layer exercises a different surface;
  the combined number is the "how much of the shipped binary is exercised
  at all" view.
- Exclusions: the e2e-mock bin is scaffolding (its coverage is noise);
  `#[cfg(test)]` code is excluded by tooling automatically.

## Testing / Verification

1. Integration suites stay green with the graceful drop (all ~16 endpoints +
   other suites; drops add milliseconds in the normal case).
2. `coverage.sh integration` reports non-zero coverage for
   `proxy::handle_responses` / passthrough / models handlers (the
   pre-fix expectation was ~0% — this is the acceptance proof).
3. `coverage.sh unit` and `coverage.sh e2e` produce valid reports.
4. A SIGKILL-fallback sanity: hard to force deterministically; the bounded
   loop is small enough to review directly.

## Docs sync

- README-DETAILS "Build, test, end-to-end" gains a "Test coverage"
  subsection (setup once, the three flows, the subprocess-flush caveat and
  its fix, the best-practice notes).
- AGENTS.md "Testing Strategy" gains a one-line pointer to coverage.sh.
- `.gitignore`: `coverage/`.

## Out of scope

- e2e-mock graceful shutdown (scaffolding; its coverage is not a target).
- CI integration of coverage (no CI exists in this repo today; the script is
  the seam for it later).
- doctor --live's real-`codex` subprocess (external binary, not ours).
- Renaming the harness's misleading `stderr_path` field (daemon logs go to
  stdout) — cosmetic, separate cleanup.

## Risks

| Risk | Mitigation |
|---|---|
| Graceful shutdown hangs on an abandoned in-flight stream | Bounded wait (≈10s) then SIGKILL; current tests always finish their streams |
| Drop blocking a test thread up to the cap | Only in the pathological case; normal exit is milliseconds |
| cargo-llvm-cov not installed locally | Script prints the one-time setup commands when the tool is missing |
| e2e profraw sprawl | `LLVM_PROFILE_FILE` with `%p-%m` pattern into a scratch dir; `llvm-cov clean` between flows |
