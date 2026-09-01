//! Chat Completions → Responses conversion (spec §7.2 / §7.3).
//!
//! Converts upstream Chat Completions responses back into the Responses API
//! shape for Codex CLI. Two entry points:
//!
//! - **Non-streaming** (`chat_response_to_items`): the fallback path for
//!   `stream: false` requests (also used by tests). A complete `ChatResponse`
//!   JSON body is mapped to Responses output items: `reasoning_content` →
//!   `reasoning` item, `content` → `message` item, `tool_calls` →
//!   `function_call` items (§7.2).
//! - **Streaming** (`StreamConverter`): the main path. Each Chat SSE chunk is
//!   converted token-by-token into Responses SSE events, from
//!   `response.created` through `response.completed`, while output items are
//!   accumulated in memory for session storage (§7.3 / §8.2).
//!
//! Shared building blocks:
//! - `OutputAccumulator` — scratch state for text / reasoning / usage that is
//!   folded into the final output `items` at stream end.
//! - `reasoning_item` / `message_item` / `function_call_item` — Responses
//!   output-item JSON builders.
//! - `build_completed_response` — the non-streaming JSON response object,
//!   which carries the full output list and token usage (the streaming path
//!   builds its `response.completed` inline in `StreamConverter::emit_finish`).
//! - `sse_event` — formats an event as an `(event_type, data)` pair.
use crate::heal::{DsmlStreamFilter, HealGates, ThinkStreamFilter};
use crate::normalize::{NamespaceToolMap, NamespaceToolName};
use crate::wire::chat::*;
use serde_json::{json, Value};

/// Accumulated output items from a streaming response, stored in SessionStore.
///
/// Scratch buffers:
/// - `text`: concatenated `delta.content` text seen so far.
/// - `reasoning`: concatenated reasoning deltas seen so far.
/// - `usage`: token usage, captured from whichever chunk carries it (usually
///   the trailing chunk, thanks to `stream_options.include_usage`).
///
/// Final result:
/// - `items`: the Responses output items (`message` / `reasoning` /
///   `function_call`) assembled once the stream finishes. `text` becomes a
///   `message` item, `reasoning` is inserted at index 0 as a `reasoning` item
///   (matching the order the client saw deltas in), and tool calls are
///   appended in ascending index order. The finished `items` list is the
///   complete conversation context handed to `SessionStore` for the next turn.
///
/// `new()` is a convenience constructor equivalent to `Default::default()`.
#[derive(Debug, Clone, Default)]
pub struct OutputAccumulator {
    pub items: Vec<Value>,
    pub text: String,
    pub reasoning: String,
    pub usage: Option<ChatUsage>,
}

