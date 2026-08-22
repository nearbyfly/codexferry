//! Chat Completions wire types (outbound to / inbound from chat upstreams).
//!
//! Split into two groups by data direction:
//!
//! - **Outbound** (derive `Serialize` only): [`ChatRequest`], [`StreamOptions`],
//!   [`ChatThinking`], and [`ChatMessage`] form the request body sent to a
//!   `format = "chat"` upstream (built by
//!   `convert::request::to_chat_request`).
//! - **Inbound** (derive `Deserialize`): [`ChatResponse`], [`ChatChoice`],
//!   [`ChatUsage`], [`ChatStreamChunk`], [`ChatStreamChoice`], [`ChatDelta`],
//!   [`DeltaToolCall`], and [`DeltaFunction`] parse the upstream's
//!   non-streaming JSON body and streaming SSE `data:` payloads.
//! - [`ChatMessage`] is the exception: it derives both `Serialize` and
//!   `Deserialize` because the non-streaming path reuses it for the outgoing
//!   request *and* the incoming response.
//!
//! Serde conventions:
//!
//! - Outbound `Option` fields use `#[serde(skip_serializing_if = "Option::is_none")]`
//!   so absent values are omitted from the JSON entirely — many upstreams
//!   reject explicit `null`s for optional parameters. [`ChatMessage::content`]
//!   is the deliberate exception (see its doc comment).
//! - Inbound fields use `#[serde(default)]` so upstreams that omit optional
//!   fields (or omit them on particular stream chunks) still deserialize.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Outbound request to upstream (Chat Completions API).
///
/// Built by `convert::request::to_chat_request` from a
/// [`ResponsesRequest`](crate::wire::responses::ResponsesRequest) plus session
/// history, then serialized as the upstream request body. All optional fields
/// are `#[serde(skip_serializing_if = "Option::is_none")]` (and `tools` skips
/// when empty) so unset parameters never appear as `null`.
#[derive(Debug, Serialize)]
pub struct ChatRequest {
    /// The upstream's real model name (from the route's `model` field).
    pub model: String,
    /// Converted conversation: history + instructions/system + new input.
    pub messages: Vec<ChatMessage>,
    /// Converted tool declarations (function/custom/freeform → `function`).
    /// `#[serde(skip_serializing_if = "Vec::is_empty")]`: omitted entirely
    /// when the request declares no tools.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Value>,
    /// `tool_choice` passthrough from the Responses request's `extra`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    /// Sampling temperature passthrough.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// `max_output_tokens` renamed to Chat's `max_tokens`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Nucleus-sampling passthrough.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// `stop` passthrough (from `extra`); may be a string or an array.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Value>,
    /// `presence_penalty` passthrough (from `extra`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    /// `frequency_penalty` passthrough (from `extra`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    /// `seed` passthrough (from `extra`) for deterministic sampling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    /// `user` passthrough (from `extra`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Reasoning effort, forwarded verbatim from the Responses request's
    /// `reasoning.effort`. Deliberately a free-form String rather than an
    /// enum: the accepted set is the upstream's business, not the router's,
    /// and it differs per provider. An unsupported value surfaces as the
    /// upstream's own error instead of being silently rewritten here. Use a
    /// provider's `drop_params` to strip it for an upstream that rejects
    /// unknown fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// GLM thinking switch; `None` for every non-GLM upstream (quirk
    /// `glm_thinking`, see [`ChatThinking`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ChatThinking>,
    /// Requests `include_usage: true` so the final SSE chunk carries token
    /// counts. Only set when streaming.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    /// Mirrors the client's `stream` flag (Codex always sends `true`).
    pub stream: bool,
}

/// Extra options sent alongside `stream: true`. Asks the upstream to include a
/// `usage` object in the terminal chunk, which the proxy reports in
/// `response.completed` and uses for the per-request log line.
#[derive(Debug, Serialize)]
pub struct StreamOptions {
    /// Emit token usage in the final stream chunk.
    pub include_usage: bool,
}

/// GLM/Zhipu thinking switch (quirk `glm_thinking`): explicitly enables
/// thinking so `reasoning_content` is emitted. GLM's auto-thinking is
/// suppressed by heavy agent system prompts (e.g. Codex's), so the switch
/// must be explicit (codex-relay issue #26). Only ever constructed with
/// `kind: "enabled"`; other models never receive the field at all.
#[derive(Debug, Serialize)]
pub struct ChatThinking {
    /// Serialized as `"type"`.
    #[serde(rename = "type")]
    pub kind: &'static str,
}

