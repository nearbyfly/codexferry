# AGENTS.md — Notes for AI Agents Working on This Repo

> Read this before making any changes. It captures non-obvious conventions,
> gotchas, and design decisions that are easy to get wrong.

## Project at a Glance

`codexferry` is a local proxy daemon that lets Codex CLI (≥0.128) use
Chat-Completions-only upstreams (DeepSeek, Kimi, GLM, SiliconFlow, …) through a
single Responses-API endpoint. It routes by `provider/alias` model names,
converts between Responses ↔ Chat Completions protocols, and maintains
in-memory session state for cross-provider multi-turn conversations.

**Migration spec:** `docs/superpowers/specs/2026-08-22-codexferry-migration-design.md`
— this repo was migrated from `codex-router-rs` and renamed on 2026-08-22.

**On `spec §N` references in source comments:** ~114 comments across the codebase
cite sections of the *original* design doc (`spec §1` … `spec §14`), which was
not carried over in the migration. Those section numbers resolve against
`docs/superpowers/specs/2026-08-13-codex-router-design.md` in the retained
`codex-router-rs` repo. They are kept because they mark genuine provenance —
treat them as pointers to history, not to a file in this repo.

## Critical Conventions

### 1. Route key format is `provider/alias` — first `/` splits

Route keys like `deepseek/deepseek-v4-flash` are split on the **first** `/`
only. The prefix must match a `[providers.X]` key. Aliases may contain `/`
(e.g. `openai/o3-mini/high`). Use `split_once('/')`, never `split('/')`.

### 2. SSE parsing is hand-written — do not add dependencies

The SSE parser in `src/upstream.rs` (`parse_sse_stream`) is intentionally
dependency-free. It handles:
- `data:` lines (with or without space after colon)
- Multi-line `data:` within one event (joined with `\n`)
- Comment lines (`:` prefix, used as keepalive)
- `event:` / `id:` / `retry:` lines (ignored)
- `data: [DONE]` sentinel
- `\n\n` and `\r\n\r\n` event delimiters
- UTF-8 characters split across chunk boundaries (buffered at byte level)

If you touch the parser, run `cargo test upstream` — there are extensive
fixture-based tests including CRLF, split UTF-8, and trailing-event flushing.

### 3. Tracing format strings use `{field}` placeholders

Tracing macros auto-capture local variables via `{var_name}` in the message
string. For example `"session hit {id}"` captures the local `id` variable using
its `Value` impl. Do **not** confuse this with `format!`-style `{}` — positional
placeholders are not supported. For Display instead of Debug, use explicit field
assignment: `tracing::info!(id = %id, "session hit")`.

### 4. Responses-format passthrough keys sessions by upstream ID

For `format = "responses"` providers, the proxy relays SSE through — byte-for-byte
verbatim when the healing quirks (`dsml_heal`/`think_tags`) are off, and
event-granular healed (rewritten/injected) when they fire — and keys the
session store by the **upstream's** response ID (extracted from the
`response.completed` event), not a proxy-generated one. This is a deliberate
deviation from spec §8.4 ("代理生成自己的 response ID") because passthrough
means the client only ever sees upstream IDs (healing rewrites events in place
and never re-keys). A proxy-generated fallback key would be unreachable on the
next turn.

### 5. `previous_response_id` is consumed, never forwarded

The proxy strips `previous_response_id` from every request before forwarding to
any upstream. It uses the ID to look up stored conversation context from
`SessionStore`, merges it with new input, and converts the combined history. On
cache miss, it logs at `debug` level (in `SessionStore::get`) and degrades
gracefully (new input only, no crash).

### 6. Session storage is full-context snapshots (O(n²))

Each `response_id` stores the **complete** conversation context (all prior items
+ new input + new output), not incremental deltas. This is intentional for MVP
simplicity. Memory is bounded by TTL + LRU + `max_memory_mb`. See spec §8.2 and
§19 (risk: O(n²) storage growth). Requests carrying `store: false` skip
persistence entirely (the `store_enabled()` helper in proxy/capture.rs; only a literal
boolean `false` disables — absent or `true` stores as before): Codex sends
`store: false` and replays its full transcript inline every turn, so the
snapshot would never be read back.

### 7. Config hot-reload uses a channel + async applier (no lost updates)

The `notify` watcher callback runs on a synchronous thread. It uses
`tokio::sync::mpsc::unbounded_channel` to send the parsed config to an
applier task on the tokio runtime, which awaits the async `RwLock` write.
If the lock is busy (during an active request), the update is **queued** in
the channel and applied as soon as read guards release. Do not call a
blocking `write()` from the notify callback itself - it would stall the
notify thread behind every in-flight request. The channel indirection keeps
the notify thread free while guaranteeing delivery.

