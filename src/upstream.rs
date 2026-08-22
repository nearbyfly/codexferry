//! Upstream communication helpers: SSE parsing, API-key resolution, and URL construction.
//!
//! ## Hand-written SSE parser (spec §7.3)
//!
//! [`parse_sse_stream`] is deliberately dependency-free (AGENTS.md #2). It
//! consumes a `reqwest` byte stream and emits [`SseEvent`]s, implementing the
//! SSE subset used by Chat Completions upstreams:
//!
//! - Only `data:` lines are processed; `event:` / `id:` / `retry:` lines and
//!   comment/keepalive lines (`:`) are ignored.
//! - Multiple `data:` lines within one event are joined with `\n` (some
//!   upstreams fragment a single JSON payload across several `data:` lines).
//! - `data: [DONE]` is the end-of-stream sentinel (see [`is_done`]).
//! - Events are delimited by either `\n\n` or `\r\n\r\n`.
//!
//! Raw bytes are buffered and only decoded at complete event boundaries, so a
//! multi-byte UTF-8 character split across chunk boundaries is never corrupted.
//!
//! ## Key resolution & URL helpers
//!
//! [`resolve_api_key`] resolves a provider's credential in the order
//! `api_key` → `api_key_env` → `api_key_file`; [`chat_url`] and
//! [`responses_url`] build upstream endpoints from a provider `base_url`.
//!
use crate::config::ProviderConfig;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};

/// A single parsed SSE event from an upstream stream.
///
/// Only the `data:` payload is retained — `event:`, `id:`, `retry:` and
/// comment lines are dropped by the parser. For Chat Completions upstreams the
/// payload is a JSON object; the `[DONE]` sentinel is surfaced as-is so the
/// caller can detect stream completion.
#[derive(Debug)]
pub struct SseEvent {
    pub data: String,
}

/// Parse a byte stream into SSE events.
/// Handles: data: lines, `[DONE]` sentinel, comment lines, multi-line data.
///
/// Raw bytes are buffered and decoded only at complete event boundaries so that
/// a multi-byte UTF-8 character split across chunk boundaries is not corrupted.
///
/// Implementation: built on `futures_util::stream::unfold` with a persistent
/// byte buffer, so no external SSE crate is required:
///
/// - Each iteration first checks the buffer for a complete event (via
///   [`find_event_boundary`], which accepts both `\n\n` and `\r\n\r\n`
///   delimiters) and emits every buffered event before requesting more data.
/// - Only when no delimiter is present does it await the next chunk and append
///   the raw bytes. Because decoding happens solely at event boundaries, a
///   multi-byte UTF-8 character split across chunk boundaries is reassembled
///   intact.
/// - Comment/keepalive events (for which [`parse_sse_event`] returns `None`)
///   are dropped transparently.
/// - When the underlying stream ends, any trailing non-whitespace bytes are
///   flushed as a final event — the `[DONE]` sentinel frequently arrives
///   without a trailing blank line.
pub fn parse_sse_stream<S>(stream: S) -> impl Stream<Item = SseEvent>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    futures_util::stream::unfold(
        (stream, Vec::<u8>::new()),
        |(mut stream, mut buffer)| async move {
            loop {
                // Check the buffer for a complete event before requesting more
                // data, so already-buffered events are drained eagerly.
                if let Some((offset, delim_len)) = find_event_boundary(&buffer) {
                    // Split off everything up to (and including) the delimiter:
                    // the prefix is this event's text, the tail is kept as the
                    // buffer for the next event.
                    let tail = buffer.split_off(offset + delim_len);
                    let event_str = String::from_utf8_lossy(&buffer[..offset]).into_owned();
                    buffer = tail;
                    let event = parse_sse_event(&event_str);
                    if let Some(evt) = event {
                        return Some((evt, (stream, buffer)));
                    }
                    // Comment or empty, continue
                    continue;
                }
                // No complete event in the buffer yet: wait for the next
                // upstream chunk and append its raw bytes.
                match stream.next().await {
                    Some(Ok(chunk)) => buffer.extend_from_slice(&chunk),
                    // Transport error: log and terminate the SSE stream.
                    Some(Err(e)) => {
                        tracing::warn!("upstream stream error: {e}");
                        return None;
                    }
                    None => {
                        // Stream ended: flush any remaining non-whitespace
                        // bytes as a trailing event (some upstreams omit the
                        // final blank line), then terminate.
                        if buffer.iter().any(|b| !b.is_ascii_whitespace()) {
                            let event_str = String::from_utf8_lossy(&buffer).into_owned();
                            buffer.clear();
                            if let Some(evt) = parse_sse_event(&event_str) {
                                return Some((evt, (stream, buffer)));
                            }
                        }
                        return None;
                    }
                }
            }
        },
    )
}
/// A structure-preserving SSE event from [`split_sse_events`]: the exact
/// original bytes (INCLUDING the trailing `\n\n` / CRLF delimiter, comments
/// and `id:`/`retry:` lines) plus the parsed `event:` name (last one wins,
/// per the SSE spec) and the `data:` lines joined with `\n`.
///
/// Concatenating every `raw` reproduces the input stream byte for byte for
/// all blocks between delimiters — that is the invariant the Phase B healing
/// passthrough relies on: events it does not touch are forwarded verbatim.
/// (The sole exception is a trailing run of pure whitespace with no event,
/// which [`split_sse_events`] drops; see its docs.)
#[derive(Debug)]
pub struct PreservedSseEvent {
    pub raw: Bytes,
    pub event: Option<String>,
    pub data: String,
}

