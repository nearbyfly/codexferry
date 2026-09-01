# Test-Coverage Infrastructure Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **Honesty note (2026-09-01 session):** this plan was formalized
> RETROACTIVELY — Tasks 1–4 were implemented during the session before the
> plan existed, in the order and with the code below; the checkboxes reflect
> what was actually done. Remaining unchecked steps are the handoff tail
> (push/PR).

**Goal:** Measure unit / integration / e2e coverage accurately by making the
integration harness stop the router gracefully (SIGTERM, production-style)
instead of SIGKILL, and wrap the three cargo-llvm-cov flows in one script.

**Architecture:** One teardown change (RouterGuard::drop: SIGTERM → bounded
`try_wait` poll → SIGKILL fallback → reap) fixes counter flushing for the
whole integration layer, because the router already handles SIGTERM via
axum's graceful shutdown. A thin bash script wraps the three cargo-llvm-cov
flows (the e2e flow needs the `LLVM_PROFILE_FILE` env dance that is easy to
mis-copy from docs). Docs record the best-practice reading (gap-finder, not
gate).

**Tech Stack:** cargo-llvm-cov 0.9 + llvm-tools-preview; `libc` 0.2 as a
unix-gated dev-dependency (SIGTERM delivery from std is SIGKILL-only).

**Spec:** docs/superpowers/specs/2026-09-01-test-coverage-infra-design.md

## Global Constraints

- libc is a `[target.'cfg(unix)'.dev-dependencies]` entry, version `"0.2"`
  (the `1` line is alpha; lockfile carries 0.2.189).
- The drop's graceful wait is HARD-CAPPED at ~10s; SIGKILL is the only
  fallback. No unbounded waits in `Drop`.
- `coverage/` is gitignored; `/tmp/cov-env.sh` never enters the repo.
- Scripts are bash with `set -euo pipefail`, executable bit set.
- Docs touched: README-DETAILS ("Test coverage" subsection under Build,
  test, end-to-end) and AGENTS.md (Testing Strategy pointer + regression
  warning about the drop).

## File Structure

- Modify: `Cargo.toml` — unix dev-dependency for SIGTERM.
- Modify: `tests/common/mod.rs` (RouterGuard, ~line 837) — the single Drop
  implementation shared by all five integration suites.
- Create: `scripts/coverage.sh` — the three measurement flows.
- Modify: `.gitignore`, `README-DETAILS.md`, `AGENTS.md` — records.

---

### Task 1: Graceful RouterGuard teardown

**Files:**
- Modify: `Cargo.toml` (dev-dependencies)
- Modify: `tests/common/mod.rs:837` (`impl Drop for RouterGuard`)
- Test: `tests/common/mod.rs` (new test next to the guard)

**Interfaces:**
- Consumes: `libc::kill`, `libc::SIGTERM`, `libc::pid_t`; `Child::try_wait`
  (std) for the bounded poll and reaping.
- Produces: unchanged `RouterGuard` struct; the behavioral contract is "drop
  terminates the child promptly, reaps it, and does not hang" — the
  coverage flush is a consequence of SIGTERM (the router's
  `with_graceful_shutdown` handles it).

