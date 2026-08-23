# Architecture

Technical analysis of `codexferry` — a local proxy daemon that aggregates
multiple LLM providers under a unified `provider/model` namespace and translates
between OpenAI Responses and Chat Completions APIs.

## 1. Problem Statement

Codex CLI (≥0.128) custom providers only accept the Responses API
(`wire_api = "responses"`), but most Chinese LLM upstreams (DeepSeek, Kimi,
GLM, SiliconFlow) only expose Chat Completions. Additionally, Codex TUI's model
selector binds to a single provider — you cannot switch providers mid-session.

`codexferry` solves both problems:
1. **Protocol translation**: Responses ↔ Chat Completions (request + streaming SSE).
2. **Model aggregation**: All providers' models are unified under `provider/alias`
   names. Codex configures one provider pointing at the proxy and switches models
   via `codex -m deepseek/flash` or `codex -m ark/glm-5.2`.

## 2. Tech Stack

| Dependency | Version | Purpose |
|-----------|---------|---------|
| **axum** | 0.8 | HTTP server, routing, SSE response |
| **tokio** | 1 (full) | Async runtime, signals, timers, mpsc channels |
| **reqwest** | 0.12 (rustls-tls) | HTTP client to upstreams, streaming body |
| **serde / serde_json** | 1 / 1 | (De)serialization of JSON request/response bodies |
| **toml** | 0.8 | Configuration file parsing |
| **notify** | 7 | Filesystem watching for config hot-reload |
| **clap** | 4 (derive) | CLI argument parsing (`gen-catalog` subcommand) |
| **tracing / tracing-subscriber** | 0.1 / 0.3 | Structured logging with env-filter |
| **thiserror** | 2 | Ergonomic error enums for config validation |
| **anyhow** | 1 | Application-level error propagation |
| **bytes** | 1 | Zero-copy byte buffers for SSE stream parsing |
| **futures-util** | 0.3 | `StreamExt`, `unfold` for SSE stream transformation |
| **uuid** | 1 (v4) | Response ID generation (`resp_<uuid_v4_simple>`) |
| **tokio-stream** | 0.1 | `ReceiverStream` wrapper for mpsc → Stream |
| **tempfile** (dev) | 3 | Temp dirs/files in tests |

**Rust edition:** 2021
**Toolchain:** stable (tested on 1.97.1)

### Design choice: rustls over native-tls

`reqwest` is configured with `default-features = false, features = ["rustls-tls"]`
to avoid linking OpenSSL. This simplifies cross-compilation and static builds.

### Design choice: hand-written SSE parser

The SSE parser (`parse_sse_stream` in `upstream.rs`) is written from scratch
using `futures_util::stream::unfold` over a byte buffer. No external SSE crate
is used. This gives precise control over:
- Multi-byte UTF-8 characters split across chunk boundaries
- CRLF (`\r\n\r\n`) vs LF (`\n\n`) event delimiters
- Comment/keepalive lines
- Multi-line `data:` fields within a single event

## 3. High-Level Architecture

```
┌─────────────────────────────────────────────────────────┐
│                    Codex CLI (TUI)                       │
│  Single provider: base_url=http://127.0.0.1:8787        │
│  wire_api = "responses"                                 │
└──────────────────────┬──────────────────────────────────┘
                       │ POST /v1/responses (SSE)
                       ▼
┌─────────────────────────────────────────────────────────┐
│                  codexferry (axum)                     │
│                                                          │
│  1. Parse model from body → lookup route                │
│  2. Check previous_response_id → merge session history  │
│  3. Branch on provider format:                          │
│     ├─ chat:      convert req → Chat Completions        │
│     │             convert SSE ← Chat SSE (逐 token)      │
│     └─ responses: passthrough (healed when quirks on)   │
│  4. Generate resp_<uuid>, store session, return ID      │
│                                                          │
│  Config: SharedConfig (Arc<RwLock<ValidatedConfig>>)    │
│  Sessions: SessionStore (Arc<RwLock<SessionState>>)     │
│  HTTP client: reqwest::Client (pooled, 90s idle)        │
└──────────┬──────────────────┬───────────────────────────┘
           │                  │
     ┌─────┴─────┐     ┌─────┴─────┐
     ▼           ▼     ▼           ▼
 [deepseek]   [ark]  [kimi]    [openai]
 format=chat  chat   chat      responses
```