### 8. Tool calls are accumulated during streaming, emitted at stream end

Streaming tool calls arrive as deltas keyed by `index`. The `StreamConverter`
accumulates them silently in a `BTreeMap<usize, (id, name, arguments)>` during
the stream (no events emitted). Function-name deltas are handled
idempotently: an incoming name that extends (or repeats) the accumulated one
replaces it, a non-extending continuation is appended - so upstreams that
re-send the full name on every delta chunk do not produce "execexec"
concatenations, while genuinely split name fragments are still stitched. A
missing/empty tool-call `id` is replaced by a synthesized `call_<uuid>`
(once per index; a real id on a later chunk still wins), in BOTH the
streaming and non-streaming paths - a stored `function_call` with `call_id
""` would replay as an assistant tool_call with `id: ""`, which strict Chat
upstreams reject. The whole finish sequence is deferred to
stream end (`StreamConverter::finish`, called after the `[DONE]` sentinel or
upstream close), NOT to the chunk carrying `finish_reason`: the router
requests `stream_options.include_usage`, whose usage-only trailing chunk
(empty choices) arrives after the `finish_reason` chunk, so deferring is what
puts the real token counts into `response.completed`. `finish()` is a no-op
when no `finish_reason` was seen (the caller emits the error sequence instead)
and when called twice (no duplicated items). Zero-payload tool-call deltas
create no phantom items: a delta with neither id nor function content (index
announcement) never creates an accumulator entry, and an entry that still has
neither a name nor arguments at stream end (id-only variant) is dropped rather
than emitted as a `name: ""` / `arguments: "{}"` function_call. At stream end,
complete items are emitted in ascending index order: `output_item.added` (status `in_progress`) →
`function_call_arguments.delta` (full accumulated args in one shot) →
`output_item.done` (status `completed`).

### 8a. SSE events follow the real OpenAI Responses wire format

Every event's JSON data includes `"type": "response.<event_name>"`. The
`response.created` and `response.completed` events wrap the response object
under a `"response"` key. Each output item has a stable per-response ID
(`msg_<uuid>`, `rs_<uuid>`, `fc_<uuid>`). Items are created lazily: the
`output_item.added` event fires only when the first delta of that type
arrives, not eagerly on the first chunk. The events `response.in_progress`,
`content_part.added/done`, and `function_call_status_changed` are NOT
emitted. Reference: `~/foss/codex-relay/src/stream.rs`.

Client-facing event order follows `output_index` (first-delta arrival
order), but the session items persisted for replay are stored in CANONICAL
turn order - reasoning before message - so a text-before-reasoning stream
still replays with its reasoning attached to the same turn (a
message-then-reasoning storage order would leave the reasoning dangling and
dropped by `to_chat_request`).

### 8b. One item-merge loop for both sources; passthrough sessions are unnormalized

`push_item_messages` in `convert/request.rs` is the ONLY item-to-message loop:
it converts both session history (`previous_response_id`) and the `input`
items array (Codex store=false replays its full transcript there inline).
Any merge/normalization rule must go in that one loop or the two sources
drift.

Known limitation (unfixed): sessions captured by responses-format passthrough
are stored verbatim from the upstream, so a non-conforming upstream can store
a `function_call` with an empty/missing `call_id`. When that session is later
replayed on a chat-format route (the store is shared across routes), the
assistant `tool_calls[].id` and the paired tool message's `tool_call_id` are
both `""`, which strict Chat upstreams reject for the rest of the session's
TTL. Router-generated items are immune (ids are synthesized at ingestion).
A fix must synthesize `call_<uuid>` for the function_call AND rewrite the
matching `function_call_output` to the same id — patching only one side
breaks the pairing.

### 9. `convert_content()` collapses single-text-part to plain string

When a Responses message has exactly one `input_text`/`output_text` part, the
converter emits a plain JSON string (not an array) for Chat content. This
matches the spec §7.1 note: "纯文本时用字符串形式". Multi-part content
(text + image) stays as an array. Image-only content also stays as an array.

### 10. Code comments are in English with module-level docs

Every module has a `//!` doc comment (purpose + role in the architecture), every
public item has a `///` doc comment, and non-obvious logic has inline `//`
comments. Keep comments in English and reference spec sections (`§7.3`) when
describing behavior. When you add or change code, keep surrounding comments
accurate — do not leave stale comments that describe old behavior.

### 11. Exactly one per-request log line via `log_request()`