/// Split a byte stream into [`PreservedSseEvent`]s.
///
/// Same buffering discipline as [`parse_sse_stream`] (decode only at event
/// boundaries so a UTF-8 character split across chunks survives), but every
/// event is also carried in its raw form. Trailing bytes without a delimiter
/// are flushed as one final event (same rule as `parse_sse_stream`); comment
/// and blank keepalive blocks are forwarded too (`event: None`, `data: ""`),
/// so concatenating every [`raw`](PreservedSseEvent::raw) reproduces the
/// input stream byte for byte. The only bytes dropped are a trailing run of
/// pure whitespace with no event (the `!is_ascii_whitespace()` check in the
/// end-of-stream arm).
pub fn split_sse_events<S>(stream: S) -> impl futures_util::Stream<Item = PreservedSseEvent>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    futures_util::stream::unfold(
        (stream, Vec::<u8>::new()),
        |(mut stream, mut buffer)| async move {
            // Buffer grows without bound while no delimiter arrives — same
            // inherited discipline as `parse_sse_stream`; do not "fix" it.
            loop {
                if let Some((offset, delim_len)) = find_event_boundary(&buffer) {
                    let raw: Vec<u8> = buffer.drain(..offset + delim_len).collect();
                    return Some((parse_preserved_event(raw), (stream, buffer)));
                }
                match stream.next().await {
                    Some(Ok(chunk)) => buffer.extend_from_slice(&chunk),
                    Some(Err(e)) => {
                        tracing::warn!("upstream stream error: {e}");
                        return None;
                    }
                    None => {
                        if buffer.iter().any(|b| !b.is_ascii_whitespace()) {
                            let raw = std::mem::take(&mut buffer);
                            return Some((parse_preserved_event(raw), (stream, buffer)));
                        }
                        return None;
                    }
                }
            }
        },
    )
}

/// Parse one raw SSE block into a [`PreservedSseEvent`].
///
/// Every block — including comment-only and blank keepalive blocks (`event:
/// None`, `data: ""`) — yields an event, so the stream round-trips byte for
/// byte; the parsed fields are what a healer inspects, not a filter. The
/// `raw` bytes are moved out (zero-copy), not cloned.
fn parse_preserved_event(raw: Vec<u8>) -> PreservedSseEvent {
    let text = String::from_utf8_lossy(&raw);
    let mut event: Option<String> = None;
    let mut data_lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    let data = data_lines.join("\n");
    PreservedSseEvent {
        raw: Bytes::from(raw),
        event,
        data,
    }
}

/// Find the first SSE event delimiter in the buffer.
///
/// Supports both `\n\n` (length 2) and CRLF `\r\n\r\n` (length 4) delimiters,
/// returning whichever occurs first as `(offset, len)`.
///
/// Both delimiter types are located with a windowed byte scan; when both are
/// present, the one whose delimiter starts earliest in the buffer wins. A bare
/// `\n\n` can never match inside a CRLF `\r\n\r\n` pair (the second `\n`
/// would have to follow the first, but there it follows `\r`), so CRLF events
/// are never split on a partial match. `None` means the buffer does not yet
/// contain a complete event.
fn find_event_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    let lf = buffer.windows(2).position(|w| w == b"\n\n").map(|p| (p, 2));
    let crlf = buffer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| (p, 4));
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// Parse one SSE event body (the text up to a delimiter) into an [`SseEvent`].
///
/// Applies the spec §7.3 rules:
/// - `data:` lines: the value after the colon, with at most one leading space
///   stripped (`data:x` and `data: x` are equivalent). Consecutive `data:`
///   lines within one event are joined with `\n`.
/// - Comment/keepalive lines (`:` prefix) and `event:` / `id:` / `retry:`
///   lines are ignored.
/// - Returns `None` when no `data:` line was present (empty or comment-only
///   events), letting the caller skip the event entirely.
fn parse_sse_event(text: &str) -> Option<SseEvent> {
    let mut data_lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        if line.starts_with(':') {
            // Comment / keepalive, skip
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            data_lines.push(rest);
        }
        // Ignore event:, id:, retry: lines
    }
    if data_lines.is_empty() {
        return None;
    }
    let data = data_lines.join("\n");
    Some(SseEvent { data })
}