impl ChatThinking {
    /// The one value the router ever sends.
    pub fn enabled() -> Self {
        Self { kind: "enabled" }
    }
}

/// A single Chat Completions message.
///
/// Used **both** outbound (request bodies from `to_chat_request`) and inbound
/// (the `message` of a non-streaming [`ChatResponse`]), hence the dual
/// `Serialize`/`Deserialize` derives.
///
/// Note that `content` is the only optional field *without*
/// `skip_serializing_if`: when `None` it serializes as an explicit `null`
/// rather than being omitted. This is deliberate — assistant messages that
/// replay tool calls from history carry `content: None` plus `tool_calls`, and
/// chat upstreams require `content` to be present (even if `null`) on such
/// messages.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChatMessage {
    /// Message role: `system`, `user`, `assistant`, or `tool`.
    pub role: String,
    /// Message payload: a plain string, an array of content parts, or `null`
    /// on assistant tool-call messages (see the struct doc comment above).
    pub content: Option<Value>,
    /// Vendor-specific chain-of-thought field (e.g. DeepSeek). The proxy sets
    /// it on assistant messages when replaying history so reasoning survives
    /// multi-turn conversations (spec §7.4). Omitted when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Tool-call declarations on an `assistant` message. Kept as raw `Value`s;
    /// the converter reads `id` / `function.name` / `function.arguments`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<Value>>,
    /// ID linking a `role: "tool"` message to the `assistant` tool call it
    /// answers. Only present on tool messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Optional message name; some upstreams include the function name on
    /// tool messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ChatMessage {
    /// Return the message text when `content` is a plain JSON string; `""`
    /// for `null` content (tool-call/tool messages) or array content.
    /// Used by the non-streaming response converter, which only handles
    /// plain-text content.
    pub fn text_content(&self) -> &str {
        self.content.as_ref().and_then(|v| v.as_str()).unwrap_or("")
    }
}

/// Non-streaming response from upstream.
///
/// Parsed from the complete JSON body when `stream = false` (the fallback
/// path, also exercised by tests). Only `choices[0]` is converted.
#[derive(Debug, Deserialize)]
pub struct ChatResponse {
    /// Response choices; the proxy converts the first one.
    pub choices: Vec<ChatChoice>,
    /// Token usage. `#[serde(default)]`: some upstreams omit it; `None` means
    /// "unknown" and the proxy reports zero tokens.
    #[serde(default)]
    pub usage: Option<ChatUsage>,
}

/// One completion choice of a non-streaming response.
#[derive(Debug, Deserialize)]
pub struct ChatChoice {
    /// The generated assistant message (text, reasoning, and/or tool calls).
    pub message: ChatMessage,
}

/// Token usage reported by the upstream. All fields default to `0` so that
/// upstreams which omit the whole object or individual counters still parse.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct ChatUsage {
    /// Input tokens consumed by this request.
    #[serde(default)]
    pub prompt_tokens: u32,
    /// Output tokens generated by this request.
    #[serde(default)]
    pub completion_tokens: u32,
    /// `prompt_tokens + completion_tokens`. Deserialized but not currently
    /// read by the proxy, hence `#[allow(dead_code)]`.
    #[serde(default)]
    #[allow(dead_code)]
    pub total_tokens: u32,
}

/// Streaming chunk from upstream.
///
/// Each SSE `data:` payload of a streaming response deserializes into this.
/// The terminal chunk typically carries `finish_reason` and — because the
/// proxy requests `include_usage` — a `usage` object.
#[derive(Debug, Deserialize)]
pub struct ChatStreamChunk {
    /// Stream deltas for this chunk (usually a single choice).
    pub choices: Vec<ChatStreamChoice>,
    /// Token usage on the terminal chunk. `#[serde(default)]`: most chunks
    /// carry none.
    #[serde(default)]
    pub usage: Option<ChatUsage>,
}

/// One choice of a streaming chunk.
#[derive(Debug, Deserialize)]
pub struct ChatStreamChoice {
    /// Choice index. `#[serde(default)]` + `#[allow(dead_code)]`: some
    /// upstreams omit it, and the proxy only handles a single choice.
    #[serde(default)]
    #[allow(dead_code)]
    pub index: usize,
    /// The incremental update for this chunk (the actual payload).
    pub delta: ChatDelta,
    /// Set on the final chunk of a choice (e.g. `"stop"`, `"tool_calls"`,
    /// `"length"`); intermediate chunks typically carry `null`.
    pub finish_reason: Option<String>,
}

