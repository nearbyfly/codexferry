//! Protocol conversion between the Responses and Chat Completions APIs.
//!
//! This module holds the two conversion directions:
//!
//! - [`request`]: **Responses → Chat** (spec §7.1). Turns an inbound
//!   [`ResponsesRequest`](crate::wire::responses::ResponsesRequest) from Codex
//!   CLI — plus session history from the `SessionStore` — into an outbound
//!   [`ChatRequest`](crate::wire::chat::ChatRequest) for a
//!   `format = "chat"` upstream.
//! - [`response`]: **Chat → Responses** (spec §7.2 and §7.3). Turns an
//!   upstream chat response — non-streaming JSON or streaming SSE — into
//!   Responses-format output items and SSE events for Codex CLI. This is where
//!   the stateful [`StreamConverter`](response::StreamConverter) (token-by-
//!   token SSE transcoding) lives.
//!
//! Reasoning round-tripping across turns (spec §7.4) spans both modules:
//! `request` attaches stored reasoning summaries to assistant messages as
//! `reasoning_content`, and `response` emits `reasoning` items from it.
//!
//! Tool-dialect normalization lives in `crate::normalize` (moved 2026-08-18).

pub mod request;
pub mod response;