`handle_responses` emits one `tracing::info!` per request (route, upstream,
model, status, input/output tokens, elapsed_ms, and for 4xx/5xx the error
message - spec §11) using the `log_request()` helper. Streaming requests are
logged from the spawned stream task (real token counts are only known at
stream end); error responses are logged from the handler, with the
`error.message` extracted from the response body via `extract_error_detail`.
Do not add extra per-request log lines. The documented
anomaly-only `warn!` exceptions (they fire on genuine anomalies, never on a
healthy request): the `missing_done` quirk/truncation warns in the chat stream
task, the streaming idle-timeout warns, and the non-2xx upstream body warn
(body truncated to 1KB via `truncate_for_log`). One more exception is the
version tripwire's `observe_client_version` info/warn: they are once-per-
distinct-version event logs (per daemon process, not per request) fired on the
first `/models` request carrying a given `client_version` — the info announces
`codex client {from} → {to} detected`, and the warn follows when that version
is not verified green in doctor's state file. Log level is controlled via
`RUST_LOG`; `CODEXFERRY_TRACE_BODY=1` adds debug body tracing.

### 12. `/models` is dual-shape

The `/v1/models` (and `/models`) endpoint serves two different response shapes
selected by the `client_version` query parameter:
- **No param** → Chat-Completions list shape (`{"object":"list","data":[...]}`) — unchanged from previous versions.
- **`client_version` present** (any value, including empty) → Codex `ModelsResponse`
  catalog shape (`{"models":[...]}`) — the live catalog built from the
  hot-reloaded config.

Codex CLI sends `client_version` unconditionally; no OpenAI-compatible list
client sends it. Catalog building logic lives exclusively in
`catalog.rs::build_catalog_value`. Both branches emit `ETag` headers and
support `If-None-Match` → `304 Not Modified`.

### 13. Keep the three top-level MD docs in sync

`README.md` (user guide), `ARCHITECTURE.md` (design, incl. §11 line counts and
test counts), and this file describe the current codebase. When making
significant changes, update the relevant docs in the same change — especially
`ARCHITECTURE.md` §11, which lists per-file line counts.

### 13. Namespaced tool calls decode via the map, never by splitting on `-`

On the chat path, namespace tools (e.g. `multi_agent_v1`) are bound upstream
under the encoded name `{namespace}-{name}` (e.g. `multi_agent_v1-spawn_agent`),
and the proxy builds a `NamespaceToolMap` from the request's tools. Response
tool calls are decoded back to a Responses `namespace` field via that map —
never by splitting the encoded name on `-`: the encoding is not reversible
(a tool name may itself contain a hyphen, e.g. `spawn-agent`), so the separator
alone cannot recover the namespace boundary. See spec §7.1.

## Architecture Quick Reference

```
Codex CLI ──POST /v1/responses──▶ axum daemon (127.0.0.1:8787)
                                   │
                    ┌──────────────┼──────────────┐
                    ▼              ▼              ▼
              [chat provider] [chat provider] [responses provider]
              convert req    convert req     passthrough
              convert SSE    convert SSE     forward SSE as-is
                    │              │              │
                    ▼              ▼              ▼
              SessionStore (in-memory, TTL + LRU)
```

## Module Responsibilities