impl OutputAccumulator {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Build a Responses `reasoning` output item.
///
/// Mirrors the shape produced by the request converter's `pending_reasoning`
/// logic, so the round-trip (reasoning → `reasoning_content` → reasoning)
/// stays consistent across session replay (spec §7.4).
fn reasoning_item(text: &str) -> Value {
    json!({
        "type": "reasoning",
        "summary": [{ "type": "summary_text", "text": text }]
    })
}

/// Build a Responses `message` output item for assistant text.
///
/// A single `output_text` part wrapping the accumulated text; the mirror image
/// of the request converter's `message` item conversion.
fn message_item(text: &str) -> Value {
    json!({
        "type": "message",
        "role": "assistant",
        "content": [{ "type": "output_text", "text": text }]
    })
}

/// Build a Responses `function_call` output item.
///
/// Fields mirror what the request converter's `function_call` replay branch
/// expects (`call_id`, `name`, `arguments`). A non-empty `namespace`
/// (restored from a namespaced Chat tool-call name) is emitted as the
/// independent `namespace` field Codex dispatches on (spec §7).
fn function_call_item(
    call_id: &str,
    namespace: Option<&str>,
    name: &str,
    arguments: &str,
) -> Value {
    let mut item = json!({
        "type": "function_call",
        "call_id": call_id,
        "name": name,
        "arguments": arguments
    });
    if let Some(ns) = namespace {
        item["namespace"] = Value::String(ns.to_string());
    }
    item
}

/// Decode a Chat tool-call name back to a Responses `namespace` + `name`
/// pair. Names known in the request's [`NamespaceToolMap`] restore their
/// namespace; everything else stays flat - the normal path for non-namespaced
/// tools and for old flat sessions.
fn decode_tool_call_name(name: &str, ns_map: &NamespaceToolMap) -> (Option<String>, String) {
    match ns_map.get(name) {
        Some(NamespaceToolName {
            namespace,
            name: inner,
        }) => (Some(namespace.clone()), inner.clone()),
        None => (None, name.to_string()),
    }
}

/// The empty/missing tool-call id fallback: a fresh `call_<uuid>`.
///
/// Upstreams that omit tool-call ids (non-spec, but common among
/// OpenAI-compatible servers) must not leak an empty `call_id` into the
/// session - replay would emit an assistant tool_call with `id: ""`, which
/// strict Chat upstreams reject. Shared by BOTH conversion paths so the id
/// shape can never drift between them.
fn synthesized_call_id() -> String {
    format!("call_{}", uuid::Uuid::new_v4().simple())
}

/// Convert a non-streaming Chat response to Responses output items.
///
/// Emits items in this order (spec §7.2): `reasoning` (from
/// `reasoning_content`, if non-empty) → `message` (from `text_content()`, if
/// non-empty) → `function_call` (one per `tool_calls` entry). Only the first
/// `choice` is converted; later choices are ignored. A missing `id` is
/// replaced by a synthesized `call_<uuid>` (strict Chat upstreams reject
/// empty ids on replay); missing `name` / `arguments` fall back to
/// `""` / `"{}"`. The resulting items are stored in the session and later
/// replayed by `to_chat_request`, so this ordering and these fallbacks keep
/// the round-trip consistent.
///
/// `ns_map` decodes encoded `{namespace}-{name}` tool-call names back to
/// their original `namespace` + `name` pair (spec §7); names not in the map
/// stay flat.
pub fn chat_response_to_items(resp: &ChatResponse, ns_map: &NamespaceToolMap) -> Vec<Value> {
    let mut items = Vec::new();
    if let Some(choice) = resp.choices.first() {
        let msg = &choice.message;
        // Reasoning item
        if let Some(rc) = &msg.reasoning_content {
            if !rc.is_empty() {
                items.push(reasoning_item(rc));
            }
        }
        // Message item (only string content is supported in the non-streaming path)
        let text = msg.text_content();
        if !text.is_empty() {
            items.push(message_item(text));
        }
        // Function call items
        if let Some(tool_calls) = &msg.tool_calls {
            for tc in tool_calls {
                let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let call_id = if id.is_empty() {
                    synthesized_call_id()
                } else {
                    id.to_string()
                };
                let name = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let (namespace, decoded_name) = decode_tool_call_name(name, ns_map);
                let args = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}");
                items.push(function_call_item(
                    &call_id,
                    namespace.as_deref(),
                    &decoded_name,
                    args,
                ));
            }
        }
    }
    items
}