/// Check if SSE data is the `[DONE]` sentinel.
///
/// Whitespace is trimmed before comparing, so `[DONE]`, ` [DONE] ` and a
/// trailing newline all count as stream end. Comparison is case-sensitive
/// (the upstream always emits uppercase).
pub fn is_done(data: &str) -> bool {
    data.trim() == "[DONE]"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(data: &str) -> Option<String> {
        parse_sse_event(data).map(|e| e.data)
    }

    fn iter_stream(items: Vec<&'static [u8]>) -> impl Stream<Item = Result<Bytes, reqwest::Error>> {
        futures_util::stream::iter(
            items
                .into_iter()
                .map(|b| Ok::<Bytes, reqwest::Error>(Bytes::from_static(b))),
        )
    }

    async fn collect_data<S>(stream: S) -> Vec<String>
    where
        S: Stream<Item = SseEvent>,
    {
        stream.map(|e| e.data).collect::<Vec<_>>().await
    }

    #[test]
    fn single_data_line() {
        assert_eq!(parse("data: hello"), Some("hello".into()));
    }

    #[test]
    fn multi_line_data_joined_with_newline() {
        assert_eq!(parse("data: a\ndata: b"), Some("a\nb".into()));
    }

    #[test]
    fn comment_and_keepalive_lines_skipped() {
        assert_eq!(parse(": keepalive\ndata: x"), Some("x".into()));
        assert_eq!(parse(": keepalive\n: another\ndata: x"), Some("x".into()));
    }

    #[test]
    fn event_id_retry_lines_ignored() {
        let text = "event: message\nid: 1\nretry: 100\ndata: x";
        assert_eq!(parse(text), Some("x".into()));
    }

    #[test]
    fn crlf_line_endings() {
        assert_eq!(parse("data: a\r\ndata: b\r\n"), Some("a\nb".into()));
    }

    #[test]
    fn no_data_lines_returns_none() {
        assert_eq!(parse(""), None);
        assert_eq!(parse(": comment\n\n"), None);
        assert_eq!(parse("event: foo\nid: 1"), None);
    }

    #[test]
    fn data_without_space_after_colon() {
        assert_eq!(parse("data:x"), Some("x".into()));
    }

    #[test]
    fn done_sentinel_detection() {
        assert!(is_done("[DONE]"));
        assert!(is_done("  [DONE]  "));
        assert!(!is_done("hello"));
        assert!(!is_done(""));
        assert!(!is_done("[done]"));
    }

    #[tokio::test]
    async fn single_event_in_one_chunk() {
        let events = collect_data(parse_sse_stream(iter_stream(vec![b"data: hello\n\n"]))).await;
        assert_eq!(events, vec!["hello"]);
    }

    #[tokio::test]
    async fn multiple_events_in_one_chunk() {
        let events = collect_data(parse_sse_stream(iter_stream(vec![
            b"data: a\n\ndata: b\n\n",
        ])))
        .await;
        assert_eq!(events, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn event_split_across_chunks() {
        let events =
            collect_data(parse_sse_stream(iter_stream(vec![b"data: hel", b"lo\n\n"]))).await;
        assert_eq!(events, vec!["hello"]);
    }

    #[tokio::test]
    async fn done_sentinel_passthrough() {
        let events = collect_data(parse_sse_stream(iter_stream(vec![b"data: [DONE]\n\n"]))).await;
        assert_eq!(events, vec!["[DONE]"]);
    }

    #[tokio::test]
    async fn comment_only_events_skipped() {
        let events = collect_data(parse_sse_stream(iter_stream(vec![
            b": keepalive\n\n",
            b"data: x\n\n",
        ])))
        .await;
        assert_eq!(events, vec!["x"]);
    }

    #[tokio::test]
    async fn utf8_char_split_across_chunks_not_corrupted() {
        // The character U+4F60 (E4 BD A0 in UTF-8) split mid-sequence across
        // chunk boundaries.
        let events = collect_data(parse_sse_stream(iter_stream(vec![
            b"data: ",
            b"\xE4",
            b"\xBD\xA0\n\n",
        ])))
        .await;
        assert_eq!(events, vec!["你"]);
    }

    #[tokio::test]
    async fn crlf_delimiter() {
        let events = collect_data(parse_sse_stream(iter_stream(vec![b"data: x\r\n\r\n"]))).await;
        assert_eq!(events, vec!["x"]);
    }

    #[tokio::test]
    async fn crlf_multiple_events_not_merged() {
        // Multiple CRLF-delimited events in one chunk must yield two events,
        // not a single merged one (regression for CRLF delimiter handling).
        let events = collect_data(parse_sse_stream(iter_stream(vec![
            b"data: a\r\n\r\ndata: b\r\n\r\n",
        ])))
        .await;
        assert_eq!(events, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn trailing_event_without_blank_line_flushed() {
        let events = collect_data(parse_sse_stream(iter_stream(vec![b"data: x"]))).await;
        assert_eq!(events, vec!["x"]);
    }
    // ---- structure-preserving splitter (Phase B healing) ----

    async fn preserved_items(items: Vec<&'static [u8]>) -> Vec<PreservedSseEvent> {
        let stream = iter_stream(items);
        futures_util::StreamExt::collect::<Vec<_>>(Box::pin(split_sse_events(stream))).await
    }

    #[tokio::test]
    async fn preserved_splitter_keeps_raw_bytes_verbatim() {
        let input: Vec<&[u8]> = vec![
            b"event: response.created\ndata: {\"a\":1}\n\n",
            b": keepalive\n\n",
            b"event: response.output_text.delta\ndata: {\"delta\":\"\\u4f60\"}\n\n",
            b"data: trailing without blank line",
        ];
        let events = preserved_items(input.clone()).await;
        // Concatenated raw output reproduces the input stream byte for byte
        // (comments included, trailing flush included).
        let joined: Vec<u8> = events.iter().flat_map(|e| e.raw.iter().copied()).collect();
        let flat: Vec<u8> = input.concat();
        assert_eq!(joined, flat);
    }

    #[tokio::test]
    async fn preserved_splitter_parses_event_name_and_joins_data() {
        let events = preserved_items(vec![
            b"event: response.completed\nid: 7\ndata: line1\ndata: line2\n\n",
        ])
        .await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("response.completed"));
        assert_eq!(events[0].data, "line1\nline2");
    }

    #[tokio::test]
    async fn preserved_splitter_forwards_comment_blocks() {
        let events = preserved_items(vec![b": keepalive\n\n", b"event: x\ndata: 1\n\n"]).await;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, None);
        assert_eq!(events[0].data, "");
        assert_eq!(events[0].raw, Bytes::from_static(b": keepalive\n\n"));
        assert_eq!(events[1].event.as_deref(), Some("x"));
    }

    #[tokio::test]
    async fn preserved_splitter_handles_crlf_boundaries() {
        let events = preserved_items(vec![
            b"event: a\r\ndata: 1\r\n\r\nevent: b\r\ndata: 2\r\n\r\n",
        ])
        .await;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.as_deref(), Some("a"));
        assert_eq!(events[1].event.as_deref(), Some("b"));
    }

    #[tokio::test]
    async fn preserved_splitter_event_split_across_chunks() {
        let events = preserved_items(vec![b"data: hel", b"lo\n\n"]).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
        assert_eq!(events[0].raw, Bytes::from_static(b"data: hello\n\n"));
    }

    #[tokio::test]
    async fn preserved_splitter_utf8_char_split_across_chunks_not_corrupted() {
        // The character U+4F60 (E4 BD A0 in UTF-8) split mid-sequence across
        // chunk boundaries.
        let events = preserved_items(vec![b"data: ", b"\xE4", b"\xBD\xA0\n\n"]).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "你");
        assert_eq!(events[0].raw, Bytes::from_static(b"data: \xE4\xBD\xA0\n\n"));
    }
}