| Module | Responsibility |
|--------|---------------|
| `main.rs` | CLI entry (clap), dispatches to server, `gen-catalog`, or `doctor` |
| `config.rs` | TOML types, validation, hot-reload watcher |
| `proxy/mod.rs` | axum router, request dispatch, streaming orchestration |
| `proxy/capture.rs` | session/usage capture shared by the chat + passthrough paths |
| `proxy/upstream.rs` | upstream HTTP transport (`send_upstream`) + error-class dedup helpers |
| `proxy/chat.rs` | chat-format handler (Responses → Chat conversion path) |
| `proxy/passthrough.rs` | responses-format passthrough handler (verbatim relay + event healing) |
| `upstream.rs` | SSE parser, API key resolution, URL construction |
| `session.rs` | `SessionStore`: in-memory, TTL, LRU eviction |
| `catalog.rs` | `gen-catalog` + `build_catalog_value` shared with the live /models endpoint |
| `models_cache.rs` | `CatalogCache`: route-fingerprint + template-mtime invalidation for live /models |
| `mode.rs` | codex catalog-wiring mode detection (pinned/dynamic/fallback) from `~/.codex/config.toml` (`detect_mode` + `DEFAULT_ACTIVE_PROVIDER`) |
| `version.rs` | codex client-version tripwire (`CodexVersionTracker`) + doctor state (`DoctorState`) |
| `doctor.rs` | doctor subcommand: mode-aware offline quick-checks (L1–L2 + mode-keyed advisories; pinned L2.7–L2.10 pin checks; dynamic L2.7' pin-shadow WARN + L2.8'/L2.9' endpoint smoke/shape), WARN/INFO/FAIL report + exit codes |
| `doctor_live.rs` | doctor --live: mode-aware L3 live probe (probe wiring mirrors the detected mode; live-catalog-fetch proof via codex `models_cache.json`; returns checks for the combined default output) |
| `logging.rs` | tracing-subscriber init |
| `metrics.rs` | `Metrics`: Prometheus registry + `/metrics` encoding (upstream requests, tokens, latency, in-flight) |
| `normalize.rs` | boundary normalization: additional_tools hoist, chat namespace flattening + encode/decode map, unknown-type visibility + counters |
| `heal/` | DSML/think response healing: `mod.rs` (HealGates + facade), `think.rs`, `dsml.rs`, `responses.rs` |
| `wire/responses.rs` | Responses API serde types (inbound from Codex) |
| `wire/chat.rs` | Chat Completions serde types (outbound to upstream) |
| `convert/request.rs` | Responses → Chat request conversion (incl. namespace encode on history replay) |
| `convert/response.rs` | Chat → Responses response conversion (JSON + SSE streaming, incl. namespace decode) |

## Testing Strategy

- **Unit tests**: inline `#[cfg(test)] mod tests` in smaller modules; the
  proxy/heal/convert/normalize suites live in sibling files (`src/proxy/tests.rs`,
  `src/proxy/capture/tests.rs`, `src/heal/{think,dsml,responses_healer}_tests.rs`,
  `src/convert/*/tests.rs`, `src/normalize/tests.rs`).
- **Integration tests** use the shared harness in `tests/common/mod.rs` plus five
  topical binaries (`chat_conversion.rs`, `passthrough.rs`, `healing.rs`,
  `sessions.rs`, `endpoints_metrics.rs`), each spawning the real binary as a
  subprocess against a mock axum upstream. They use `CARGO_BIN_EXE_codexferry`
  and poll `/healthz` before making assertions.

- **E2E scripts** (`scripts/e2e.sh`, `scripts/e2e-real.sh`) drive the real
  Codex CLI against a scripted mock (`src/bin/e2e-mock.rs`) or real upstreams.
  They are manual tools outside `cargo test`; see README "End-to-End Tests".
- The default `codexferry doctor` run composes L1 + L2 offline checks then the
  L3 live probe into one report; `--offline` / `--live` select the stages
  (see `docs/superpowers/specs/2026-08-23-mode-aware-doctor-design.md`).
- Integration tests bind to ephemeral ports with a TOCTOU retry (see `setup()`).
- Env-mutating tests use an `EnvGuard` RAII guard to clean up on panic.

## Things to Watch Out For

- **reqwest's per-request `.timeout()` is a TOTAL deadline that keeps being
  enforced on every streaming-body read** (verified against reqwest 0.12.28
  internals: `TotalTimeoutBody` polls the deadline on each body read). Setting
  it on a streaming request caps the stream's TOTAL duration — a healthy
  long stream dies at `timeout_ms`. That is why `send_upstream` only sets it
  for non-streaming requests; streaming bounds the header phase externally
  (`tokio::time::timeout` around `send()`) and the body via the manual
  per-chunk idle timeout in the stream loops (issue #14). Do not "simplify"
  this back to a single `.timeout()`.
- **Don't add `async-stream`**: the plan originally proposed it, but the final
  implementation uses `tokio::sync::mpsc` + `tokio_stream::wrappers::ReceiverStream`
  instead. No `async-stream` dependency exists.
- **`ChatRequest` serialization**: all `Option` fields use
  `skip_serializing_if = "Option::is_none"`. When adding new fields, follow this
  pattern or upstreams may reject `null` values.
- **`ResponsesRequest` uses `#[serde(flatten)] extra`**: unknown fields are
  captured in `extra: serde_json::Map<String, Value>`. Passthrough fields
  (`stop`, `seed`, `user`, `presence_penalty`, `frequency_penalty`,
  `tool_choice`) are extracted from `extra` in `to_chat_request()`.
- **Non-streaming chat path reads bytes first**: the handler calls
  `upstream_resp.bytes().await` then `serde_json::from_slice` (not
  `upstream_resp.json()`) so that `CODEXFERRY_TRACE_BODY` can log the raw body.
- **`SessionStore` is `Clone`**: it wraps `Arc<RwLock<SessionState>>`, so cloning
  is cheap and shares state. The streaming handler clones it into a spawned task.
