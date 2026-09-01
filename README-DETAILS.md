# codexferry — Reference

This is the deep reference: every config knob, every endpoint, the full
dynamic/generated catalog runbooks, and the `doctor` failure-bisection
table. The user-facing quick start is in [README.md](./README.md).

## How it works

```
┌─── codexferry dynamic config ────────────────────────────────┐
│                                                              │
│ cxf.toml edit → hot-reload → next codex turn sees new routes │
│                                                              │
└──────────────────────────────────────────────────────────────┘

                 ┌──── codex session ────┐
                 │   one provider:       │
                 │   `codexferry`        │
                 └──────────┬────────────┘
                            │
                ┌───────────┴───────────┐
                ▼                       ▼
         ┌──────────────────┐   ┌──────────────────┐
         │  responses       │   │       chat       │
         │  upstream        │   │    upstream      │
         │  passthrough     │   │    + heal        │
         │  (verbatim)      │   │   Responses →    │
         │                  │   │    Chat          │
         └──────────────────┘   └──────────────────┘


  switch mid-session (full history carried):
    codex -m upstream-A-route
    codex resume --last -m upstream-B-route
```

Codex sees a single provider whose `base_url` points at the daemon; the
`provider/alias` prefix of each `-m` route key selects the upstream. The
two branches: responses-format upstreams are relayed byte-for-byte
verbatim (with opt-in event-granular healing when a quirk fires), while
chat-format upstreams get the full Responses → Chat request conversion
and Chat → Responses SSE conversion, plus the healing quirks below.

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
| `hide_bundled_models` | `false` | Emit `visibility:"hide"` overrides for Codex's bundled models on the live `/models` catalog (dynamic mode only; `gen-catalog` never affected). Requires the `codex` binary on the proxy's `PATH`; if discovery fails, hiding is silently disabled and a warning is logged. |

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
| `ttl_hours` | 168 (7 days) | Idle session retention before eviction. `0` expires every session immediately — the store is effectively disabled (warns at startup). |
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

Provider quirks are on by default and can be disabled per quirk. The full
list of registered quirks:

- `glm_thinking` — adds `thinking: {"type":"enabled"}` for GLM/Zhipu models
  (they suppress auto-thinking under heavy agent system prompts).
- `missing_done` — treats a stream that ends without `[DONE]` but with a
  `finish_reason` as complete, not failed.
- `dsml_heal` — repairs leaked DeepSeek DSML tool-call markers in chat
  replies (both streaming and one-shot paths restore leaked markers to
  structured `tool_calls` and strip them from visible text).
- `think_tags` — splits leaked `<think>…</think>` markers into the
  reasoning channel instead of rendering them as body text.
- `merge_fragmented` (responses-format path only) — collapses upstream
  Responses SSE runs of same-type `output_item.added` events (observed on
  MiniMax M3, 5–14 fragments per item) into a single Responses-conformant
  item. All deltas are rewritten to the first fragment's `item_id` /
  `output_index`; a synthesized `content_part.done` + `output_item.done`
  closes the run. Self-deactivates on healthy streams (run length = 1).
  Health-check passthrough is otherwise verbatim.

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

## Two config modes

codexferry serves a live Codex model catalog at `GET /v1/models`: when a client
sends the `client_version` query parameter, the daemon answers with the Codex
`ModelsResponse` catalog shape built from the hot-reloaded `cxf.toml`.

Whether **Codex CLI** calls that endpoint is decided by the provider's auth
shape (`codex-rs/models-manager/src/manager.rs`, `should_refresh_models`):

```rust
self.endpoint_client.uses_codex_backend().await || self.endpoint_client.has_command_auth()
```

`uses_codex_backend` is true when the active auth is a Codex-backed mode
(ChatGPT OAuth/tokens, headers, agent identity, or personal access token);
`has_command_auth` requires the provider to declare an auth command
(`[model_providers.X.auth]` with a `command`). An `env_key`-only provider
with no Codex-backed auth satisfies **neither** — codex does not fetch
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

The auth command's stdout (trimmed, non-empty) becomes the bearer token — the
router does not authenticate clients, so a dummy echo is enough. `auth` is
mutually exclusive with `env_key` / `experimental_bearer_token` /
`requires_openai_auth` in codex's config schema.

With this wiring codex fetches the catalog on session start (cache miss),
caches it in `~/.codex/models_cache.json` (TTL 300s, ETag revalidation), and
merges it with its bundled models. Verified on codex 0.147.0: `codex debug
models` lists the router routes alongside the bundled gpt models, and real
`codex exec` turns resolve their metadata from the fetched catalog.