## 4. Request Flow (Chat Format)

```
Codex CLI
  │ POST /v1/responses
  │ {model: "deepseek/flash", input: [...], stream: true}
  ▼
handle_responses()
  │ 1. Deserialize body → ResponsesRequest
  │ 2. Lookup route: config.routes["deepseek/flash"]
  │ 3. If previous_response_id → SessionStore.get() → history items
  │ 4. resolve_api_key(provider) → Bearer token
  ▼
handle_chat_format()
  │ 5. to_chat_request(req, history, upstream_model)
  │    - Convert history items (Responses → Chat messages)
  │    - Merge instructions → system message
  │    - Convert new input items
  │    - Convert tools (function, custom/freeform → function)
  │    - Map params: max_output_tokens→max_tokens, passthrough fields
  │ 6. POST to {base_url}/chat/completions with stream=true
  ▼
StreamConverter (stateful, per-request)
  │ 7. For each upstream SSE chunk:
  │    - First chunk: emit created, in_progress, output_item.added, content_part.added
  │    - Content delta → response.output_text.delta
  │    - Reasoning delta → response.reasoning_summary_text.delta
  │    - Tool call delta → aggregate by index, emit function_call_status_changed (first only)
  │                      → response.function_call_arguments.delta
  │    - Finish chunk → emit content_part.done, output_item.done, response.completed
  │ 8. On upstream error/early end → emit error + response.failed
  │ 9. Store session: history + input + accumulated output items
  ▼
Codex CLI receives Responses SSE stream
```

## 5. Protocol Conversion Details

### 5.1 Request: Responses → Chat (`convert/request.rs`)

| Responses | Chat Completions | Notes |
|-----------|-----------------|-------|
| `input` (string) | `messages: [{role: "user", content: "..."}]` | |
| `input` (array) | `messages: [...]` | Each item converted by type |
| `instructions` / `system` | `messages[0]: {role: "system"}` | Prepended if not already system |
| `message` item | Chat message | role: developer/system→system, assistant→assistant, else→user |
| `function_call` item | assistant message with `tool_calls` | History replay; namespaced calls replay with encoded `{ns}-{name}` (spec §7) |
| `function_call_output` item | `{role: "tool", tool_call_id, content}` | |
| `reasoning` item | `reasoning_content` on next assistant msg | Spec §7.4 round-trip |
| `tools[type=function]` | `{type: "function", function: {name, description, parameters}}` | |
| `tools[type=custom/freeform]` | Same shape, parameters = `{input: string}` | Codex apply_patch |
| `tools[type=namespace]` | inner function tools, names encoded `{ns}-{name}` | Chat flatten + decode map (spec §7) |
| `max_output_tokens` | `max_tokens` | Renamed |
| `temperature`, `top_p`, `stop` | passthrough | |
| `presence_penalty`, `frequency_penalty` | passthrough (from `extra`) | |
| `seed`, `user`, `tool_choice` | passthrough (from `extra`) | |
| `reasoning.effort` | `reasoning_effort` | Verbatim passthrough (spec §5) |
| `store`, `metadata`, `include`, `text`, `parallel_tool_calls` | **dropped** | Responses-only fields |

**Item merge** (`push_item_messages`, one loop for BOTH sources — session
history and the `input` items array, which Codex fills with its full
transcript when running store=false):

- Consecutive assistant `message` + `function_call` items of the same turn
  merge into ONE assistant message carrying both `content` and `tool_calls`
  (Chat Completions requires this; separate messages produce a dangling
  assistant message sequence that strict upstreams reject).
- A `function_call` with no preceding assistant message starts its own
  assistant message and absorbs following `function_call` items.
- Standalone `reasoning` items buffer their summary for the NEXT assistant
  message; an EMPTY summary neither shadows the message item's own
  `reasoning_content` nor clears an earlier buffered summary.

**Known limitation (unfixed)**: `function_call` items whose `call_id` is
empty/missing — which non-conforming responses-format upstreams can store via
the passthrough path — replay with `tool_calls[].id: ""` when the session is
later routed to a chat-format provider (the SessionStore is shared across
routes). Strict Chat upstreams reject the whole request. Fixing this requires
paired synthesis: a `call_<uuid>` assigned to the empty-id `function_call`
must also rewrite the matching `function_call_output`'s `tool_call_id`.
Router-generated items are immune (ids are synthesized at ingestion).