/// Build the final response object for the non-streaming JSON path.
///
/// Includes `"status": "completed"` and per-item IDs, matching the shape
/// Codex CLI expects in a non-streaming Responses JSON body. Token usage is
/// mapped from Chat's `prompt_tokens` / `completion_tokens` into Responses'
/// `input_tokens` / `output_tokens`; when `usage` is absent both default to 0
/// and `total_tokens` is their sum.
pub fn build_completed_response(
    response_id: &str,
    model: &str,
    items: &[Value],
    usage: Option<&ChatUsage>,
) -> Value {
    let (input_tokens, output_tokens) = usage
        .map(|u| (u.prompt_tokens, u.completion_tokens))
        .unwrap_or((0, 0));
    // Stamp each item with a stable ID if it doesn't already have one.
    let output: Vec<Value> = items
        .iter()
        .map(|item| {
            let mut obj = item.clone();
            if let Some(obj) = obj.as_object_mut() {
                if !obj.contains_key("id") {
                    let prefix = match obj.get("type").and_then(|v| v.as_str()) {
                        Some("reasoning") => "rs_",
                        Some("message") => "msg_",
                        Some("function_call") => "fc_",
                        _ => "item_",
                    };
                    obj.insert(
                        "id".into(),
                        Value::String(format!("{}{}", prefix, uuid::Uuid::new_v4().simple())),
                    );
                }
                // Ensure message items have status:completed
                if obj.get("type").and_then(|v| v.as_str()) == Some("message") {
                    obj.insert("status".into(), Value::String("completed".into()));
                }
            }
            obj
        })
        .collect();
    json!({
        "id": response_id,
        "object": "response",
        "status": "completed",
        "model": model,
        "output": output,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "total_tokens": u64::from(input_tokens) + u64::from(output_tokens)
        }
    })
}

/// Stateful converter that transforms Chat SSE chunks into Responses SSE events.
///
/// Produces events compatible with Codex CLI (≥0.128), matching the wire format
/// of the real OpenAI Responses API as confirmed by the codex-relay reference
/// implementation. Key design decisions:
///
/// - **Lazy item creation**: `output_item.added` is emitted only when the first
///   delta of that type (text, reasoning, or tool-call at finish) arrives — not
///   eagerly on the first chunk. A pure-tool-call response never creates a
///   message item.
/// - **Deferred finish**: the entire finish sequence (done events,
///   tool-call items, `response.completed`) is deferred past the chunk
///   carrying `finish_reason` to the end of the upstream stream
///   ([`StreamConverter::finish`]). The router requests
///   `stream_options.include_usage`, whose usage-only chunk (empty choices)
///   arrives AFTER the finish_reason chunk, so emitting at stream end is what
///   gets the real token counts into `response.completed`.
/// - **Deferred tool calls**: tool-call deltas are accumulated silently during
///   streaming and emitted as complete items (added → delta → done) in the
///   finish sequence, not incrementally.
/// - **No `response.in_progress`, `content_part.added/done`, or
///   `function_call_status_changed`**: these events are not emitted; Codex CLI
///   does not require them (confirmed by codex-relay).
/// - **Stable item IDs**: `msg_<uuid>`, `rs_<uuid>`, `fc_<uuid>` per response,
///   carried in every event referencing that item.
/// - **`type` + `response` wrappers**: every event's JSON data includes
///   `"type": "response.<event_name>"`; `response.created`/`response.completed`
///   nest the response object under `"response"`.
pub struct StreamConverter {
    pub response_id: String,
    pub model: String,
    pub acc: OutputAccumulator,
    /// Accumulated tool calls keyed by index: (id, name, arguments).
    tool_calls: std::collections::BTreeMap<usize, (String, String, String)>,
    /// Per-request namespace decode map (built from the request's tools).
    namespace_tools: NamespaceToolMap,
    /// Stable per-response item IDs for message and reasoning items.
    msg_item_id: String,
    reasoning_item_id: String,
    /// Next output index to assign (increments as items are created).
    next_output_index: usize,
    /// Output index assigned to the reasoning item (None until created).
    reasoning_output_index: Option<usize>,
    /// Output index assigned to the message item (None until created).
    message_output_index: Option<usize>,
    /// Whether response.created has been emitted.
    started: bool,
    /// The `finish_reason` carried by the most recent chunk that had one
    /// (None until then). The proxy's `missing_done` quirk reads this to
    /// tell a merely unterminated stream (no `[DONE]` sentinel, but the
    /// model finished) from a truncated one, and to log which reason it
    /// finished with.
    finish_reason: Option<String>,
    /// Whether the finish sequence has been emitted (guards double emission).
    finish_emitted: bool,
    /// Healing filters (quirks `dsml_heal` + `think_tags`), constructed from
    /// the proxy-provided gates. Disabled filters are identity.
    dsml_filter: DsmlStreamFilter,
    think_filter: ThinkStreamFilter,
}

