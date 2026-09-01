# codexferry

> Local proxy that lets [Codex CLI](https://github.com/openai/codex) use Chat-Completions-only LLM providers (DeepSeek, Kimi, GLM, SiliconFlow, …) through a single Responses-API endpoint.

## Why codexferry

Native codex takes one provider per session and only speaks the Responses API. Most open-source model providers (SiliconFlow, Ollama, etc.) only expose Chat Completions — `codex -m` can't reach them at all, and switching providers mid-session means reconfiguring codex.

codexferry is one provider (`codexferry`) backed by an internal router. `cxf.toml` lists every upstream; `codex -m provider/alias` picks one, `codex resume --last -m …` switches mid-session. The router translates Responses ↔ Chat per route, so you can use any upstream behind the same codex endpoint — providers that already speak Responses pass through verbatim, Chat-only ones get the conversion on the fly.

```
                 ┌──── codex session ────┐
                 │   one provider:       │
                 │   `codexferry`        │
                 └──────────┬────────────┘
                            │
                ┌───────────┴───────────┐
                ▼                       ▼
         ┌──────────────┐       ┌──────────────┐
         │  responses   │       │     chat     │
         │  upstream,   │       │  upstream,   │
         │  passthrough │       │  Responses ↔ │
         │  (verbatim)  │       │  Chat + heal │
         └──────────────┘       └──────────────┘

   switch mid-session (full history carried):
     codex -m upstream-A-route
     codex resume --last -m upstream-B-route
```

## Features

- **Lite** — single Rust binary, ~30 MB resident, **no system dependencies** (uses rustls, not OpenSSL).
- **Dynamic config** — edit `cxf.toml`, save; the daemon hot-reloads and the next codex turn picks up new routes automatically. Offline setups can pin a generated catalog instead.
- **One provider in Codex, many models** — Codex sees one provider (`codexferry`). Switch upstreams with `codex -m deepseek/flash` or `codex -m ark/glm-5.2`; mid-session switches carry the full conversation history.
- **Upstream healing quirks** — opt-out repairs for broken upstreams (leaked DSML tool-call markers, `<think>` tags, fragmented Responses items); healthy streams pass through untouched. List and kill switch in [README-DETAILS.md](./README-DETAILS.md).

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

## Two config modes

**Dynamic config** (recommended) — the daemon serves `GET /v1/models?client_version=…`; Codex fetches it on session start when wired with `auth.command`. Routes added to `cxf.toml` are visible to Codex in the next session.

**Generated config** — `codexferry gen-catalog` writes a pinned JSON catalog; Codex reads it statically and never hits `/v1/models`. For zero network dependency at runtime, or Codex versions predating dynamic-mode support.

Both modes use the same `cxf.toml`; only the Codex side differs. Wiring examples, runbooks, and bundled-model hiding live in [README-DETAILS.md](./README-DETAILS.md).

## Use

```bash
# Start an interactive session on the configured default model
codex

# Continue yesterday's session on a different route — full history carried over,
# cross-provider and cross-wire both work. Equivalent headless:
codex exec resume --last --skip-git-repo-check -m deepseek/deepseek-v4-flash "continue"
```

## See also

- [README-DETAILS.md](./README-DETAILS.md) — config field reference, env vars, endpoints, dynamic/generated runbook, doctor flags & bisection, systemd unit, build/test/e2e commands, troubleshooting.
- [RELEASE.md](./RELEASE.md) — when and how to cut a release (GitHub + Gitea).
- [ARCHITECTURE.md](./ARCHITECTURE.md) — module map, request/SSE flows, session design.

## License

MIT — see [LICENSE](./LICENSE).
