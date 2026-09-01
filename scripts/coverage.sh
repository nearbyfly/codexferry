#!/usr/bin/env bash
# Test-coverage flows for codexferry (coverage-infra spec
# docs/superpowers/specs/2026-09-01-test-coverage-infra-design.md §Design
# Part 2).
#
#   scripts/coverage.sh unit         unit tests (inside the bin crate)
#   scripts/coverage.sh integration  the five integration suites
#   scripts/coverage.sh e2e          deterministic e2e with instrumented
#                                    binaries, then a merged report
#
# Reports land in coverage/<mode>/index.html (gitignored). Coverage is a
# gap-finder, not a gate: no absolute-number threshold — the actionable
# signal is whether the lines a change touched got exercised.
set -euo pipefail
cd "$(dirname "$0")/.."

ensure_tool() {
  if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
    echo "cargo-llvm-cov is not installed. One-time setup:" >&2
    echo "  rustup component add llvm-tools-preview" >&2
    echo "  cargo install cargo-llvm-cov --locked" >&2
    exit 2
  fi
}

mode="${1:-}"
case "$mode" in
  unit)
    ensure_tool
    cargo llvm-cov clean --workspace
    cargo llvm-cov --bin codexferry --html --output-dir coverage/unit
    echo "HTML report: coverage/unit/index.html"
    ;;
  integration)
    ensure_tool
    # Valid ONLY with the graceful RouterGuard teardown (tests/common/mod.rs):
    # a bare SIGKILL would lose the router subprocess's LLVM counters and
    # report the handlers at ~0% (coverage-infra spec §Problem/Part 1).
    cargo llvm-cov clean --workspace
    cargo llvm-cov --test chat_conversion --test endpoints_metrics \
                   --test healing --test passthrough --test sessions \
                   --html --output-dir coverage/integration
    echo "HTML report: coverage/integration/index.html"
    ;;
  e2e)
    ensure_tool
    cargo llvm-cov clean --workspace
    # e2e-lib.sh already tears the router down with SIGTERM (graceful), so
    # an instrumented binary flushes its counters; the mock's counters are
    # lost (no signal handler) but the mock is scaffolding, not a target.
    # The subshell keeps the instrumentation env out of your normal builds.
    cargo llvm-cov show-env | grep -E '^export' >/tmp/cov-env.sh
    (
      # shellcheck disable=SC1091
      . /tmp/cov-env.sh
      export LLVM_PROFILE_FILE="$PWD/target/llvm-cov-target/e2e-%p-%m.profraw"
      ./scripts/e2e.sh all
    )
    cargo llvm-cov report --html --output-dir coverage/e2e
    echo "HTML report: coverage/e2e/index.html"
    ;;
  ""|-h|--help|help)
    cat <<USAGE
usage: scripts/coverage.sh [unit|integration|e2e]

  unit         unit tests (they live inside the bin crate)
  integration  chat_conversion + endpoints_metrics + healing + passthrough
               + sessions (spawn the real router; requires the graceful
               RouterGuard teardown so its counters flush)
  e2e          scripts/e2e.sh all with instrumented binaries, merged report

Reports land in coverage/<mode>/index.html (gitignored). Use
cargo llvm-cov --summary-only ... directly for a terminal-only view.
One-time setup:
  rustup component add llvm-tools-preview
  cargo install cargo-llvm-cov --locked
USAGE
    ;;
  *)
    echo "unknown mode: $mode (see scripts/coverage.sh -h)" >&2
    exit 1
    ;;
esac
