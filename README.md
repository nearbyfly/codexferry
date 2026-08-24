# codexferry

> Local proxy that lets [Codex CLI](https://github.com/openai/codex) use Chat-Completions-only LLM providers (DeepSeek, Kimi, GLM, SiliconFlow, …) through a single Responses-API endpoint.

## Why codexferry

Codex CLI talks the OpenAI Responses API. Most Chinese and indie LLM providers only expose Chat Completions. codexferry is a tiny Rust daemon that sits between them: it exposes Responses, translates to Chat Completions per-provider, and exposes every upstream model as a `provider/alias` route you select with `codex -m`. Add a route to `cxf.toml` and it's available in the next Codex turn — no restart.

## Features

- **Lite** — single Rust binary, ~30 MB resident, **no system dependencies** (uses rustls, not OpenSSL).
- **Dynamic config** — edit `cxf.toml`, save; the daemon hot-reloads and the next codex turn picks up new routes automatically.
- **Generated config** — for offline / pinned-catalog setups: `codexferry gen-catalog` writes a static file; codex reads it with no `/v1/models` fetch.
- **One provider in Codex, many models** — Codex sees one provider (`codexferry`). Switch upstreams with `codex -m deepseek/flash` or `codex -m ark/glm-5.2`.
- **Mid-session model switch** — resume `codex` onto a different route and the full conversation history carries over.

## Install

```bash
cargo install --git https://github.com/nearbyfly/codexferry
# or, from a local checkout:
cargo build --release   # binary: target/release/codexferry
```

Requires Rust stable (≥1.75). No C deps.

## Quick start

```bash
# 1. Make a config
cp cxf.toml.example cxf.toml
$EDITOR cxf.toml           # add provider base URLs + API key env var names

# 2. Export keys
export DEEPSEEK_API_KEY=sk-...

# 3. Start
./target/release/codexferry    # listens on 127.0.0.1:8787

# 4. Configure Codex (~/.codex/config.toml) — see scripts/codex-config-*.toml.example
#    dynamic config (recommended) wires codex to fetch /v1/models on session start.
#    generated config pins a snapshot file; no /models fetch.

# 5. Use
codex -m deepseek/deepseek-v4-flash
```

Full field reference and every config knob live in [README-DETAILS.md](./README-DETAILS.md). Build, test, and doctor guidance live in [BUILD.md](./BUILD.md). Cadence + release flow in [RELEASE.md](./RELEASE.md).

## Two config modes

**Dynamic config** (recommended) — the daemon serves `GET /v1/models?client_version=…`. Codex CLI fetches it on session start when wired with `auth.command`. Routes you add to `cxf.toml` are visible to Codex in the next session. Choose this unless you have a reason not to.

**Generated config** — `codexferry gen-catalog` writes a pinned JSON file (e.g. `~/.codex/codexferry-catalog.json`); Codex reads it as a static `model_catalog_json` and never hits `/v1/models`. Pick this when you want zero network dependency for catalog at runtime, or when your Codex version predates the dynamic-mode support.

Both modes use the same `cxf.toml`. The difference is only the Codex side — see `scripts/codex-config-dynamic.toml.example` and `scripts/codex-config-static.toml.example`.

## Use

```bash
# Start an interactive session on the configured default model
codex

# Continue yesterday's session on a different route — full history carried over,
# cross-provider and cross-wire both work. Equivalent headless:
codex exec resume --last --skip-git-repo-check -m deepseek/deepseek-v4-flash "continue"
```

## See also

- [README-DETAILS.md](./README-DETAILS.md) — config field reference, env vars, endpoints, dynamic/generated runbook, doctor flags & bisection, systemd unit, troubleshooting.
- [BUILD.md](./BUILD.md) — `cargo test`, e2e, `doctor --live`.
- [RELEASE.md](./RELEASE.md) — when and how to cut a release (GitHub + Gitea).
- [ARCHITECTURE.md](./ARCHITECTURE.md) — module map, request/SSE flows, session design.

## License

MIT — see [LICENSE](./LICENSE).