impl StreamConverter {
    /// Create a converter for one streaming request.
    ///
    /// `response_id` is the proxy-generated `resp_<uuid>` that will be
    /// announced in `response.created` and echoed in `response.completed`;
    /// `model` is the upstream model name shown to the client; `heal` carries
    /// the quirks gates. `namespace_tools` is the request's
    /// [`NamespaceToolMap`], used at stream end to decode encoded
    /// `{namespace}-{name}` tool-call names back to their `namespace` field
    /// (spec §7). Stable per-response item IDs (`msg_`, `rs_`) are generated
    /// up front so every event referencing an item carries a consistent ID.
    pub fn new(
        response_id: String,
        model: String,
        heal: HealGates,
        namespace_tools: NamespaceToolMap,
    ) -> Self {
        Self {
            response_id,
            model,
            acc: OutputAccumulator::new(),
            tool_calls: Default::default(),
            namespace_tools,
            msg_item_id: format!("msg_{}", uuid::Uuid::new_v4().simple()),
            reasoning_item_id: format!("rs_{}", uuid::Uuid::new_v4().simple()),
            next_output_index: 0,
            reasoning_output_index: None,
            message_output_index: None,
            started: false,
            finish_reason: None,
            finish_emitted: false,
            dsml_filter: DsmlStreamFilter::new(heal.dsml),
            think_filter: ThinkStreamFilter::new(heal.think),
        }
    }

    /// Emit the stream-opening `response.created` event once (None on
    /// subsequent calls).
    ///
    /// The proxy calls this eagerly - right after the upstream accepts the
    /// stream, before the first chunk arrives - because at high/max
    /// reasoning effort the upstream's first chunk can lag by tens of
    /// seconds, and without an early opener the client stares at a silent
    /// stream meanwhile. `on_chunk` also calls it lazily, so converters
    /// driven without an eager call behave exactly as before.
    pub fn start(&mut self) -> Option<(String, String)> {
        if self.started {
            return None;
        }
        self.started = true;
        Some(sse_event(
            "response.created",
            json!({
                "type": "response.created",
                "response": {
                    "id": &self.response_id,
                    "status": "in_progress",
                    "model": &self.model
                }
            }),
        ))
    }

