//! Responses → Chat Completions request conversion (spec §7.1).
//!
//! Turns a `ResponsesRequest` from Codex CLI into a `ChatRequest` for a
//! Chat-Completions-only upstream. The conversion pipeline is:
//!
//! 1. **History merge** — prior conversation items from `SessionStore`
//!    (`history`, Responses format) are converted to Chat messages first, so
//!    the stateless Chat upstream sees the full multi-turn context on every
//!    call (§8.3).
//! 2. **System / instructions** — `instructions` (or `system`) becomes a
//!    leading `system` message, merged onto an existing one if present.
//! 3. **Input items** — the new `input` (a plain string or an item array) is
//!    appended after the history.
//! 4. **Tools** — Responses `function` / `custom` / `freeform` tools are
//!    converted to Chat `function` tools (apply_patch compatible).
//! 5. **Params** — Responses-only parameters are renamed (`max_output_tokens`
//!    → `max_tokens`) or passed through; fields with no Chat equivalent are
//!    dropped.
//!
//! Reasoning round-trip (spec §7.4): standalone `reasoning` items in the
//! history are consumed and re-attached to the following assistant message as
//! `reasoning_content`, so reasoning models (DeepSeek-R1, Kimi k2, GLM) keep
//! their chain-of-thought across cross-turn, cross-provider replay.
//!
//! Dropped fields (Responses-only, no Chat equivalent): `store`, `metadata`,
//! `include`, `parallel_tool_calls`, `text`, plus any unknown fields
//! captured loosely in `extra` (`reasoning.effort` is forwarded verbatim as
//! `reasoning_effort`). `previous_response_id` is handled by the session
//! layer (§8) and is never forwarded upstream.
use crate::wire::chat::*;
use crate::wire::responses::*;
use serde_json::{json, Value};

/// Convert a Responses request to a Chat-Completions request.
///
/// Convenience wrapper over [`to_chat_request_with_ns_map`] that discards the
/// namespace decode map. Test-only since proxy.rs switched to the map-returning
/// variant; keep this gate so the bin build stays warning-free.
#[cfg(test)]
pub fn to_chat_request(
    req: &ResponsesRequest,
    history: &[Value],
    upstream_model: &str,
    glm_thinking_quirk_on: bool,
) -> ChatRequest {
    to_chat_request_with_ns_map(req, history, upstream_model, glm_thinking_quirk_on).0
}

