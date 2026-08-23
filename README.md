# codexferry

Local proxy daemon that lets [Codex CLI](https://github.com/openai/codex) (≥0.128)
use Chat-Completions-only LLM providers (DeepSeek, Kimi, GLM, SiliconFlow, …)
through a single Responses-API endpoint.

- **Protocol translation**: Converts between OpenAI Responses API and Chat Completions API (request + streaming SSE, token-by-token).
- **One provider in Codex, many models**: Codex is configured with a single provider (`codexferry`); every upstream model is exposed as a `provider/alias` model name. Switch models with `codex -m deepseek/flash` — no Codex reconfiguration.
- **Seamless model switching**: Multi-turn conversations keep their full history even when the new model is backed by a different upstream. From Codex's view it's just `codex -m`.
- **Native passthrough**: Providers that already speak Responses API are forwarded (byte-for-byte verbatim when healing is off; leaked DSML/think markup is healed event-granular when the `dsml_heal`/`think_tags` quirks fire).
- **Config hot-reload**: Edit TOML without restarting.

## Quick Start

### 1. Build

```bash
cargo build --release
# Binary: target/release/codexferry
```

Requirements: Rust stable (≥1.75), no system dependencies (uses rustls, not OpenSSL).

### 2. Configure

```bash
cp cxf.toml.example cxf.toml
# Edit cxf.toml - set your provider base URLs and API keys
```

Minimal config:

```toml
[server]
host = "127.0.0.1"
port = 8787

[providers.deepseek]
base_url = "https://api.deepseek.com/v1"
api_key_env = "DEEPSEEK_API_KEY"    # reads from environment variable
format = "chat"                      # "chat" or "responses"

[routes]
"deepseek/deepseek-v4-flash" = { model = "deepseek-chat" }
"deepseek/deepseek-v4-pro"   = { model = "deepseek-reasoner", context_window = 131072 }
```

### 3. Set API keys

```bash
export DEEPSEEK_API_KEY="sk-your-key-here"
```

### 4. Run

```bash
./target/release/codexferry
# → codexferry listening on 127.0.0.1:8787

# Debug mode: verbose router logs + request/response body tracing, saved to /tmp/router-debug.log
RUST_LOG=codexferry=debug CODEXFERRY_TRACE_BODY=1 ./target/release/codexferry 2>&1 | tee /tmp/router-debug.log
```

### 5. Configure Codex CLI

The router config (`cxf.toml`) and the Codex CLI config
(`~/.codex/config.toml`) are two ends of the same pipe — don't confuse them.
Ready-made examples for the Codex side live in `scripts/`:

- **`codex-config-dynamic.toml.example`** (recommended): command-auth wiring
  under which codex fetches the model catalog live from `GET /v1/models` on
  session start. Adding a route to `cxf.toml` needs no regeneration step;
  codex picks it up when its 300s models cache expires. The `/model` picker
  list is *merged* (codexferry routes + codex's bundled models) — that is a
  codex-side behavior no config can turn off for non-ChatGPT auth.
- **`codex-config-static.toml.example`**: pin a `gen-catalog`-generated file
  with `model_catalog_json`. Pure model list (nothing bundled leaks in) and
  independent of the `/models` endpoint, but the file is a snapshot —
  re-run `gen-catalog` after route changes or codex upgrades.

Both were verified end-to-end on codex 0.147.0. Copy one to
`~/.codex/config.toml` (merging with your own approval/sandbox/projects
settings) — the file headers explain the mode-specific keys.

That's the **only** provider Codex ever needs. The router aggregates every
upstream from `cxf.toml` and exposes them as `provider/alias` model names,
so switching models is just `codex -m <alias>` — Codex has no idea (and doesn't
care) that different models may be backed by different providers.

### 6. Use

```bash
codex -m deepseek/deepseek-v4-flash
codex -m deepseek/deepseek-v4-pro
# Mid-session, switch to a model backed by a different provider:
codex -m ark/glm-5.2
```

## Configuration Reference

### Full config example

```toml
[server]
host = "127.0.0.1"          # bind address
port = 8787                  # bind port

[providers.deepseek]
base_url = "https://api.deepseek.com/v1"  # must include API version path (/v1)
api_key_env = "DEEPSEEK_API_KEY"           # API key from env var
format = "chat"                             # "chat" or "responses"
timeout_ms = 120000                         # optional, default 120s
# extra_headers = { "X-Custom" = "value" } # optional static headers

[providers.ark]
base_url = "https://ark.cn-beijing.volces.com/api/v3"
api_key = "sk-direct-key"                   # or inline key (plaintext)
format = "chat"

[providers.openai]
base_url = "https://api.openai.com/v1"
api_key_file = "/path/to/keyfile"           # or read from file (trimmed)
format = "responses"                         # native passthrough, no conversion

[session]
ttl_hours = 168              # session retention, default 7 days
max_sessions = 256           # max cached sessions (LRU eviction)
max_memory_mb = 512          # memory budget (LRU eviction)

[routes]
# Route key format: "<provider_name>/<alias>"
# - provider_name must match a [providers.X] key
# - alias is your custom name (shown in Codex TUI)
# - model is the upstream's actual model name
"deepseek/deepseek-v4-pro"   = { model = "deepseek-reasoner", context_window = 131072 }
"deepseek/deepseek-v4-flash" = { model = "deepseek-chat" }
"ark/glm-5.2"                = { model = "glm-5.2" }
"openai/gpt-4o"              = { model = "gpt-4o", context_window = 128000 }
```

To Codex, a route key is just a model name (that's what you pass to
`codex -m`); the `provider/` prefix exists only so the router knows which
upstream serves it.

### Field reference

#### `[server]`

| Field | Default | Description |
|-------|---------|-------------|
| `host` | `127.0.0.1` | Bind address |
| `port` | `8787` | Bind port |

#### `[providers.<name>]`

| Field | Required | Description |
|-------|----------|-------------|
| `base_url` | yes | Upstream base URL **including API version path** (e.g. `/v1`). The proxy appends `/chat/completions` or `/responses`. |
| `format` | yes | `"chat"` (convert) or `"responses"` (passthrough) |
| `api_key` | one of three | Plaintext API key |
| `api_key_env` | one of three | Environment variable name containing the key |
| `api_key_file` | one of three | File path containing the key (read + trimmed) |
| `timeout_ms` | no | Upstream timeout in ms (default: 120000). For streaming requests this bounds the connect/response-header phase and the **inter-chunk idle time** (a stream silent for `timeout_ms` is failed with `response.failed`); it is NOT a cap on the stream's total duration, so healthy long generations are never cut short. For non-streaming requests it bounds the whole request. |
| `extra_headers` | no | Static headers to inject into upstream requests |

**API key resolution order**: `api_key` → `api_key_env` → `api_key_file`.

#### `[routes]`

| Field | Required | Description |
|-------|----------|-------------|
| `model` | yes | Upstream model name (what the provider actually accepts) |
| `context_window` | no | Context window for catalog generation (default: 1048576) |

**Route key rules:**
- Format: `<provider_name>/<alias>`, split on the first `/`.
- `provider_name` must match a `[providers.X]` entry.
- The route key is what you pass to `codex -m <key>`.
- Duplicate keys cause startup error.

#### `[session]` (all optional)

| Field | Default | Description |
|-------|---------|-------------|
| `ttl_hours` | 168 (7 days) | Idle session retention before eviction |
| `max_sessions` | 256 | Max cached sessions (LRU when exceeded) |
| `max_memory_mb` | 512 | Memory budget (LRU eviction when exceeded) |

### Reasoning effort

Codex's chosen effort level is passed through to chat upstreams verbatim as
`reasoning_effort` (responses-format upstreams receive the full `reasoning`
object, as before). Providers can strip or add chat-body fields:

```toml
[providers.foo]
format = "chat"
drop_params  = ["reasoning_effort"]   # upstream rejects unknown fields
extra_params = { top_k = 50 }         # fixed body additions
```

Provider quirks — the GLM thinking switch, missing-`[DONE]` tolerance, and
automatic healing of leaked DSML tool-call / `<think>` thinking markup on
chat-route responses — are on by default and can be disabled per quirk:

```toml
[quirks]
disabled = ["glm_thinking"]
```

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `CODEXFERRY_CONFIG` | `cxf.toml` | Path to config file (a legacy `./config.toml` still loads, with a warning) |
| `CODEXFERRY_TRACE_BODY` | (unset) | Set to `1` to log request/response bodies at debug level |
| `RUST_LOG` | `codexferry=info` | Log level filter (e.g. `debug`, `codexferry=debug`) |

## Endpoints

| Method | Path | Description |
|--------|------|-------------|
| POST | `/v1/responses` | Main entry — Responses API (SSE streaming) |
| POST | `/responses` | Same, without `/v1` prefix |
| GET | `/v1/models` | List all route keys as models (sorted); `?client_version=` selects the Codex ModelsResponse catalog shape |
| GET | `/models` | Same, without `/v1` prefix |
| GET | `/metrics` | Prometheus-format upstream metrics (always-on) |
| GET | `/healthz` | Health check — returns `"ok"` |

`/metrics` is always-on and serves Prometheus-format counters and histograms
for upstream requests — request counts by error class, token usage,
time-to-first-token and full-duration latency, and an in-flight gauge, labeled
per provider/route/model.

## Model Catalog: `/models` and `model_catalog_json`

codexferry serves a live Codex model catalog at `GET /v1/models`: when a client
sends the `client_version` query parameter, the daemon answers with the Codex
`ModelsResponse` catalog shape built from the hot-reloaded `cxf.toml`.

Whether **Codex CLI** calls that endpoint is decided by the provider's auth
shape (`codex-rs/models-manager/src/manager.rs`, `should_refresh_models`):

```rust
self.endpoint_client.uses_codex_backend().await || self.endpoint_client.has_command_auth()
```

`uses_codex_backend` requires ChatGPT-family auth; `has_command_auth` requires
the provider to declare an auth command (`[model_providers.X.auth]` with a
`command`). An `env_key` provider satisfies **neither** - codex never fetches
`/models` for it, and a `model_catalog_json` pin switches codex to the static
in-process catalog (`StaticModelsManager`) regardless of auth. That yields the
two supported modes, both with ready-made examples in `scripts/`.

### Dynamic mode (recommended): command auth, live fetch

```toml
# ~/.codex/config.toml - full file: scripts/codex-config-dynamic.toml.example
model = "deepseek/deepseek-v4-flash"
model_provider = "codexferry"

[model_providers.codexferry]
name = "codexferry"
base_url = "http://127.0.0.1:8787/v1"
wire_api = "responses"

[model_providers.codexferry.auth]
command = "echo"
args = ["dummy-token"]
```

The auth command's stdout (trimmed, non-empty) becomes the bearer token - the
router does not authenticate clients, so a dummy echo is enough. `auth` is
mutually exclusive with `env_key` / `experimental_bearer_token` /
`requires_openai_auth` in codex's config schema.

With this wiring codex fetches the catalog on session start (cache miss),
caches it in `~/.codex/models_cache.json` (TTL 300s, ETag revalidation), and
merges it with its bundled models. Verified on codex 0.147.0: `codex debug
models` lists the router routes alongside the bundled gpt models, and real
`codex exec` turns resolve their metadata from the fetched catalog.

Caveat: the `/model` picker list is a MERGE, not a replacement - non-ChatGPT
auth cannot suppress codex's bundled models (`apply_remote_models` requires a
ChatGPT account for replace semantics). For a pure route-only list, use
static mode.

### Static mode: generated catalog, pinned

```toml
# ~/.codex/config.toml - full file: scripts/codex-config-static.toml.example
model = "deepseek/deepseek-v4-flash"
model_provider = "codexferry"
model_catalog_json = "codexferry-catalog.json"   # relative paths resolve against ~/.codex/

[model_providers.codexferry]
name = "codexferry"
base_url = "http://127.0.0.1:8787/v1"
wire_api = "responses"
env_key = "CODEXFERRY_DUMMY"  # any non-empty value; export CODEXFERRY_DUMMY=dummy
```

Generate the pinned file with `gen-catalog` and regenerate it whenever you add
routes or upgrade Codex:

```bash
codexferry gen-catalog \
  --config cxf.toml \
  --out ~/.codex/codexferry-catalog.json
```

The trade-off: unlike the live endpoint, a generated file is a snapshot.
Adding a route to `cxf.toml` hot-reloads the daemon, but Codex keeps the old
model list until you re-run `gen-catalog` and restart Codex.

**If you end up with neither** (env_key wiring, no pin): requests still work -
routing, streaming and tool calls are unaffected, because the route name is
passed through to codexferry regardless. But Codex has no metadata for the
route, so it falls back to built-in defaults and warns:

```
Model metadata for `deepseek/deepseek-v4-pro` not found. Defaulting to fallback
metadata; this can degrade performance and cause issues.
```

In practice that means a generic context window and generic prompt/reasoning
settings instead of the ones your config declares (`context_window`, the
inherited `base_instructions`, the reasoning ladder) - usable for a quick test,
not what you want day to day.


### Clients that do call `/v1/models`

Clients that send `client_version` get the Codex `ModelsResponse` catalog,
built live from the current config with ETag revalidation — no restart needed
after a config edit. Clients that do **not** send `client_version` (e.g.
plain OpenAI-compatible list clients) get the Chat-Completions list shape
(`{"object":"list","data":[...]}`) at the same URL. Both shapes are supported
and tested; a codex wired with command auth (dynamic mode above) hits the
`client_version` shape on its own, no sniffer required.

### gen-catalog template search

**Template search** (for inheriting fields like `base_instructions`):

1. `--codex-models <path>` (explicit, if provided)
2. `codex debug models --bundled` (Codex CLI command)
3. `~/.codex/models.json`
4. `$XDG_DATA_HOME/codex/models.json`
5. `/usr/local/share/codex/models.json`
6. `/usr/share/codex/models.json`
7. `/Applications/Codex.app/Contents/Resources/models.json` (macOS)

If no template is found, entries are generated from scratch with the full
pinned field set described in the Catalog Generation Policy below — including
the codex-required structural fields and a neutral `base_instructions`
placeholder (codex ≥0.147 rejects any model carrying neither
`base_instructions` nor `model_messages.instructions_template`).

## Catalog Generation Policy (deny-by-default)

`gen-catalog` inherits only two fields from the bundled Codex template:
`base_instructions` and `model_messages` (the prompt fields that track the
installed Codex version). All other template fields — dialect switches
(`use_responses_lite`, `tool_mode`, `shell_type`, …), OpenAI-ecosystem fields
(`include_*`, `service_tiers`, `upgrade`, …), TUI decoration — are dropped,
and the dropped names are logged at generation time. Generated entries pin:
`use_responses_lite=false`, `shell_type="default"`, `prefer_websockets=false`,
`supports_parallel_tool_calls=true`, plus the codex-required structural fields
`priority=99`, `support_verbosity=false`,
`truncation_policy={mode:"tokens",limit:10000}`,
`experimental_supported_tools=[]`, and an always-emitted
`supported_reasoning_levels` ladder (codex ≥0.147's ModelInfo has no serde
default for these — a catalog missing them is rejected outright).

Background: the 2026-08-17 DSML leak — the template's `use_responses_lite=true`
made codex 0.147 deliver tools as `additional_tools` input items that
third-party upstreams cannot bind, so tool calls leaked as DSML text.
Deny-by-default makes future template fields inert instead of leaking onto the
wire.

## doctor: regression check after Codex upgrades

```bash
# Offline (default): regenerate the catalog and deep-compare with the
# installed one, detecting any drift
codexferry doctor --config cxf.toml

# Live: in-process mock upstream + temporary router + real `codex exec`,
# asserting the normalized wire shape and a full tool round-trip (offline, zero tokens)
codexferry doctor --live --config cxf.toml
```

Exit codes: 0 all pass; 1 a check failed; 2 environment unusable (e.g. codex
not installed).

### Codex upgrade runbook

```bash
cargo build --release
# If not pointing Codex at the router for live catalog, generate statically:
# ./target/release/codexferry gen-catalog --out ~/.codex/codexferry-catalog.json --config cxf.toml
./target/release/codexferry doctor --live --config cxf.toml
```

The "dropped N template field(s)" line in the generation log and doctor's INFO
lines show the template fields a new Codex version introduced; a live-probe red
light means Codex changed its wire dialect — fix the router's normalization
first, then re-run.

## Running as a systemd Service

```ini
[Unit]
Description=codexferry
After=network.target

[Service]
Type=simple
ExecStart=/path/to/codexferry
Environment=CODEXFERRY_CONFIG=/etc/codexferry/cxf.toml
Environment=DEEPSEEK_API_KEY=sk-your-key
KillSignal=SIGTERM
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

The proxy handles SIGTERM gracefully (stops accepting new connections, lets
in-flight requests complete).

## How It Works

### Chat format providers (`format = "chat"`)

```
Codex CLI                    codexferry                    Upstream
    │                             │                             │
    │── POST /v1/responses ──────▶│                             │
    │   (Responses API, SSE)      │── POST /chat/completions ─▶│
    │                             │   (Chat Completions, SSE)   │
    │◀── Responses SSE ───────────│◀── Chat SSE ───────────────│
    │   (token-by-token)          │   (converted)               │
```

The proxy:
1. Converts the Responses request to Chat Completions format.
2. Forwards to `${base_url}/chat/completions`.
3. Converts the Chat SSE stream to Responses SSE events in real time.
4. Generates a `resp_<uuid>` ID and stores the conversation context.

### Responses format providers (`format = "responses"`)

The proxy forwards the request to `${base_url}/responses` with the model name
replaced and authorization header injected. The SSE stream is passed through
(byte-for-byte verbatim when healing is off; leaked DSML/think markup is healed
event-granular when the `dsml_heal`/`think_tags` quirks fire). Session state is
captured from the `response.completed` event.

### Request normalization (Codex private dialects)

Codex occasionally delivers parts of a request in OpenAI-internal dialect
shapes that third-party upstreams never defined. The proxy normalizes them
before forwarding:

- **`additional_tools` hoisting** (both formats): Codex ≥0.147 can deliver its
  toolset as `additional_tools` input items (namespace-wrapped) instead of the
  top-level `tools` array. The proxy hoists every function-shaped tool into the
  top-level `tools` array (name-deduplicated) and strips the non-standard item.
- **Chat-path namespace flattening**: `namespace` tool entries (e.g. Codex's
  multi-agent tools) are flattened into their inner function tools with names
  encoded as `{namespace}-{name}` (e.g. `multi_agent_v1-spawn_agent`), which
  Chat upstreams can bind. The proxy builds a decode map from the request's
  tools and restores an independent `namespace` field on response tool calls,
  so Codex can dispatch them. Responses upstreams leave namespace entries
  verbatim.
- **Unknown-type visibility**: unknown input item types and tool types are
  never silently swallowed — they pass through or are dropped, and are logged
  with a process-lifetime counter (`unknown input item type(s) passed
  through/dropped: ...`, `tool type(s) not mappable to chat dropped: ...`), so a
  new Codex dialect shows up in the logs instead of degrading silently.

### Session state

When Codex sends `previous_response_id`, the proxy:
1. Looks up the stored conversation context.
2. Merges it with the new input.
3. Converts the full history (including reasoning items → `reasoning_content`),
   merging each turn's assistant text + tool calls into one message and
   synthesizing `call_<uuid>` ids when an upstream omitted tool-call ids.
4. Stores the new complete context under a new response ID.

Requests with `store: false` skip step 4 entirely: Codex in that mode replays
its full transcript inline on every turn and never sends
`previous_response_id`, so a stored snapshot would never be read back (this
also avoids the O(n²) memory growth of full-context snapshots).

This is what makes model switching seamless: you can start with DeepSeek and
continue with GLM mid-session, and the full history (including reasoning) is
preserved. From Codex's point of view this is just switching models — the
router silently keeps the conversation coherent across upstreams.

Known limitation: sessions captured by responses-format passthrough are stored
verbatim; if such an upstream emits a function call with an empty
`call_id`, replaying that session on a chat-format route sends
`tool_calls[].id: ""`, which strict providers reject for the remainder of the
session's TTL. Router-converted responses are not affected.

## Building & Testing

```bash
# Debug build
cargo build

# Release build
cargo build --release

# Run all tests (unit + integration)
cargo test

# Run only unit tests (this is a binary-only crate, so use --bin)
cargo test --bin codexferry

# Run only integration tests (spawns real binary + mock upstream)
cargo test --test integration

# See request/response bodies (debug)
CODEXFERRY_TRACE_BODY=1 RUST_LOG=codexferry=debug cargo run
```

## End-to-End Tests

Two manual scripts drive the **real Codex CLI** through the router (they are
not part of `cargo test`; they need `codex` on PATH, `python3`, and `curl`):

- `scripts/e2e.sh [basic|models|tools|multiturn|all]` — deterministic layer:
  a scripted mock upstream (`src/bin/e2e-mock.rs`) plus a temp router; the
  mock-layer scenarios assert against three sources (CLI output, the mock's
  recorded requests, the router's `/metrics`).
- `E2E_REAL_ROUTES="route1 route2" scripts/e2e-real.sh` — opt-in real-provider
  smoke (spends tokens; starts a dedicated router from your real config with
  a rewritten ephemeral port, so the resident instance is untouched).

On failure, all artifacts (CLI logs, mock request records, router log) are
left in the temp directory printed at the end of the run.

The `tools` scenario always runs Codex with
`--dangerously-bypass-approvals-and-sandbox` (via `E2E_CODEX_SANDBOX=bypass`):
bwrap needs user namespaces, which externally sandboxed containers cannot
create, so a normal sandbox makes even `echo` fail. `scripts/e2e-real.sh`
refuses to run whenever `E2E_CODEX_SANDBOX=bypass` is set, because real
providers must never run past the sandbox.

## Documentation

- [`ARCHITECTURE.md`](./ARCHITECTURE.md) — technical deep dive: request/SSE
  flows, protocol conversion details, session design, config validation.
- [`AGENTS.md`](./AGENTS.md) — conventions and gotchas for AI agents (and
  contributors) working on this repo.

## Limitations (MVP)

- **In-memory sessions only**: Restarting the proxy loses conversation history. (`previous_response_id` misses degrade gracefully.)
- **No retry/failover**: A single upstream timeout or error returns an error to the client.
- **No upstream model auto-discovery**: Routes are configured manually.
- **No WebSocket transport**: Codex's `supports_websockets` stays `false`.
- **No multi-user auth**: Designed for local personal use.
- **No Anthropic Messages upstream**: Architecture is extensible (`format` branch) but not implemented.