    /// Process a chunk, returns (event_type, data) pairs to send to client.
    ///
    /// The first call emits `response.created` (via [`StreamConverter::start`]).
    /// Subsequent calls emit deltas for
    /// text and reasoning (lazily creating items on first delta). Tool-call
    /// fragments are accumulated silently. The chunk carrying a
    /// `finish_reason` only marks the response finished; the finish sequence
    /// itself (done events, tool-call items, `response.completed`) is emitted
    /// by [`StreamConverter::finish`] at stream end, so the trailing
    /// usage-only chunk is captured.
    pub fn on_chunk(&mut self, chunk: &ChatStreamChunk) -> Vec<(String, String)> {
        let mut events = Vec::new();

        if let Some(event) = self.start() {
            events.push(event);
        }

        if let Some(choice) = chunk.choices.first() {
            let delta = &choice.delta;

            // Healing pipeline (quirks dsml_heal + think_tags): content is
            // isolated from DSML FIRST (a DSML parameter value may
            // legitimately contain `<think>` text as part of an argument),
            // then split by the think filter. Native reasoning_content is
            // never filtered; think-healed reasoning is appended AFTER it
            // (codex-relay per-delta order).
            let healed = {
                let dsml_text = self
                    .dsml_filter
                    .push(delta.content.as_deref().unwrap_or(""));
                self.think_filter.push(&dsml_text)
            };
            let reasoning_delta: String = match delta.reasoning_text() {
                Some(native) if healed.reasoning.is_empty() => native.to_string(),
                Some(native) => format!("{native}{}", healed.reasoning),
                None => healed.reasoning.clone(),
            };
            if !reasoning_delta.is_empty() {
                events.extend(self.reasoning_delta_events(&reasoning_delta));
            }
            if !healed.text.is_empty() {
                events.extend(self.text_delta_events(&healed.text));
            }

            // Tool-call deltas: accumulate silently (emitted at finish).
            if let Some(tool_calls) = &delta.tool_calls {
                for tc in tool_calls {
                    // Skip zero-payload deltas: some upstreams announce an
                    // index with neither id nor function content. Creating an
                    // entry for those would emit a phantom function_call
                    // (name "", arguments "{}") at finish.
                    let has_payload = tc.id.as_deref().is_some_and(|id| !id.is_empty())
                        || tc.function.as_ref().is_some_and(|f| {
                            f.name.as_deref().is_some_and(|n| !n.is_empty())
                                || f.arguments.as_deref().is_some_and(|a| !a.is_empty())
                        });
                    if !has_payload {
                        continue;
                    }
                    // The entry's id defaults to a synthesized call_<uuid>
                    // (upstreams that omit tool-call ids must not leak an
                    // empty call_id into the session - see
                    // [`synthesized_call_id`]); a real id, on this or a later
                    // chunk, overwrites it via the assignment below.
                    let entry = self
                        .tool_calls
                        .entry(tc.index)
                        .or_insert_with(|| (synthesized_call_id(), String::new(), String::new()));
                    if let Some(id) = &tc.id {
                        if !id.is_empty() {
                            entry.0 = id.clone();
                        }
                    }
                    if let Some(func) = &tc.function {
                        if let Some(name) = &func.name {
                            if !name.is_empty() {
                                // Per spec the function name arrives once,
                                // complete, on the index's first delta - but
                                // some OpenAI-compatible upstreams re-send the
                                // FULL name on every delta for the index,
                                // while others may split it into fragments
                                // like the arguments. Idempotent rule: a name
                                // that extends (or repeats) the accumulated
                                // one replaces it; a non-extending
                                // continuation is appended. Concatenating
                                // blindly would corrupt re-sending upstreams
                                // into "execexec".
                                if name.starts_with(entry.1.as_str()) {
                                    entry.1 = name.clone();
                                } else {
                                    entry.1.push_str(name);
                                }
                            }
                        }
                        if let Some(args) = &func.arguments {
                            entry.2.push_str(args);
                        }
                    }
                }
            }

            // Finish: the chunk carrying finish_reason only marks the
            // response finished. The finish sequence is deferred to stream
            // end (`finish`), because the include_usage trailing chunk with
            // the real token counts arrives AFTER the finish_reason chunk.
            if let Some(reason) = &choice.finish_reason {
                self.finish_reason = Some(reason.clone());
            }
        }

        // Capture usage even on chunks without finish_reason (trailing usage chunk).
        if let Some(usage) = &chunk.usage {
            self.acc.usage = Some(usage.clone());
        }

        events
    }

    /// Emit one reasoning delta (lazily creating the reasoning item).
    /// Shared by `on_chunk` and the heal flush in `finish`.
    fn reasoning_delta_events(&mut self, reasoning: &str) -> Vec<(String, String)> {
        let mut events = Vec::new();
        if self.reasoning_output_index.is_none() {
            let idx = self.next_output_index;
            self.next_output_index += 1;
            self.reasoning_output_index = Some(idx);
            events.push(sse_event(
                "response.output_item.added",
                json!({
                    "type": "response.output_item.added",
                    "output_index": idx,
                    "item": {
                        "type": "reasoning",
                        "id": &self.reasoning_item_id,
                        "summary": [{ "type": "summary_text", "text": "" }]
                    }
                }),
            ));
        }
        let idx = self.reasoning_output_index.unwrap();
        self.acc.reasoning.push_str(reasoning);
        events.push(sse_event(
            "response.reasoning_summary_text.delta",
            json!({
                "type": "response.reasoning_summary_text.delta",
                "item_id": &self.reasoning_item_id,
                "output_index": idx,
                "summary_index": 0,
                "delta": reasoning
            }),
        ));
        events
    }