/// Like `to_chat_request`, additionally returning the `NamespaceToolMap`
/// the caller must hand to the response converter so namespaced tool_calls
/// are decoded back to a Responses `namespace` field (spec §7).
///
/// # Arguments
/// - `req`: the inbound Responses request from Codex CLI.
/// - `history`: prior conversation items (Responses format) resolved from
///   `SessionStore` via `previous_response_id`; converted first so the
///   stateless Chat upstream receives the complete conversation (§8.3).
/// - `upstream_model`: the route's real upstream model name (the `model`
///   field of the matched route entry), replacing the `provider/alias` key.
/// - `glm_thinking_quirk_on`: the GLM thinking quirk gate, read from config
///   (`ValidatedConfig::quirk_enabled("glm_thinking")`) once per request.
///   When `true`, GLM/Zhipu upstream models receive the explicit
///   `thinking` switch so they emit `reasoning_content` (see
///   [`crate::wire::chat::ChatThinking`]); non-GLM models never receive
///   the field.
///
/// Steps: (1) history → messages, (2) system/instructions, (3) new input,
/// (4) tools, (5) stream options, then parameter mapping.
pub fn to_chat_request_with_ns_map(
    req: &ResponsesRequest,
    history: &[Value],
    upstream_model: &str,
    glm_thinking_quirk_on: bool,
) -> (ChatRequest, crate::normalize::NamespaceToolMap) {
    let mut messages: Vec<ChatMessage> = Vec::new();

    // 1. Convert history items (Responses format -> Chat messages),
    //    merging same-turn assistant text + tool_calls (see
    //    [`push_item_messages`]).
    push_item_messages(&mut messages, history);

    // 2. System / instructions.
    //    `instructions` and `system` are the two Responses ways of supplying
    //    a system prompt; whichever is set and non-empty wins. The system
    //    message is inserted at index 0, but only when there is not already a
    //    system message (e.g. one replayed from history), so the upstream
    //    receives exactly one system message.
    let system_text = req
        .instructions
        .as_ref()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| req.system.as_ref().filter(|s| !s.trim().is_empty()));
    if let Some(sys) = system_text {
        if messages.is_empty() || messages[0].role != "system" {
            messages.insert(
                0,
                ChatMessage {
                    role: "system".into(),
                    content: Some(Value::String(sys.clone())),
                    reasoning_content: None,
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                },
            );
        }
    }

    // 3. Convert new input items.
    //    A plain string `input` becomes a single user message. An item array
    //    goes through the SAME merge loop as history: Codex CLI with
    //    store=false replays its full transcript inline in `input` (no
    //    previous_response_id), so the same-turn assistant text +
    //    function_call merge must apply here too or the upstream sees the
    //    dangling assistant message sequence the merge exists to prevent.
    match &req.input {
        ResponsesInput::Text(text) => {
            messages.push(ChatMessage {
                role: "user".into(),
                content: Some(Value::String(text.clone())),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            });
        }
        ResponsesInput::Items(items) => {
            push_item_messages(&mut messages, items);
        }
    }

    // 4. Convert tools.
    //    Responses `function` / `custom` / `freeform` tools become Chat
    //    `function` tools; the original JSON tool objects are preserved, just
    //    re-wrapped in the Chat `function` shape.
    //    Codex >= 0.147 sends its tools as `additional_tools` INPUT items
    //    (namespace-wrapped) with an empty top-level `tools` array - hoist
    //    them (from the new input AND replayed history, name-deduplicated)
    //    or the chat upstream would bind no tools at all.
    //    normalize_chat_tools then flattens top-level `namespace` entries
    //    (Chat upstreams cannot bind them) and drops unmappable tool types
    //    with a logged counter.
    let mut all_tools = req.tools.clone();
    // The hoist drops unmappable tools (top-level or namespace-inner) and
    // skips same-named definitions with a different schema; merge the
    // reports from both sources and surface them ONCE below (spec §4).
    let mut hoist_unmappable: Vec<String> = Vec::new();
    let mut hoist_schema_conflicts: Vec<String> = Vec::new();
    let mut ns_map = crate::normalize::NamespaceToolMap::new();
    {
        let (u, c) = crate::normalize::collect_additional_tools_reporting(
            history,
            &mut all_tools,
            Some(&mut ns_map),
        );
        hoist_unmappable.extend(u);
        hoist_schema_conflicts.extend(c);
    }
    if let ResponsesInput::Items(items) = &req.input {
        let (u, c) = crate::normalize::collect_additional_tools_reporting(
            items,
            &mut all_tools,
            Some(&mut ns_map),
        );
        hoist_unmappable.extend(u);
        hoist_schema_conflicts.extend(c);
    }
    let (flat_tools, top_ns_map) = crate::normalize::normalize_chat_tools(&all_tools);
    // Merge the top-level decode map last; `or_insert` keeps whichever
    // entry was recorded first. A collision means both sources produced the
    // same encoded string — in practice the same `{namespace}-{name}` tool —
    // so either order decodes identically.
    for (k, v) in top_ns_map {
        ns_map.entry(k).or_insert(v);
    }
    let tools = convert_tools(&flat_tools);

    // Unknown input item types have no Chat mapping and are skipped by
    // `push_item_messages` (above); surface them once per request (history +
    // new input) per the visibility principle. `additional_tools` is
    // EXCLUDED: it is a known dialect consumed by the hoist above (not an
    // unknown type), and the responses path likewise never counts it (it
    // strips before warn) — counting it here would emit expected WARN noise
    // on every tool-using chat request and drown out genuinely new dialects.
    {
        let mut all_items: Vec<&Value> = history.iter().collect();
        if let ResponsesInput::Items(items) = &req.input {
            all_items.extend(items.iter());
        }
        crate::normalize::warn_unknown_item_types(
            all_items
                .into_iter()
                .filter(|i| i.get("type").and_then(Value::as_str) != Some("additional_tools")),
            "dropped",
        );
    }
    // Unmappable tools dropped by the `additional_tools` hoist, and
    // same-named tool definitions skipped with a different schema, are
    // visible via one warn each per request (never silently swallowed).
    if !hoist_unmappable.is_empty() {
        crate::normalize::warn_dropped_tool_types(
            "tool type(s) not mappable to chat dropped from additional_tools",
            &hoist_unmappable,
        );
    }
    if !hoist_schema_conflicts.is_empty() {
        crate::normalize::warn_dropped_tool_types(
            "same-named tool definition with a different schema dropped",
            &hoist_schema_conflicts,
        );
    }

    // 5. Stream options (include usage when streaming).
    //    `stream_options.include_usage = true` makes the upstream's final
    //    streaming chunk carry token usage, which we need for the
    //    `response.completed` event and for session accounting.
    let stream_options = if req.stream {
        Some(StreamOptions {
            include_usage: true,
        })
    } else {
        None
    };

    (
        ChatRequest {
            model: upstream_model.to_string(),
            messages,
            tools,
            // Passthrough tool_choice from the Responses request (spec §7.1: tool_choice passthrough).
            // ResponsesRequest has no dedicated field, so it arrives in `extra`.
            tool_choice: req.extra.get("tool_choice").cloned(),
            temperature: req.temperature,
            // Rename Responses `max_output_tokens` to Chat `max_tokens` (spec §7.1).
            max_tokens: req.max_output_tokens,
            top_p: req.top_p,
            // The remaining passthrough fields have no dedicated ResponsesRequest
            // field either; they are pulled from the loosely-captured `extra` map
            // (see `wire/responses.rs`). `stop` may be a string or an array, so it
            // is forwarded as an untyped JSON `Value`.
            stop: req.extra.get("stop").cloned(),
            presence_penalty: req.extra.get("presence_penalty").and_then(|v| v.as_f64()),
            frequency_penalty: req.extra.get("frequency_penalty").and_then(|v| v.as_f64()),
            seed: req.extra.get("seed").and_then(|v| v.as_u64()),
            user: req
                .extra
                .get("user")
                .and_then(|v| v.as_str())
                .map(String::from),
            // Effort passthrough (spec §1): verbatim, no validation — the
            // accepted set is the upstream's business.
            reasoning_effort: req
                .reasoning
                .as_ref()
                .and_then(|r| r.get("effort"))
                .and_then(Value::as_str)
                .map(str::to_string),
            // Quirk `glm_thinking`: GLM/Zhipu only emit reasoning_content when
            // the thinking switch is explicitly present — their auto-thinking
            // is suppressed by heavy agent system prompts. Non-GLM models think
            // by default and must not receive the field.
            thinking: (glm_thinking_quirk_on && crate::quirks::is_glm_like_model(upstream_model))
                .then(crate::wire::chat::ChatThinking::enabled),
            stream_options,
            stream: req.stream,
        },
        ns_map,
    )
}