On every successful hot-reload the daemon deletes that cache file, so codex's
next catalog read re-fetches `/v1/models` instead of waiting out the 300s TTL.
A background worker inside codex also re-fetches every 3 minutes on its own.
Catalog serving distinguishes config changes from time-based staleness: reads
arriving after a config edit WAIT for the refreshed catalog (the hot-reload
applier also refreshes proactively), so a removed route disappears from the
catalog immediately — important because codex persists the response into its
own 300s cache. Only the 60s time-based recheck serves stale-while-revalidate
(old body served while a background refresh runs).

Caveat: the `/model` picker list is a MERGE, not a replacement — non-ChatGPT
auth cannot suppress codex's bundled models (`apply_remote_models` requires a
ChatGPT account for replace semantics). For a pure route-only list, use
static mode — or hide the bundled entries as described next.

### Hiding Codex's bundled GPT models (dynamic mode)

In dynamic mode Codex merges its own bundled model catalog (the GPT family
compiled into the codex binary) underneath the models this proxy serves, so
those entries show up in the picker. Setting

```toml
[server]
hide_bundled_models = true
```

makes the live `/models` catalog additionally return `visibility: "hide"`
copies of every picker-visible bundled model (discovered via `codex debug models
--bundled`), which suppresses them from the picker while your routes stay
selectable. If the `codex` binary is not on the proxy's `PATH`, hiding is
silently disabled and the bundled models reappear — check the daemon log
for the warning. `gen-catalog` output is never affected.

### Adding and removing routes (dynamic mode)

Both operations start the same way — edit `cxf.toml` and save. The daemon
hot-reloads and deletes codex's `~/.codex/models_cache.json`. The updated
catalog reaches `/v1/models` via the single-flight background refresh
(stale-while-revalidate): the first read after the edit may return the
pre-change body, the next one is fresh.
What differs is what a *running* codex session does next, because codex pins
the `-m` selection for the lifetime of a session: the catalog governs what
can be *selected*, never what is *already selected*.

**Adding a route** — new sessions see it immediately, running ones do not:

```bash
# Immediate: a fresh codex process resolves the new route.
codex exec --skip-git-repo-check -m sensenova/glm-5.2 "task"

# Immediate, and the everyday workflow: continue the previous session on
# the new route with full history (cross-provider and cross-wire both
# fine). Without --last the picker lets you pick session + model.
codex resume --last -m sensenova/glm-5.2

# Immediate: a fresh TUI boot picks up the route in its /model list.
codex
```

A *running* TUI's `/model` picker will not list the new route: codex takes
the picker list as a one-time snapshot at TUI startup and never refreshes it
in-process. Switching within a running TUI only works for models that were
already listed when that TUI started.

**Removing a route** — sessions on other routes are unaffected; sessions on
the removed route fail on their next turn:

- The router answers 400 `no route for model` for the removed key (correct
  defense — the route no longer exists).
- To continue a conversation that was running on the removed route, resume
  it onto a live route (`-m` is mandatory — a bare resume re-uses the
  recorded model and keeps 400ing):

```bash
codex resume --last -m deepseek/deepseek-v4-flash "continue"
```

- An active TUI conversation on the removed route: switch via `/model` to a
  route that existed when the TUI started, or restart the TUI.
- Expect codex's "This session was recorded with model X but is resuming
  with Y" warning on model-changing resumes. For same-family routes behind
  this router it is safe to ignore — both sides' metadata comes from the
  same generated catalog. It matters only when the new route has a smaller
  context window or a weaker reasoning ladder (codex re-clamps the effort
  and may compact the history).

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

**If you end up with neither** (env_key wiring, no pin): requests still work —
routing, streaming and tool calls are unaffected, because the route name is
passed through to codexferry regardless. But Codex has no metadata for the
route, so it falls back to built-in defaults and warns:

```
Model metadata for `deepseek/deepseek-v4-pro` not found. Defaulting to fallback
metadata; this can degrade performance and cause issues.
```

In practice that means a generic context window and generic prompt/reasoning
settings instead of the ones your config declares (`context_window`, the
inherited `base_instructions`, the reasoning ladder) — usable for a quick test,
not what you want day to day.

### Clients that do call `/v1/models`

