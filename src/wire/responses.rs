//! Inbound Responses API wire types (from Codex CLI).
//!
//! [`ResponsesRequest`] is deserialized from the body of Codex's
//! `POST /v1/responses` call. It also derives [`Serialize`] because the
//! passthrough path (`format = "responses"` upstreams) re-serializes the
//! request — with the model name replaced, history merged, and
//! `previous_response_id` removed — and forwards it verbatim (see
//! `handle_responses_format` in `proxy.rs`).
//!
//! Serde conventions:
//!
//! - Every optional field carries `#[serde(default)]`, so requests that omit
//!   them (Codex does not always send every parameter) still deserialize.
//! - `#[serde(flatten)] extra` captures all unrecognized fields (e.g. `store`,
//!   `metadata`, `stop`, `seed`, `user`, `tool_choice`, `presence_penalty`,
//!   `frequency_penalty`) into a loose map. Chat conversion drops them;
//!   passthrough preserves them.
//! - [`ResponsesInput`] is `#[serde(untagged)]`: `input` may be a plain text
//!   string or an array of item objects.
//!
//! Item objects inside `input` and `tools` are kept as untyped
//! `serde_json::Value`s rather than dedicated structs: the conversion code
//! dispatches on each item's `"type"` field and stays tolerant of vendor
//! variations (spec §7.1).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Inbound request from Codex CLI (Responses API).
///
/// Only `model` and `input` are required; every other field is
/// `#[serde(default)]` so a minimal request still parses.
#[derive(Debug, Deserialize, Serialize)]
pub struct ResponsesRequest {
    /// Route key in `provider/alias` form (split on the *first* `/` only),
    /// used to look up the upstream provider and model in the config.
    pub model: String,
    /// The user's prompt: either a plain text string or an array of input
    /// items (messages, function calls, reasoning, …).
    pub input: ResponsesInput,
    /// ID of a previous response, used to continue a multi-turn conversation.
    /// Only present on follow-up turns (`#[serde(default)]`). The proxy uses
    /// it to look up session history and **strips it** before forwarding — it
    /// is consumed, never sent upstream.
    #[serde(default)]
    pub previous_response_id: Option<String>,
    /// Tool declarations for this request. Kept as raw `Value`s because tools
    /// arrive in several shapes: `function`, `custom`, and `freeform`
    /// (Codex's apply_patch). `#[serde(default)]`: omitted on tool-less
    /// requests.
    #[serde(default)]
    pub tools: Vec<Value>,
    /// Whether the client wants a streaming SSE response. `#[serde(default)]`
    /// makes it default to `false`; Codex always sends `true`.
    #[serde(default)]
    pub stream: bool,
    /// Sampling temperature (0..2), passed through to the upstream.
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Response-token budget; renamed to `max_tokens` for chat upstreams.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// Nucleus-sampling parameter, passed through.
    #[serde(default)]
    pub top_p: Option<f64>,
    /// Developer-level instructions, merged into a `system` message during
    /// chat conversion.
    #[serde(default)]
    pub instructions: Option<String>,
    /// Legacy alias for `instructions`; consulted when `instructions` is empty.
    #[serde(default)]
    pub system: Option<String>,
    /// Responses-only reasoning configuration (e.g. `{"effort": "high"}`).
    /// Ignored during chat conversion — chat upstreams have no equivalent.
    #[serde(default)]
    pub reasoning: Option<Value>,
    // Other fields are captured loosely and dropped during conversion.
    // `#[serde(flatten)]` collects every field not listed above (e.g. `store`,
    // `metadata`, `stop`, `seed`, `user`, `tool_choice`, `presence_penalty`,
    // `frequency_penalty`) into this map, so that (a) the passthrough path can
    // re-emit them verbatim and (b) `to_chat_request` can extract the
    // chat-passthrough parameters from `extra`. Unknown fields not consumed
    // anywhere are silently dropped on chat conversion (spec §7.1).
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// The `input` field of a Responses request.
///
/// `#[serde(untagged)]` — the JSON carries no type tag, so serde tries the
/// variants in order: a JSON string deserializes to [`ResponsesInput::Text`],
/// anything else (an array) to [`ResponsesInput::Items`].
#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ResponsesInput {
    /// `input` as a plain string → converted to a single `user` message.
    Text(String),
    /// `input` as an array of item objects (messages, function calls,
    /// `function_call_output`, reasoning, …). Items are untyped `Value`s and
    /// are dispatched by their `"type"` field during conversion.
    Items(Vec<Value>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_simple_request() {
        let json = r#"{"model":"deepseek/flash","input":"hello","stream":true}"#;
        let req: ResponsesRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.model, "deepseek/flash");
        assert!(matches!(req.input, ResponsesInput::Text(ref t) if t == "hello"));
        assert!(req.stream);
        assert!(req.previous_response_id.is_none());
    }

    #[test]
    fn deserialize_request_with_items() {
        let json = r#"{"model":"ark/glm","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#;
        let req: ResponsesRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(req.input, ResponsesInput::Items(_)));
    }

    #[test]
    fn captures_unknown_fields() {
        let json = r#"{"model":"x","input":"hi","store":true,"metadata":{"k":"v"}}"#;
        let req: ResponsesRequest = serde_json::from_str(json).unwrap();
        assert!(req.extra.contains_key("store"));
        assert!(req.extra.contains_key("metadata"));
    }
}