### Reasoning effort & quirks (chat format)

- `reasoning.effort` is forwarded verbatim as a top-level `reasoning_effort`
  string — deliberately unvalidated; the accepted set is the upstream's
  business. Strip it per provider with `drop_params`.
- Quirk `glm_thinking` ([quirks] switch): GLM/Zhipu models get
  `thinking: {"type":"enabled"}` so reasoning_content is emitted under agent
  system prompts. The switch is gated on the **upstream model only** (not the
  route alias) — a deliberate deviation from codex-relay (see spec §2).
- Quirk `missing_done` ([quirks] switch): a stream ending without `[DONE]`
  but with `finish_reason` completes (with a warn); without either, the turn
  is truncated and fails via `response.failed` — never persisted, never
  authorizing tool execution.
- Providers may declare `extra_params` (merged into the chat body, wins on
  collision) and `drop_params` (stripped), validated against touching
  router-managed fields.

### 5.2 Response: Chat → Responses (`convert/response.rs`)

**Non-streaming** (`chat_response_to_items`):
- `message.reasoning_content` → `reasoning` item (`summary_text`)
- `message.content` → `message` item (`output_text`)
- `message.tool_calls` → `function_call` items (names in the request's `NamespaceToolMap`
  decode back to a `function_call` with an independent `namespace` field Codex dispatches
  on, spec §7; unmapped names stay flat)

**Streaming** (`StreamConverter::on_chunk` / `finish`) — wire format matched to
the real OpenAI Responses API (reference: codex-relay):

```
response.created   {type, response: {id, status: "in_progress", model}}
─── first delta of each item type (lazy item creation) ───
response.output_item.added  {output_index, item: {type: "message"|"reasoning", id: msg_/rs_<uuid>, ...}}
─── per upstream chunk ───
response.output_text.delta             {item_id, output_index, delta}
response.reasoning_summary_text.delta  {item_id, output_index, summary_index, delta}
(tool-call deltas: accumulated silently, no events)
─── stream end: finish(), DEFERRED past the finish_reason chunk ───
response.output_item.done               {output_index, item}        (message/reasoning, in output_index order)
response.output_item.added              {output_index, item: fc}    (per tool call, index order)
response.function_call_arguments.delta  {item_id, delta}            (full accumulated args, one shot)
response.output_item.done               {output_index, item: fc}
response.completed  {type, response: {id, status: "completed", model, output: [...], usage}}
```

Not emitted: `response.in_progress`, `content_part.added/done`,
`function_call_status_changed` (Codex CLI does not require them). Session
items are persisted in canonical turn order (reasoning before message)
regardless of delta arrival order. Every event's data carries
`"type": "response.<event>"`.

Error mid-stream: `error` + `response.failed` (both with `type` + `response`
wrapper).

**Response healing (quirks `dsml_heal` + `think_tags`)**: content deltas pass
through DSML isolation first, then the think filter — a DSML parameter value
may legitimately contain `<think>` text as part of a tool argument. Native
`reasoning_content` is never filtered; think-healed reasoning is appended after
it. At stream end, `finish()` flushes both filters BEFORE the finish sequence:
residual text/reasoning emits as ordinary deltas, and healed DSML calls join
the tool-call accumulator as `call_dsml_<uuid>` so they emit through the
regular `function_call` sequence. Both quirks are default-on class-B heals
(detection-triggered, no-ops on healthy responses); the `[quirks] disabled`
list is the kill switch, and each fired quirk logs exactly one `warn!` —
telemetry that doubles as the removal signal once the upstream fixes the bug.

### 5.3 SSE Parsing Rules (`upstream.rs`)

- Buffer raw bytes; decode at complete event boundaries only (UTF-8 safe).
- Only `data:` lines are processed; `:` comments, `event:`, `id:`, `retry:` ignored.
- Multiple `data:` lines in one event joined with `\n`.
- `data: [DONE]` → stream end sentinel.
- Supports both `\n\n` and `\r\n\r\n` delimiters.