/// Resolve API key from provider config: api_key -> api_key_env -> api_key_file.
///
/// Priority (first match wins):
/// 1. `api_key` — plaintext inline key.
/// 2. `api_key_env` — name of an environment variable holding the key; an
///    unset variable is an error.
/// 3. `api_key_file` — path to a file whose contents are read and trimmed.
///
/// Returns `Err` with a human-readable message when nothing is configured or
/// the chosen source fails (used to produce a 500 Responses error upstream).
pub fn resolve_api_key(provider: &ProviderConfig) -> Result<String, String> {
    if let Some(key) = &provider.api_key {
        return Ok(key.clone());
    }
    if let Some(env_var) = &provider.api_key_env {
        return std::env::var(env_var).map_err(|_| format!("env var {env_var} not set"));
    }
    if let Some(path) = &provider.api_key_file {
        return std::fs::read_to_string(path)
            .map(|s| s.trim().to_string())
            .map_err(|e| format!("failed to read key file {path}: {e}"));
    }
    Err("no API key configured".into())
}

/// Build the upstream URL for a chat completions request.
///
/// Appends `/chat/completions` to the provider `base_url`, trimming any
/// trailing `/` first so a base URL ending in `/v1/` still produces
/// `.../v1/chat/completions`.
pub fn chat_url(base_url: &str) -> String {
    format!("{}/chat/completions", base_url.trim_end_matches('/'))
}