    /// Emit one text delta (lazily creating the message item). Shared by
    /// `on_chunk` and the heal flush in `finish`.
    fn text_delta_events(&mut self, text: &str) -> Vec<(String, String)> {
        let mut events = Vec::new();
        if self.message_output_index.is_none() {
            let idx = self.next_output_index;
            self.next_output_index += 1;
            self.message_output_index = Some(idx);
            events.push(sse_event(
                "response.output_item.added",
                json!({
                    "type": "response.output_item.added",
                    "output_index": idx,
                    "item": {
                        "type": "message",
                        "id": &self.msg_item_id,
                        "role": "assistant",
                        "status": "in_progress",
                        "content": []
                    }
                }),
            ));
        }
        let idx = self.message_output_index.unwrap();
        self.acc.text.push_str(text);
        events.push(sse_event(
            "response.output_text.delta",
            json!({
                "type": "response.output_text.delta",
                "item_id": &self.msg_item_id,
                "output_index": idx,
                "delta": text
            }),
        ));
        events
    }

    /// Emit the deferred finish sequence at stream end.
    ///
    /// Call once the upstream stream has ended (`[DONE]` sentinel or stream
    /// close). The sequence - done events for reasoning/message, tool-call
    /// items (added -> delta -> done), and `response.completed` - is deferred
    /// to this point so that the usage-only trailing chunk (empty choices,
    /// sent after the finish_reason chunk when `include_usage` is set) is
    /// reflected in `response.completed`'s usage instead of reporting zeros.
    ///
    /// Returns empty (no events, no session items) when no `finish_reason`
    /// was seen - the caller emits the error sequence via
    /// [`StreamConverter::on_error`] instead - or when the sequence was
    /// already emitted, so a repeated finish_reason chunk cannot duplicate
    /// output items.
    pub fn finish(&mut self) -> Vec<(String, String)> {
        if self.finish_reason.is_none() || self.finish_emitted {
            return Vec::new();
        }
        self.finish_emitted = true;
        let mut events = self.flush_heal();
        events.extend(self.emit_finish());
        events
    }

    /// Flush the healing filters at stream end, BEFORE the finish sequence:
    /// residual text/reasoning emit as ordinary deltas (lazily creating
    /// items), healed DSML calls join the tool-call accumulator so
    /// `emit_finish` emits them through the regular function_call sequence.
    /// DSML first, then think - same order as the per-delta pipeline.
    fn flush_heal(&mut self) -> Vec<(String, String)> {
        let mut events = Vec::new();
        // mem::take: both filters' Default is `new(true)`, but the taken
        // instances are consumed here and never read again.
        let dsml_filter = std::mem::take(&mut self.dsml_filter);
        let (dsml_tail, calls) = dsml_filter.finish();
        if !calls.is_empty() {
            tracing::warn!(
                "quirk dsml_heal fired: healed {} leaked DSML tool call(s) from streamed text",
                calls.len()
            );
        }
        let mut think_filter = std::mem::take(&mut self.think_filter);
        let mut split = think_filter.push(&dsml_tail);
        let think_fired = think_filter.fired();
        let tail = think_filter.finish();
        split.reasoning.push_str(&tail.reasoning);
        split.text.push_str(&tail.text);
        if think_fired {
            tracing::warn!(
                "quirk think_tags fired: split leaked <think> markup out of streamed text"
            );
        }
        if !split.reasoning.is_empty() {
            events.extend(self.reasoning_delta_events(&split.reasoning));
        }
        if !split.text.is_empty() {
            events.extend(self.text_delta_events(&split.text));
        }
        if !calls.is_empty() {
            let base = self.tool_calls.keys().max().map_or(0, |k| k + 1);
            for (i, call) in calls.into_iter().enumerate() {
                self.tool_calls.insert(
                    base + i,
                    (crate::heal::synthesize_call_id(), call.name, call.arguments),
                );
            }
        }
        events
    }