## 6. Session State Management (`session.rs`)

```
SessionStore (Arc<RwLock<SessionState>>)
  └─ HashMap<String, SessionEntry>
       └─ SessionEntry { items: Vec<Value>, last_used_at: SystemTime }
```

- **Storage format**: Responses-format items (format-agnostic, works for both chat and responses upstreams).
- **Full-context snapshots**: Each `response_id` stores the complete conversation (O(n²) total, bounded by TTL + LRU).
- **Eviction**: Lazy on `get()`/`save()` (TTL expiry) + background hourly cleanup task.
- **LRU**: By `last_used_at`; `get()` promotes recency.
- **Memory limit**: `max_memory_mb` — oversized single sessions are rejected (warning logged).
- **Response IDs**: `resp_<uuid_v4_simple>` (proxy-generated for chat format; upstream ID for responses passthrough).

### Cross-provider flow example

```
Turn 1: codex -m deepseek/flash
  → Chat conversion → DeepSeek → Chat SSE → Responses SSE
  → Store: resp_001 = [user, assistant, reasoning]
  → Return: resp_001

Turn 2: codex -m ark/glm-5.2  (switch provider!)
  → Lookup resp_001 → merge history
  → reasoning → reasoning_content (spec §7.4)
  → Chat conversion → Ark → Chat SSE → Responses SSE
  → Store: resp_002 = [user, assistant, reasoning, new_user, new_assistant]
  → Return: resp_002
```

## 7. Configuration & Hot Reload (`config.rs`)

- **TOML format**: `[server]`, `[providers.X]`, `[routes]`, `[session]`.
- **Validation** (at startup + on reload):
  - Route keys must contain `/` (format: `provider/alias`).
  - Route key prefix must match a `[providers.X]` entry.
  - No duplicate route keys.
  - Each provider must have an API key (`api_key` / `api_key_env` / `api_key_file`).
  - `format` must be `chat` or `responses`.
- **Hot reload**: `notify` watcher → `try_write()` (non-blocking, skips if lock busy).
- **API key resolution order**: `api_key` (plaintext) → `api_key_env` (env var) → `api_key_file` (file, trimmed).

## 8. Endpoints

| Method | Path | Description |
|--------|------|-------------|
| POST | `/v1/responses`, `/responses` | Main entry (Responses API, SSE) |
| GET | `/v1/models`, `/models` | Dual-shape: chat list (no param) or Codex ModelsResponse |
| GET | `/metrics` | Prometheus-format upstream metrics (always-on) |
| GET | `/healthz` | Health check (`"ok"`) |
| * | fallback | 404 Responses-shaped error |

## 9. Error Handling