/// Build the upstream URL for a responses passthrough request.
///
/// Appends `/responses` to the provider `base_url`, trimming any trailing `/`
/// first (same normalization as [`chat_url`]).
pub fn responses_url(base_url: &str) -> String {
    format!("{}/responses", base_url.trim_end_matches('/'))
}

#[cfg(test)]
mod client_tests {
    use super::*;
    use crate::config::ProviderConfig;

    fn make_provider(
        api_key: Option<&str>,
        env: Option<&str>,
        file: Option<&str>,
    ) -> ProviderConfig {
        ProviderConfig {
            base_url: "https://api.test.com/v1".into(),
            api_key: api_key.map(String::from),
            api_key_env: env.map(String::from),
            api_key_file: file.map(String::from),
            format: "chat".into(),
            timeout_ms: 120_000,
            extra_headers: None,
            extra_params: None,
            drop_params: None,
        }
    }

    #[test]
    fn resolve_inline_key() {
        let p = make_provider(Some("sk-inline"), None, None);
        assert_eq!(resolve_api_key(&p).unwrap(), "sk-inline");
    }

    /// RAII guard that removes an env var on drop, keeping env-mutating
    /// tests panic-safe (and future-edition-proof once set_var is unsafe).
    struct EnvGuard(&'static str);
    impl EnvGuard {
        fn set(name: &'static str, value: &str) -> Self {
            std::env::set_var(name, value);
            EnvGuard(name)
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            std::env::remove_var(self.0);
        }
    }

    #[test]
    fn resolve_env_key() {
        let _guard = EnvGuard::set("TEST_API_KEY_ENV", "sk-env");
        let p = make_provider(None, Some("TEST_API_KEY_ENV"), None);
        assert_eq!(resolve_api_key(&p).unwrap(), "sk-env");
    }

    #[test]
    fn resolve_key_from_file_trims() {
        let dir = std::env::temp_dir().join(format!("codexferry_key_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("key.txt");
        std::fs::write(&path, "  sk-from-file\n").unwrap();
        let p = make_provider(None, None, Some(path.to_str().unwrap()));
        assert_eq!(resolve_api_key(&p).unwrap(), "sk-from-file");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_key_file_errors() {
        let p = make_provider(None, None, Some("/nonexistent/key/file.txt"));
        let err = resolve_api_key(&p).unwrap_err();
        assert!(err.contains("failed to read key file"));
    }

    #[test]
    fn unset_env_var_errors() {
        std::env::remove_var("TEST_API_KEY_UNSET");
        let p = make_provider(None, Some("TEST_API_KEY_UNSET"), None);
        let err = resolve_api_key(&p).unwrap_err();
        assert!(err.contains("env var TEST_API_KEY_UNSET not set"));
    }

    #[test]
    fn no_key_configured_errors() {
        let p = make_provider(None, None, None);
        assert_eq!(resolve_api_key(&p).unwrap_err(), "no API key configured");
    }

    #[test]
    fn url_construction() {
        assert_eq!(
            chat_url("https://api.x.com/v1"),
            "https://api.x.com/v1/chat/completions"
        );
        assert_eq!(
            chat_url("https://api.x.com/v1/"),
            "https://api.x.com/v1/chat/completions"
        );
        assert_eq!(
            responses_url("https://api.x.com/v1"),
            "https://api.x.com/v1/responses"
        );
    }
}
