//! Wire protocol types.
//!
//! Serde (de)serialization types for the two wire protocols the proxy speaks:
//!
//! - [`responses`]: OpenAI **Responses API** types, used **inbound** from Codex
//!   CLI (`POST /v1/responses`). [`responses::ResponsesRequest`] is also
//!   re-serialized verbatim when forwarding to a `format = "responses"`
//!   upstream (passthrough mode), which is why it derives `Serialize` too.
//! - [`chat`]: OpenAI **Chat Completions** types, used **outbound to** and
//!   **inbound from** `format = "chat"` upstreams (request JSON body +
//!   streaming SSE `data:` payloads).
//!
//! Serde conventions used across these types:
//!
//! - `#[serde(flatten)] extra` on [`responses::ResponsesRequest`] captures
//!   unknown fields so they survive the passthrough round-trip and can be
//!   extracted during chat conversion (e.g. `stop`, `seed`, `user`,
//!   `tool_choice`, `presence_penalty`, `frequency_penalty`).
//! - `#[serde(untagged)]` on [`responses::ResponsesInput`] lets the `input`
//!   field be either a plain string or an array of item objects.
//! - `#[serde(default)]` on inbound types tolerates fields that clients or
//!   upstreams may omit entirely.
//! - `#[serde(skip_serializing_if = "Option::is_none")]` on outbound
//!   [`chat::ChatRequest`] fields omits absent optional values, because many
//!   upstreams reject explicit `null`s for optional parameters.

pub mod chat;
pub mod responses;
