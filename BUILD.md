# Building & Testing

Build, test, and end-to-end verification commands. See [README.md](./README.md)
for installation and configuration; see [ARCHITECTURE.md](./ARCHITECTURE.md)
for the internals that the tests cover.

## Build

```bash
# Debug build (faster, larger binaries)
cargo build

# Release build
cargo build --release

# See request/response bodies while running (debug only)
CODEXFERRY_TRACE_BODY=1 RUST_LOG=codexferry=debug cargo run
```

## Tests

`cargo test` runs the unit + integration suites. Two manual scripts drive the
**real Codex CLI** through the router and are not part of `cargo test` (they
need `codex` on PATH, `python3`, and `curl`).

```bash
# All tests (unit + integration)
cargo test

# Only unit tests (this is a binary-only crate, so use --bin)
cargo test --bin codexferry

# Only integration tests (spawn the real binary + mock upstream)
cargo test --test integration

# Targeted: only the SSE parser (most fixture-heavy module)
cargo test upstream

# Doctor subcommand: codex ↔ router contract check
cargo test doctor

# --live additionally drives the installed Codex CLI through an in-process
# router + mock upstream; needs `codex` on PATH. Zero tokens (mock upstream).
cargo run -- doctor --live --config cxf.toml
```

### End-to-End scripts

- `scripts/e2e.sh [basic|models|tools|multiturn|all]` — deterministic layer:
  scripted mock upstream (`src/bin/e2e-mock.rs`) + temp router. Each scenario
  asserts against three sources (CLI output, the mock's recorded requests,
  the router's `/metrics`).
- `E2E_REAL_ROUTES="route1 route2" scripts/e2e-real.sh` — opt-in real-provider
  smoke (spends tokens; starts a dedicated router from your real config with
  a rewritten ephemeral port, so the resident instance is untouched). Needs
  `E2E_REAL_CONFIG=path/to/cxf.toml` if your config lives outside the default
  `~/.config/codexferry/cxf.toml`.

On failure, all artifacts (CLI logs, mock request records, router log) are
left in the temp directory printed at the end of the run.

The `tools` scenario always runs Codex with
`--dangerously-bypass-approvals-and-sandbox` (via `E2E_CODEX_SANDBOX=bypass`):
bwrap needs user namespaces, which externally sandboxed containers cannot
create, so a normal sandbox makes even `echo` fail. `scripts/e2e-real.sh`
refuses to run whenever `E2E_CODEX_SANDBOX=bypass` is set — real providers
must never run past the sandbox.