/// Convert Responses-format items to Chat messages, appending to `messages`.
///
/// The one loop shared by both item sources:
/// - **history** (step 1): prior turns resolved from `SessionStore` via
///   `previous_response_id`;
/// - **`input` items array** (step 3): the new turn's items - which Codex
///   CLI populates with its full transcript when running store=false, so it
///   needs exactly the same conversion semantics.
///
/// Merge rules (spec §7.1 / §7.4):
/// - Consecutive assistant `message` + `function_call` items of the same
///   turn are merged into ONE Chat message, because Chat Completions
///   requires a single assistant message to carry both content and
///   tool_calls. Without merging, a stateless Chat upstream sees a dangling
///   assistant message immediately followed by another, breaking multi-turn
///   conversations with tool use.
/// - A `function_call` with no preceding assistant message (tool-only turn)
///   starts its own assistant message and absorbs following function_calls.
/// - Standalone `reasoning` items buffer their summary and attach to the
///   NEXT assistant message as `reasoning_content`, so reasoning survives
///   replay. A `reasoning_content` field carried on the message item itself
///   is the fallback when no standalone item precedes it.
/// - Other item types (`function_call_output`, ...) convert via
///   [`convert_responses_item_to_chat`]; unknown types are skipped.
fn push_item_messages(messages: &mut Vec<ChatMessage>, history: &[Value]) {
    let mut pending_reasoning: Option<String> = None;
    let mut i = 0;
    while i < history.len() {
        let item = &history[i];
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");

        if item_type == "reasoning" {
            // Buffer reasoning summary; attach to next assistant message.
            // An empty/missing summary does NOT clear a previously buffered
            // one and does not shadow the message item's own
            // `reasoning_content` below (Some("") is truthy for Option::or,
            // which would replay reasoning_content "" instead of real text).
            let reasoning_text = item
                .get("summary")
                .and_then(|s| s.as_array())
                .and_then(|arr| arr.first())
                .and_then(|t| t.get("text"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from);
            if reasoning_text.is_some() {
                pending_reasoning = reasoning_text;
            }
            i += 1;
            continue;
        }

        if item_type == "message" {
            let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            let chat_role = match role {
                "developer" | "system" => "system",
                "assistant" => "assistant",
                _ => "user",
            };

            if chat_role == "assistant" {
                // Convert the message, then look ahead for consecutive
                // function_call items to merge into the same assistant message.
                let content = item.get("content").and_then(convert_content);
                // The item itself may carry `reasoning_content` (as
                // `convert_responses_item_to_chat` reads it); a preceding
                // standalone reasoning item takes precedence, matching the
                // pre-merge behavior (spec §7.4).
                let item_reasoning = item
                    .get("reasoning_content")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let mut msg = ChatMessage {
                    role: "assistant".into(),
                    content,
                    reasoning_content: pending_reasoning.take().or(item_reasoning),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                };
                // Absorb following function_call items.
                while i + 1 < history.len() && is_function_call(&history[i + 1]) {
                    msg.tool_calls
                        .get_or_insert_with(Vec::new)
                        .push(function_call_to_tool_call(&history[i + 1]));
                    i += 1;
                }
                messages.push(msg);
            } else {
                // User/system message: convert content normally.
                if let Some(msg) = convert_responses_item_to_chat(item) {
                    messages.push(msg);
                }
            }
            i += 1;
            continue;
        }

        if item_type == "function_call" {
            // Tool call without a preceding assistant message (tool-only turn).
            // Create an assistant message and absorb consecutive function_calls.
            let mut msg = ChatMessage {
                role: "assistant".into(),
                content: None,
                reasoning_content: pending_reasoning.take(),
                tool_calls: Some(vec![function_call_to_tool_call(item)]),
                tool_call_id: None,
                name: None,
            };
            // Absorb following function_call items.
            while i + 1 < history.len() && is_function_call(&history[i + 1]) {
                msg.tool_calls
                    .get_or_insert_with(Vec::new)
                    .push(function_call_to_tool_call(&history[i + 1]));
                i += 1;
            }
            messages.push(msg);
            i += 1;
            continue;
        }

        // Other item types (function_call_output, etc.)
        if let Some(msg) = convert_responses_item_to_chat(item) {
            messages.push(msg);
        }
        i += 1;
    }
}

/// Whether a Responses history/input item is a `function_call`.
fn is_function_call(item: &Value) -> bool {
    item.get("type").and_then(|v| v.as_str()) == Some("function_call")
}

/// Build a Chat tool_call object from a Responses `function_call` item:
/// `{"id", "type": "function", "function": {"name", "arguments"}}`.
///
/// Missing `call_id` / `name` / `arguments` fall back to `""` / `""` /
/// `"{}"` (spec §7.1). A `namespace` field makes the Chat name the encoded
/// `{namespace}-{name}` form (`multi_agent_v1-spawn_agent`), the same name
/// the request side bound for a namespaced tool, so a stored function_call
/// replays against the tool the model actually called (spec §7); without a
/// namespace the flat name is used unchanged. Used by every path that
/// converts a function_call item: the history merge loop (both absorb sites
/// and the tool-only turn) and `convert_responses_item_to_chat` for new
/// input items.
fn function_call_to_tool_call(fc: &Value) -> Value {
    let call_id = fc.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
    let namespace = fc.get("namespace").and_then(|v| v.as_str()).unwrap_or("");
    let name = fc.get("name").and_then(|v| v.as_str()).unwrap_or("");
    // Namespaced function_calls (stored by the response converter after this
    // fix) replay with the same `{namespace}-{name}` Chat name the request
    // side encoded, keeping the round-trip consistent (spec §7).
    let chat_name = if namespace.is_empty() {
        name.to_string()
    } else {
        crate::normalize::chat_function_name_for_namespace_tool(namespace, name)
    };
    let arguments = fc.get("arguments").and_then(|v| v.as_str()).unwrap_or("{}");
    json!({
        "id": call_id,
        "type": "function",
        "function": { "name": chat_name, "arguments": arguments }
    })
}

/// Convert a single Responses input item (JSON Value) to a Chat message.
///
/// Handled item types:
/// - `message`: mapped to a Chat message. Roles `developer` / `system` become
///   `system` (Responses `developer` has no Chat equivalent), `assistant`
///   stays `assistant`, anything else falls back to `user`. Content is
///   converted via [`convert_content`]; a `reasoning_content` field carried
///   on the item (as stored by the response converter) is preserved.
/// - `function_call`: replayed as an assistant message with one `tool_calls`
///   entry, so a prior tool invocation stays visible to the model during
///   multi-turn history replay.
/// - `function_call_output`: becomes a `tool` message tied to the originating
///   call via `tool_call_id`.
/// - `reasoning`: produces no message here — `to_chat_request` extracts its
///   summary and attaches it to the next assistant message (§7.4).
/// - anything else: skipped (`None`).
fn convert_responses_item_to_chat(item: &Value) -> Option<ChatMessage> {
    let item_type = item.get("type")?.as_str()?;
    match item_type {
        "message" => {
            // Responses `message` item: map role, convert content parts.
            let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            let chat_role = match role {
                "developer" | "system" => "system",
                "assistant" => "assistant",
                _ => "user",
            };
            let content = item.get("content").and_then(convert_content);
            let reasoning = item
                .get("reasoning_content")
                .and_then(|v| v.as_str())
                .map(String::from);
            Some(ChatMessage {
                role: chat_role.into(),
                content,
                reasoning_content: reasoning,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            })
        }
        "function_call" => {
            // Replaying a previous assistant tool call
            Some(ChatMessage {
                role: "assistant".into(),
                content: None,
                reasoning_content: None,
                tool_calls: Some(vec![function_call_to_tool_call(item)]),
                tool_call_id: None,
                name: None,
            })
        }
        "function_call_output" => {
            // The result of a prior tool call: a Chat `tool` message that
            // references the originating call via `tool_call_id`.
            let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
            let output = item.get("output").and_then(|v| v.as_str()).unwrap_or("");
            Some(ChatMessage {
                role: "tool".into(),
                content: Some(Value::String(output.into())),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: Some(call_id.into()),
                name: None,
            })
        }
        "reasoning" => {
            // Standalone reasoning items produce no chat message here. `to_chat_request`
            // extracts their summary text and attaches it to the next assistant message
            // as `reasoning_content` (spec §7.4), which is how reasoning survives replay.
            None
        }
        _ => None,
    }
}

/// Convert Responses content to Chat content (string or parts array).
///
/// - String content passes through unchanged.
/// - Array content is mapped part by part:
///   - `input_text` / `output_text` → `{"type":"text","text":...}`
///   - `input_image` → `{"type":"image_url","image_url":{"url":...}}`
///     (the URL is read from either the `image_url` or `image` field).
///   - unknown part types are dropped.
/// - Arrays with no convertible parts yield `None` (no content at all).
/// - Collapsing (spec §7.1): an array with exactly one converted *text* part
///   is collapsed to a plain JSON string rather than a one-element array,
///   because Chat upstreams expect pure-text content as a string. A single
///   *image* part — or any multi-part content — stays an array, since an image
///   cannot be represented as a plain string.
fn convert_content(content: &Value) -> Option<Value> {
    match content {
        Value::String(s) => Some(Value::String(s.clone())),
        Value::Array(parts) => {
            let chat_parts: Vec<Value> = parts
                .iter()
                .filter_map(|p| {
                    let ptype = p.get("type")?.as_str()?;
                    match ptype {
                        "input_text" | "output_text" => Some(json!({
                            "type": "text",
                            "text": p.get("text").and_then(|v| v.as_str()).unwrap_or("")
                        })),
                        "input_image" => {
                            let url = p
                                .get("image_url")
                                .and_then(|v| v.as_str())
                                .or_else(|| p.get("image").and_then(|v| v.as_str()));
                            url.map(|u| json!({ "type": "image_url", "image_url": { "url": u } }))
                        }
                        _ => None,
                    }
                })
                .collect();
            if chat_parts.is_empty() {
                None
            } else if chat_parts.len() == 1 {
                // Spec §7.1: pure-text content is represented as a plain string.
                match chat_parts[0].get("text").and_then(|v| v.as_str()) {
                    Some(t) => Some(Value::String(t.to_string())),
                    None => Some(Value::Array(chat_parts)),
                }
            } else {
                Some(Value::Array(chat_parts))
            }
        }
        _ => None,
    }
}

/// Convert Responses tools to Chat tools.
///
/// - `type = "function"`: re-wrapped verbatim into the Chat `function` shape
///   (`name`, `description`, `parameters`).
/// - `type = "custom"` / `type = "freeform"` (Codex's apply_patch and other
///   freeform tools): converted to a `function` tool whose parameters are a
///   fixed `{input: string}` schema, so Chat-only upstreams can accept them
///   (spec §7.1; same shape as deepseek-responses-proxy). The tool's own
///   `input_schema` is deliberately ignored — the generic schema works for
///   all freeform tools.
/// - anything else: skipped (`None`).
fn convert_tools(tools: &[Value]) -> Vec<Value> {
    tools.iter().filter_map(|tool| {
        let ttype = tool.get("type")?.as_str()?;
        match ttype {
            "function" => {
                let name = tool.get("name").and_then(|v| v.as_str())?;
                let description = tool.get("description").and_then(|v| v.as_str()).unwrap_or("");
                let parameters = tool.get("parameters").cloned().unwrap_or(json!({}));
                Some(json!({
                    "type": "function",
                    "function": { "name": name, "description": description, "parameters": parameters }
                }))
            }
            "custom" | "freeform" => {
                let name = tool.get("name").and_then(|v| v.as_str())?;
                let description = tool.get("description").and_then(|v| v.as_str()).unwrap_or("");
                Some(json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": description,
                        "parameters": {
                            "type": "object",
                            "properties": { "input": { "type": "string" } },
                            "required": ["input"]
                        }
                    }
                }))
            }
            _ => None,
        }
    }).collect()
}

#[cfg(test)]
mod tests;