    /// The finish reason seen so far, if any chunk carried one. `Some(_)`
    /// doubles as the old saw-finish-reason flag for the `missing_done`
    /// quirk's completion gate.
    pub fn finish_reason(&self) -> Option<&str> {
        self.finish_reason.as_deref()
    }

    /// Emit the finish sequence: done events for existing items, then
    /// tool-call items (added → delta → done), then response.completed.
    ///
    /// Assembles session items into `self.acc.items` (simplified shapes for
    /// internal storage), in canonical turn order - reasoning before message -
    /// regardless of delta arrival order. The `response.completed` event's
    /// `"output"` array uses the SAME full-shape item JSON as the
    /// `output_item.done` events (carrying `id` + `status`), so the client
    /// sees consistent items throughout the stream (spec §7.3).
    fn emit_finish(&mut self) -> Vec<(String, String)> {
        let mut events = Vec::new();
        // Full-shape items for response.completed's "output" (same JSON as
        // the output_item.done "item" payloads).
        let mut completed_output: Vec<Value> = Vec::new();

        // Collect (output_index, done_item_json) for done events.
        let mut done_sequence: Vec<(usize, Value)> = Vec::new();

        // Reasoning done
        if let Some(idx) = self.reasoning_output_index {
            done_sequence.push((
                idx,
                json!({
                    "type": "reasoning",
                    "id": &self.reasoning_item_id,
                    "summary": [{ "type": "summary_text", "text": &self.acc.reasoning }]
                }),
            ));
        }

        // Message done
        if let Some(idx) = self.message_output_index {
            done_sequence.push((
                idx,
                json!({
                    "type": "message",
                    "id": &self.msg_item_id,
                    "role": "assistant",
                    "status": "completed",
                    "content": [{ "type": "output_text", "text": &self.acc.text }]
                }),
            ));
        }

        // Sort done events by output_index (reasoning may have higher index
        // than message if text arrives before reasoning).
        done_sequence.sort_by_key(|(idx, _)| *idx);

        for (idx, done_item) in &done_sequence {
            events.push(sse_event(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": idx,
                    "item": done_item
                }),
            ));
            // Full-shape item for the completed event's output array.
            completed_output.push(done_item.clone());
        }

        // Session items are persisted in CANONICAL turn order - reasoning
        // before the message - independent of the delta arrival order the
        // client events follow above. History replay (to_chat_request)
        // attaches a reasoning item to the NEXT assistant message, so a
        // message-then-reasoning storage order would leave this turn's
        // reasoning dangling (dropped or misattached) on replay.
        if self.reasoning_output_index.is_some() {
            self.acc.items.push(reasoning_item(&self.acc.reasoning));
        }
        if self.message_output_index.is_some() {
            self.acc.items.push(message_item(&self.acc.text));
        }

        // Tool-call items: added → delta → done, in ascending index order.
        // Entries without a NAME at stream end are dropped. The id-only
        // variant (no name, no arguments) carries no call the client could
        // execute; the args-only variant (arguments streamed but no name
        // fragment ever arrived — a broken or partial upstream) is equally
        // unusable: emitting it would produce a function_call with name ""
        // which replays as a tool_call for an unnamed function that strict
        // Chat upstreams reject for the rest of the session (review E3,
        // same failure family as the empty-call_id case in AGENTS.md §8b).
        // Because ALL emission happens here at stream end, dropping the
        // entry means the client never sees any of its events.
        let live_tool_calls: Vec<(&usize, &(String, String, String))> = self
            .tool_calls
            .iter()
            .filter(|(_, (_, name, _))| !name.is_empty())
            .collect();
        let base_index = self.next_output_index;
        for (rel_idx, (_, (id, name, args))) in live_tool_calls.into_iter().enumerate() {
            let output_index = base_index + rel_idx;
            let fc_item_id = format!("fc_{}", uuid::Uuid::new_v4().simple());
            // id is never empty: on_chunk synthesizes a call_<uuid> fallback
            // when the upstream omits it.
            let call_id = id.as_str();
            let (namespace, decoded_name) =
                decode_tool_call_name(name.as_str(), &self.namespace_tools);
            let tool_name = decoded_name.as_str();
            let arguments = if args.trim().is_empty() {
                "{}"
            } else {
                args.as_str()
            };

            // added - namespace field injected when the name decoded to one.
            let mut added_item = json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": {
                    "type": "function_call",
                    "id": &fc_item_id,
                    "call_id": call_id,
                    "name": tool_name,
                    "arguments": "",
                    "status": "in_progress"
                }
            });
            if let Some(ns) = &namespace {
                added_item["item"]["namespace"] = Value::String(ns.clone());
            }
            events.push(sse_event("response.output_item.added", added_item));

            // delta (full accumulated arguments in one shot)
            events.push(sse_event(
                "response.function_call_arguments.delta",
                json!({
                    "type": "response.function_call_arguments.delta",
                    "item_id": &fc_item_id,
                    "output_index": output_index,
                    "delta": arguments
                }),
            ));

            // done — full-shape item JSON used for both the event and the
            // completed output array. The namespace field is injected when
            // the name decoded to one, so the completed output array matches
            // the added/done events.
            let mut fc_done_item = json!({
                "type": "function_call",
                "id": &fc_item_id,
                "call_id": call_id,
                "name": tool_name,
                "arguments": arguments,
                "status": "completed"
            });
            if let Some(ns) = &namespace {
                fc_done_item["namespace"] = Value::String(ns.clone());
            }
            events.push(sse_event(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": output_index,
                    "item": fc_done_item.clone()
                }),
            ));
            completed_output.push(fc_done_item);

            self.acc.items.push(function_call_item(
                call_id,
                namespace.as_deref(),
                tool_name,
                arguments,
            ));
        }

        // Usage: acc.usage already carries whatever the upstream sent, on the
        // finish chunk or the trailing usage-only chunk (see on_chunk).
        let (input_tokens, output_tokens) = self
            .acc
            .usage
            .as_ref()
            .map(|u| (u.prompt_tokens, u.completion_tokens))
            .unwrap_or((0, 0));

        // response.completed with type + response wrapper. The "output" array
        // uses the same full-shape items as the output_item.done events.
        events.push(sse_event(
            "response.completed",
            json!({
                "type": "response.completed",
                "response": {
                    "id": &self.response_id,
                    "status": "completed",
                    "model": &self.model,
                    "output": &completed_output,
                    "usage": {
                        "input_tokens": input_tokens,
                        "output_tokens": output_tokens,
                        "total_tokens": u64::from(input_tokens) + u64::from(output_tokens)
                    }
                }
            }),
        ));

        events
    }

    /// Generate error + response.failed events when upstream fails mid-stream.
    ///
    /// Emits a Responses-shaped `error` event (carrying `response_id` for
    /// client correlation) followed by `response.failed`, which terminates the
    /// stream (spec §7.3). Takes `&self` because no state is mutated.
    pub fn on_error(&self, message: &str) -> Vec<(String, String)> {
        vec![
            sse_event(
                "error",
                json!({
                    "type": "error",
                    "message": message,
                    "response_id": &self.response_id
                }),
            ),
            sse_event(
                "response.failed",
                json!({
                    "type": "response.failed",
                    "response": {
                        "id": &self.response_id,
                        "status": "failed"
                    }
                }),
            ),
        ]
    }
}

/// Format a Responses SSE event as an (event_type, data) pair.
///
/// Returns `(event_type, JSON-stringified data)`; the caller renders each
/// pair as `event: <type>\ndata: <data>\n\n`. The data is `Value::to_string()`
/// (compact JSON), which is exactly what the Responses wire format expects.
fn sse_event(event_type: &str, data: Value) -> (String, String) {
    (event_type.to_string(), data.to_string())
}

#[cfg(test)]
mod stream_tests;

#[cfg(test)]
mod tests;