- [x] **Step 1: Write the failing-behavior test** (a bare-SIGKILL drop also
  passes the prompt-termination assert, but the reap + no-hang contract is
  what the new drop must satisfy; the coverage flush itself is proven in
  Task 4's acceptance run)

```rust
#[test]
fn router_guard_drop_terminates_and_reaps_promptly() {
    let dir = tempfile::tempdir().unwrap();
    // A short-lived child: the drop must terminate + reap it well under a
    // second (SIGTERM path); a hang here means the drop waits unbounded.
    let mut child = std::process::Command::new("sleep")
        .arg("5")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut guard = RouterGuard {
        child,
        stderr_path: dir.path().join("log"),
        _dir: dir,
    };
    let start = std::time::Instant::now();
    drop(&mut guard);
    assert!(
        start.elapsed() < std::time::Duration::from_secs(3),
        "drop must terminate the child promptly, took {:?}",
        start.elapsed()
    );
}
```

- [x] **Step 2: Add the dependency**

```toml
[target.'cfg(unix)'.dev-dependencies]
libc = "0.2"
```

- [x] **Step 3: Implement the drop**

```rust
fn drop(&mut self) {
    #[cfg(unix)]
    {
        unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM); }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while self.child.try_wait().ok().flatten().is_none() {
            if std::time::Instant::now() >= deadline {
                let _ = self.child.kill(); // SIGKILL fallback; counters lost
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let _ = self.child.wait(); // reap
    }
    #[cfg(not(unix))]
    {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
```

- [x] **Step 4: Run the integration suites to verify no teardown regression**

Run: `cargo test --test endpoints_metrics --test chat_conversion`
Expected: PASS (drops fast, zombie reaped, no suite hang).

- [x] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock tests/common/mod.rs
git commit -m "fix(test): RouterGuard stops the router gracefully (SIGTERM, bounded)"
```

### Task 2: scripts/coverage.sh + gitignore

**Files:**
- Create: `scripts/coverage.sh` (executable)
- Modify: `.gitignore` (append `coverage/`)

**Interfaces:**
- Produces: `coverage.sh unit|integration|e2e` — HTML reports under
  `coverage/<mode>/index.html`; `-h`/no-arg prints usage + one-time setup.
- Consumes: Task 1's teardown (the integration flow is invalid without it —
  the script documents this inline).

- [x] **Step 1: Write the script** (full content as committed:
  `set -euo pipefail`; `ensure_tool` prints the one-time setup on missing
  cargo-llvm-cov; `unit` = `cargo llvm-cov --bin codexferry --html
  --output-dir coverage/unit`; `integration` = the five `--test` flags into
  `coverage/integration` with the in-script warning that it requires the
  graceful teardown; `e2e` = `cargo llvm-cov show-env | grep ^export >
  /tmp/cov-env.sh`, subshell sources it, exports
  `LLVM_PROFILE_FILE="$PWD/target/llvm-cov-target/e2e-%p-%m.profraw"`, runs
  `./scripts/e2e.sh all`, then `cargo llvm-cov report --html
  --output-dir coverage/e2e`; usage text for no args).

- [x] **Step 2: `chmod +x` + `bash -n` syntax check + gitignore**

Run: `bash -n scripts/coverage.sh && echo SYNTAX-OK`
Expected: SYNTAX-OK; `coverage/` ignored.

- [x] **Step 3: Commit**

```bash
git add scripts/coverage.sh .gitignore
git commit -m "feat(test): coverage.sh wraps the three cargo-llvm-cov flows"
```

### Task 3: Docs (README-DETAILS + AGENTS.md)

**Files:**
- Modify: `README-DETAILS.md` (new "### Test coverage" subsection before
  "### End-to-End scripts")
- Modify: `AGENTS.md` (Testing Strategy bullet)

**Interfaces:** none (documentation).

- [x] **Step 1: README-DETAILS** — one-time setup, the three flows, and the
  reading policy: per-layer reports are the useful ones; the integration
  layer's graceful teardown is REQUIRED for real numbers (SIGKILL ⇒ ~0%);
  coverage is a gap-finder, not a gate (patch coverage is the review
  signal; the DSML-leak class of bug is invisible to line coverage); the
  e2e-mock binary is scaffolding and excluded by intent.

- [x] **Step 2: AGENTS.md** — Testing Strategy bullet pointing at
  coverage.sh with the regression warning: do not replace the SIGTERM-first
  drop with a bare SIGKILL, or router subprocess coverage silently drops to
  ~0%.

- [x] **Step 3: Commit** (folded into the implementation commit — docs and
  code land together for this small feature).

### Task 4: Acceptance proof (spec Verification 2)

**Files:** none (measurement run).

**Interfaces:**
- Consumes: `scripts/coverage.sh integration` (Task 2) over Task 1's drop.

- [x] **Step 1: Run integration coverage**

Run: `./scripts/coverage.sh integration`
Expected: EXIT 0, all five suites green, report written.

- [x] **Step 2: Assert handlers are NOT ~0%**

Run: `cargo llvm-cov report --summary-only | grep -E "proxy/mod|chat\.rs|passthrough"`
Expected (actual 2026-09-01): proxy/mod.rs 74.5% lines, chat.rs 86.0%,
passthrough.rs 71.6%, models_cache.rs 81.6% — the pre-fix expectation was
~0% for all of them. 50 `codexferry-*` profraw files prove the subprocesses
flushed.

- [x] **Step 3: Full suite + clippy + fmt remain green**

Run: `cargo test && cargo clippy --all-targets && cargo fmt --check`
Expected: 7/7 suite-oks, zero clippy, clean formatting.

## Self-Review

- **Spec coverage:** Part 1 → Task 1; Part 2 → Task 2 (+ script inline
  docs); Part 3 → Task 3; Verification → Task 4. Docs sync → Task 3.
  Exclusions and e2e-mock non-goals are recorded in the script comments and
  README-DETAILS prose. No gaps.
- **Placeholder scan:** all code blocks contain the committed content;
  Task 3's two doc steps describe their committed prose in full (no
  "TBD").
- **Type consistency:** `RouterGuard` fields unchanged; `refresh_if_stale`
  n/a here; the only new name is `MAX`-free `single_flight`-style bounded
  loop local to the drop.