- **Startup config errors**: hard fail with specific message.
- **Unknown model**: 400 `invalid_request_error`.
- **Upstream non-2xx**: passthrough status + upstream body as error message.
- **Upstream connect/transport timeout** (incl. streaming header phase): 502 (reqwest timeout).
- **Streaming idle timeout**: no upstream chunk within `timeout_ms` → the
  proxy fails the stream (`error` + `response.failed` events; passthrough
  gets a synthesized terminal `response.failed`), records `timeout` in
  metrics, and never persists a session. Healthy streams are NOT
  total-duration-capped: `timeout_ms` bounds only the header phase and the
  inter-chunk idle time (issue #14).
- **Stream mid-error**: `error` + `response.failed` SSE events.
- **Session cache miss**: warning log, graceful degradation (new input only).
- All errors use Responses error shape: `{"error": {"type": "...", "message": "..."}}`.

## 10. Graceful Shutdown

- Listens for SIGINT and SIGTERM (Unix).
- `axum::serve().with_graceful_shutdown()` stops accepting new connections.
- In-flight streaming tasks continue until upstream completes or client disconnects.

## 11. Project Structure

```
codexferry/
├── Cargo.toml
├── cxf.toml.example         # Router config template (real cxf.toml is gitignored)
├── AGENTS.md               # AI agent notes
├── ARCHITECTURE.md         # This file
├── README.md               # User guide
├── scripts/
│   ├── e2e-lib.sh           # E2E shared helpers: sandbox selection + run_codex (215)
│   ├── e2e.sh               # deterministic E2E layer: basic/models/tools/multiturn (199)
│   ├── e2e-real.sh          # opt-in real-provider smoke; refuses sandbox bypass (97)
│   ├── codex-config-dynamic.toml.example  # Codex CLI side: live /models catalog via auth.command (47)
│   └── codex-config-static.toml.example   # Codex CLI side: pinned gen-catalog file (34)
├── docs/superpowers/
│   └── specs/              # Migration spec (the original design spec and plan
│                           #   live in the retained codex-router-rs repo)
├── src/
│   ├── main.rs             # CLI entry (clap) (171 lines)
│   ├── bin/
│   │   └── e2e-mock.rs     # scripted mock upstream for the E2E scripts (354)
│   ├── config.rs           # TOML types + validation + hot reload (954)
│   ├── doctor.rs           # doctor subcommand: offline drift check + report (315)
│   ├── doctor_live.rs      # doctor --live: mock upstream + wire-shape probe (808)
│   ├── proxy/
│   │   ├── mod.rs             # axum routing + dispatch (660)
│   │   ├── chat.rs            # chat-format handler (515)
│   │   ├── passthrough.rs     # responses-format relay (472)
│   │   ├── upstream.rs        # send_upstream + error-class dedup helpers (150)
│   │   ├── capture.rs         # session/usage capture (164)
│   │   │   └── tests.rs       # capture unit tests (178)
│   │   ├── tests.rs           # unit tests (227)
│   │   └── metrics_route_tests.rs  # metrics route unit tests (42)
│   ├── quirks.rs           # quirk names + GLM matcher (63)
│   ├── heal/
│   │   ├── mod.rs           # HealGates + facade re-exports (63)
│   │   ├── think.rs         # think-tag healing (192)
│   │   ├── dsml.rs          # DSML tool-call healing (677)
│   │   ├── responses.rs     # Responses passthrough healer (611)
│   │   └── (tests: heal/{think,dsml,responses_healer}_tests.rs)
│   ├── session.rs          # SessionStore (340)
│   ├── upstream.rs         # SSE parser + key resolution (623)
│   ├── catalog.rs          # gen-catalog (861)
│   ├── logging.rs          # tracing init (36)
│   ├── metrics.rs          # Prometheus metrics registry, label types, error classification, recording methods (369)
│   ├── normalize.rs        # boundary normalization (hoist, namespace flatten + chat-name encode/decode map, unknown-type visibility) (452)
│   │   └── (tests: normalize/tests.rs)
│   ├── wire/
│   │   ├── mod.rs          # wire types module (28)
│   │   ├── responses.rs    # Responses API types (132)
│   │   └── chat.rs         # Chat Completions types (372)
│   └── convert/
│       ├── mod.rs          # conversion module (23)
│       ├── request.rs      # Responses → Chat, incl. namespace tool encode on replay (613)
│       │   └── (tests: convert/request/tests.rs)
│       ├── response.rs     # Chat → Responses, incl. namespace decode (881)
│       │   ├── (tests: convert/response/stream_tests.rs)
│       │   └── (tests: convert/response/tests.rs)
└── tests/
    ├── common/mod.rs         # shared harness (~1,118)
    ├── chat_conversion.rs    # chat-path conversion tests (691)
    ├── passthrough.rs        # responses-format relay tests (438)
    ├── healing.rs            # dsml/think leak healing tests (352)
    ├── sessions.rs           # cross-turn session tests (197)
    └── endpoints_metrics.rs  # healthz, models, metrics, doctor tests (448)
```

> Line counts are approximate and include comments; they drift as the code evolves.
> Update them when making significant changes.

~18,748 lines total. 314 tests (273 unit + 5 e2e-mock unit + 35 integration + 1 ignored).

**Catalog generation is deny-by-default**: the allowlist inherits only
`base_instructions` and `model_messages` from the bundled Codex template; every
other template field (dialect switches, OpenAI-ecosystem fields, TUI
decoration) is dropped and logged at generation time. See the README's
"Catalog Generation Policy (deny-by-default)" for the full policy and the
`doctor` upgrade runbook. **`doctor`** (offline, default) regenerates the
catalog in memory and deep-compares it against the installed one to detect any
drift; `--live` drives the installed Codex CLI through an in-process router +
mock upstream, asserting the normalized wire shape and a full tool round-trip.