/// The incremental update carried by a streaming chunk's `delta` field.
///
/// All fields are `#[serde(default)]`: a given chunk usually carries only one
/// kind of update (role announcement, text, reasoning, or tool-call
/// fragments).
#[derive(Debug, Deserialize, Default)]
pub struct ChatDelta {
    /// Role announcement, present on the first chunk of a response.
    #[serde(default)]
    #[allow(dead_code)]
    pub role: Option<String>,
    /// Text delta for the assistant's content.
    #[serde(default)]
    pub content: Option<String>,
    /// Chain-of-thought delta in the DeepSeek-style `reasoning_content` field
    /// (also used by Kimi, GLM, and other Chinese reasoning models).
    #[serde(default)]
    pub reasoning_content: Option<String>,
    /// Alternative reasoning field used by some vendors under the name
    /// `reasoning` instead of `reasoning_content`. `reasoning_text()`
    /// normalizes the difference, preferring `reasoning_content`.
    #[serde(default)]
    pub reasoning: Option<String>,
    /// Incremental tool-call fragments; `index` groups fragments of the same
    /// call across chunks, and `arguments` fragments are concatenated.
    #[serde(default)]
    pub tool_calls: Option<Vec<DeltaToolCall>>,
}

impl ChatDelta {
    /// Return this chunk's reasoning text regardless of which vendor field
    /// carries it: `reasoning_content` (DeepSeek-style) wins, falling back to
    /// `reasoning` (other vendors).
    pub fn reasoning_text(&self) -> Option<&str> {
        self.reasoning_content
            .as_deref()
            .or(self.reasoning.as_deref())
    }
}

/// A single streaming tool-call fragment.
///
/// Streaming upstreams split each tool call across multiple chunks; fragments
/// are grouped by [`index`](DeltaToolCall::index) in the `StreamConverter`
/// (`convert/response.rs`), which concatenates the `arguments` fragments in
/// ascending index order.
#[derive(Debug, Deserialize, Default)]
pub struct DeltaToolCall {
    /// Groups fragments belonging to the same tool call. `#[serde(default)]`
    /// (→ 0) is essential: upstreams only include `index` on the *first*
    /// fragment of a call, and the converter aggregates by this value — a
    /// missing index must not break deserialization of subsequent fragments.
    #[serde(default)]
    pub index: usize,
    /// Tool-call ID, present only on the first fragment of a call.
    #[serde(default)]
    pub id: Option<String>,
    /// Function name / arguments fragment; `name` appears only on the first
    /// fragment.
    #[serde(default)]
    pub function: Option<DeltaFunction>,
}

/// The `function` part of a streaming tool-call fragment.
#[derive(Debug, Deserialize, Default)]
pub struct DeltaFunction {
    /// Function name; present only on the first fragment of a tool call.
    #[serde(default)]
    pub name: Option<String>,
    /// JSON-string fragment of the call arguments. The converter concatenates
    /// these fragments (in `index` order) into the full arguments string.
    #[serde(default)]
    pub arguments: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_chat_request() {
        let req = ChatRequest {
            model: "deepseek-chat".into(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: Some(Value::String("hi".into())),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            }],
            tools: vec![],
            tool_choice: None,
            temperature: None,
            max_tokens: Some(4096),
            top_p: None,
            stop: None,
            presence_penalty: None,
            frequency_penalty: None,
            seed: None,
            user: None,
            reasoning_effort: None,
            thinking: None,
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
            stream: true,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"max_tokens\":4096"));
        assert!(json.contains("\"stream\":true"));
        assert!(json.contains("\"include_usage\":true"));
        // Fields with None should be skipped
        assert!(!json.contains("\"temperature\""));
        assert!(!json.contains("\"tools\""));
    }

    #[test]
    fn deserialize_stream_chunk() {
        let json = r#"{"choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":null}]}"#;
        let chunk: ChatStreamChunk = serde_json::from_str(json).unwrap();
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("hello"));
    }

    #[test]
    fn reasoning_text_prefers_reasoning_content() {
        let delta = ChatDelta {
            reasoning_content: Some("a".into()),
            reasoning: Some("b".into()),
            ..Default::default()
        };
        assert_eq!(delta.reasoning_text(), Some("a"));
    }
}