Clients that send `client_version` get the Codex `ModelsResponse` catalog,
built from the current config with ETag revalidation — no restart needed
after a config edit (reads after an edit wait for the refreshed catalog, so
they always reflect it; only the 60s time-based recheck serves a stale body
while a background refresh runs). Clients that do **not** send `client_version` (e.g.
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

## Catalog Generation Policy (static-mode generator only)

`gen-catalog` only matters in **static mode** (codex pinned to a generated file
via `model_catalog_json`). In dynamic mode the live `/v1/models` endpoint
serves the catalog at session start — no generation step, no regeneration on
route changes.

In static mode the generator inherits only two fields from the bundled Codex
template: `base_instructions` and `model_messages` (the prompt fields that
track the installed Codex version). All other template fields — dialect
switches (`use_responses_lite`, `tool_mode`, `shell_type`, …), OpenAI-ecosystem
fields (`include_*`, `service_tiers`, `upgrade`, …), TUI decoration — are
dropped, and the dropped names are logged at generation time. Generated
entries pin: `use_responses_lite=false`, `shell_type="default"`,
`prefer_websockets=false`, `supports_parallel_tool_calls=true`, plus the
codex-required structural fields `priority=99`, `support_verbosity=false`,
`truncation_policy={mode:"tokens",limit:10000}`,
`experimental_supported_tools=[]`, and an always-emitted
`supported_reasoning_levels` ladder (codex ≥0.147's ModelInfo has no serde
default for these — a catalog missing them is rejected outright).

Background: the 2026-08-17 DSML leak — the template's `use_responses_lite=true`
made codex 0.147 deliver tools as `additional_tools` input items that
third-party upstreams cannot bind, so tool calls leaked as DSML text.
Deny-by-default keeps future template fields inert instead of leaking onto the
wire.

## doctor: mode-aware regression checks

`codexferry doctor` reads `~/.codex/config.toml`, detects which catalog wiring
you use (pinned / dynamic / fallback), and runs the checks that apply to that
mode:

- **L1 — offline config checks** (always): the router config loads, routes
  exist, Codex is wired to this router, the wiring mode is classified
  (`detected mode`), and the Codex version status vs the last verified green
  is reported.
- **L2 — mode-specific checks** (always): dynamic mode smokes the live
  `/v1/models` endpoint and checks its catalog shape; pinned mode reconciles
  `model_catalog_json` against the router routes and checks the pinned field
  set; fallback mode WARNs that env-key-only wiring is degraded. The
  mode-independent `codex version age` check also notes when the installed
  codex is newer than anything codexferry has been verified against.
- **L3 — live probe** (when Codex is available): a real `codex exec`
  round-trip against an in-process mock upstream, with probe wiring that
  mirrors your detected mode, folded into the same report.

### What doctor checks per mode

- **Dynamic (recommended)** — `[model_providers.X.auth] command`, live
  `/v1/models` fetch. Doctor checks the endpoint smoke (`models endpoint
  reachable`) and catalog shape (`models endpoint shape`) against the running
  daemon. The daemon's version tripwire also surfaces client-version upgrades
  (`codex client X → Y detected`), so an upgrade is noticed even when nobody
  runs doctor.
- **Pinned** — `model_catalog_json` generated by `gen-catalog`. Doctor checks
  the pin parses, that every router route is covered (`pin covers router`),
  that every pin slug is a route (`pin matches router`), and the pin entry
  field set (`pin entry shape`, the fields Codex ≥0.147 requires). Re-run
  `codexferry gen-catalog` after route changes or Codex upgrades.
- **Fallback** — env-key only, no pin and no `auth.command`. Requests still
  work, but Codex falls back to generic metadata; doctor emits a
  `fallback wiring` WARN and recommends migrating to a pin (static mode,
  `gen-catalog`) or to `auth.command` (dynamic mode).

### Flags

- **Default** — L1 + L2, then the L3 live probe, composed into one report
  when Codex is available; L1 + L2 alone when it is not.
- **`--offline`** — L1 + L2 only (fast path, no codex required).
- **`--live`** — L3 probes only; overrides `--offline`.

Exit codes: 0 all pass; 1 a check failed; 2 environment unusable (codex not
installed or unrunnable — raised by the live path).

Live-involving runs (default or `--live`) persist the outcome to
`~/.local/state/codexferry/doctor.json`: a green run records `last_green` (the
normalized codex version) and `last_run`; a red run records `last_run` while
preserving `last_green`. `--offline` never writes the state file.

### Failure bisection

| Observed failure | Diagnosis | Fix layer | Remediation snippet |
|---|---|---|---|
| `pin unreadable` | static-mode hard error | user config | rerun `codexferry gen-catalog --config cxf.toml --out ~/.codex/codexferry-catalog.json` |
| `pin covers router` / `pin matches router` FAIL | stale pin OR stale router | gen-catalog or router config | rerun `codexferry gen-catalog`; check `cxf.toml` routes |
| `pin entry shape` FAIL | Codex upgrade added a required field | codexferry release | upgrade codexferry; stopgap: hand-add the field |
| `pin shadows live fetch` WARN | mixed static + dynamic wiring | user config | remove the pin or the `auth.command` — pick one mode |
| `models endpoint reachable` / `models endpoint shape` FAIL | daemon down or catalog output regression | router (`models_cache.rs`) | start the daemon; upgrade codexferry |
| live probe `tool round-trip executed` FAIL | Codex changed its wire dialect | `normalize.rs` / `convert/` | upgrade codexferry |
| dynamic probe: `live catalog fetched` FAIL | live `/v1/models` fetch silently failing | router (`models_cache.rs`) | upgrade codexferry |

### Codex upgrade runbook

```bash
cargo build --release
# Static mode: regenerate the pin first if routes or Codex changed.
# ./target/release/codexferry gen-catalog --config cxf.toml --out ~/.codex/codexferry-catalog.json
./target/release/codexferry doctor --config cxf.toml
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

## Build, test, end-to-end

```bash
# Debug build (faster, larger binaries)
cargo build

# Release build
cargo build --release

# See request/response bodies while running (debug only)
CODEXFERRY_TRACE_BODY=1 RUST_LOG=codexferry=debug cargo run
```

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

- `scripts/e2e.sh [basic|models|static|tools|multiturn|cross_format_switch|stale_catalog|doctor_dynamic|doctor_pinned|doctor_fallback|all]` — deterministic layer:
  scripted mock upstream (`src/bin/e2e-mock.rs`) + temp router. Each scenario
  asserts against three sources (CLI output, the mock's recorded requests,
  the router's `/metrics`).
- `E2E_REAL_ROUTES="route1 route2" scripts/e2e-real.sh` — opt-in real-provider
  smoke (spends tokens; starts a dedicated router from your real config with
  a rewritten ephemeral port, so the resident instance is untouched). Needs
  `E2E_REAL_CONFIG=path/to/cxf.toml` if your config lives outside the default
  `~/.config/codexferry/cxf.toml`. Pick the codex-side wiring with
  `E2E_REAL_MODE=dynamic|static` (default `dynamic`):

  ```bash
  # Live /v1/models + auth.command (default; asserts every tested route
  # arrived through the /models fetch).
  E2E_REAL_ROUTES="deepseek/... sensenova/..." E2E_REAL_CONFIG=cxf.toml \
    scripts/e2e-real.sh

  # env_key + model_catalog_json pin (gen-catalog from E2E_REAL_CONFIG;
  # asserts routes work without ever fetching /v1/models). Run this mode
  # separately to validate the static-mode example wiring against real
  # upstreams - it costs its own set of tokens.
  E2E_REAL_MODE=static \
    E2E_REAL_ROUTES="deepseek/... sensenova/..." E2E_REAL_CONFIG=cxf.toml \
    scripts/e2e-real.sh
  ```

On failure, all artifacts (CLI logs, mock request records, router log) are
left in the temp directory printed at the end of the run.

The `tools` scenario always runs Codex with
`--dangerously-bypass-approvals-and-sandbox` (via `E2E_CODEX_SANDBOX=bypass`):
bwrap needs user namespaces, which externally sandboxed containers cannot
create, so a normal sandbox makes even `echo` fail. `scripts/e2e-real.sh`
refuses to run whenever `E2E_CODEX_SANDBOX=bypass` is set — real providers
must never run past the sandbox.

## Limitations (MVP)

- **In-memory sessions only**: Restarting the proxy loses conversation history. (`previous_response_id` misses degrade gracefully.)
- **No retry/failover**: A single upstream timeout or error returns an error to the client.
- **No upstream model auto-discovery**: Routes are configured manually.
- **No WebSocket transport**: Codex's `supports_websockets` stays `false`.
- **No multi-user auth**: Designed for local personal use.
- **No Anthropic Messages upstream**: Architecture is extensible (`format` branch) but not implemented.
