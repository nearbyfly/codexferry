//! DSML tool-call healing (chat path): parse leaked `<tool_calls>`
//! markup out of assistant text and replay it as tool calls — blocking
//! `heal_dsml_chat_message` plus `DsmlStreamFilter`. Extracted from
//! `heal.rs` (module-split spec Phase 3).

//! Healing for leaked DeepSeek DSML tool-call markup.
//!
//! DeepSeek V3.2/V4 models emit tool calls as DSML markup which the provider
//! is supposed to parse into structured `tool_calls`. Intermittently —
//! especially with `tool_choice=auto` + streaming, or when a parameter value
//! contains newlines — the raw markup leaks into the assistant `content`
//! instead:
//!
//! ```text
//! <｜DSML｜tool_calls>
//! <｜DSML｜invoke name="shell">
//! <｜DSML｜parameter name="command" string="true">Get-Content 'a.csv' -TotalCount 5</｜DSML｜parameter>
//! </｜DSML｜invoke>
//! </｜DSML｜tool_calls>
//! ```
//!
//! Codex then sees plain text, no tool runs, and the turn silently stalls.
//! This module parses leaked DSML back into structured tool calls and strips
//! the markup from the visible text. The `｜` (U+FF5C) delimiters are
//! DeepSeek-internal tokens that never appear in legitimate output, so
//! healing is always on.
//!
//! # Dialects
//!
//! V4 Flash leaks a *doubled* delimiter and expresses parameters as
//! self-closing `invoke` tags whose value sits in the `string` attribute
//! (observed 2026-08-07 via Command Code):
//!
//! ```text
//! <｜｜DSML｜｜tool_calls>
//! <｜｜DSML｜｜invoke name="exec_command">
//! <｜｜DSML｜｜invoke name="cmd" string="echo alpha" />
//! </｜｜DSML｜｜invoke>
//! </｜｜DSML｜｜tool_calls>
//! ```
//!
//! Matching only the single-bar form made healing a no-op for that model: the
//! marker search missed, `parse_leaked_tool_calls` returned `None`, and the
//! raw markup reached Codex as plain text. Both delimiters are handled now,
//! and parameters are read by *shape* rather than by tag name — a self-closing
//! tag has no body, so its value can only live in an attribute.

use crate::wire::chat::ChatMessage;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// One delimiter flavour of the DSML markup.
///
/// Everything is spelled out per dialect rather than concatenated from a
/// prefix, so a mistyped delimiter fails to compile instead of silently
/// producing a marker that never matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DsmlDialect {
    /// Prefix common to every opening tag of this dialect.
    marker: &'static str,
    invoke_open: &'static str,
    invoke_close: &'static str,
    calls_open: &'static str,
    calls_close: &'static str,
}

/// `<｜DSML｜…>` — V3.2 / V4 Pro.
const SINGLE_BAR: DsmlDialect = DsmlDialect {
    marker: "<｜DSML｜",
    invoke_open: "<｜DSML｜invoke",
    invoke_close: "</｜DSML｜invoke>",
    calls_open: "<｜DSML｜tool_calls>",
    calls_close: "</｜DSML｜tool_calls>",
};

/// `<｜｜DSML｜｜…>` — V4 Flash.
const DOUBLE_BAR: DsmlDialect = DsmlDialect {
    marker: "<｜｜DSML｜｜",
    invoke_open: "<｜｜DSML｜｜invoke",
    invoke_close: "</｜｜DSML｜｜invoke>",
    calls_open: "<｜｜DSML｜｜tool_calls>",
    calls_close: "</｜｜DSML｜｜tool_calls>",
};

/// Double-bar first: `<｜DSML｜` is not a substring of `<｜｜DSML｜｜` (the `<`
/// is followed by two bars there), but probing the more specific form first
/// keeps that independent of the delimiters ever changing.
const DIALECTS: [DsmlDialect; 2] = [DOUBLE_BAR, SINGLE_BAR];

/// The dialect whose marker appears earliest in `text`, if any.
fn detect_dialect(text: &str) -> Option<(DsmlDialect, usize)> {
    DIALECTS
        .iter()
        .filter_map(|dialect| text.find(dialect.marker).map(|at| (*dialect, at)))
        .min_by_key(|(_, at)| *at)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DsmlToolCall {
    pub name: String,
    /// JSON-encoded arguments object, ready for Chat Completions `function.arguments`.
    pub arguments: String,
}

/// Parse leaked DSML tool-call markup out of `text`.
///
/// Returns the cleaned visible text plus the parsed calls, or `None` when no
/// complete `<｜DSML｜invoke>` block could be parsed (the text is then left
/// untouched so nothing is lost).
pub fn parse_leaked_tool_calls(text: &str) -> Option<(String, Vec<DsmlToolCall>)> {
    let mut calls = Vec::new();
    let mut cleaned = String::new();
    let mut copied_through = 0;
    let mut search_from = 0;

    while let Some((dialect, start)) = find_next_invoke(text, search_from) {
        match parse_invoke(text, start, dialect) {
            Ok((end, call)) => {
                cleaned.push_str(&text[copied_through..start]);
                calls.push(call);
                copied_through = end;
                search_from = end;
            }
            Err(()) => {
                // Preserve malformed candidates byte-for-byte, but continue so
                // an unrelated valid call later in the message can still heal.
                let Some(end) = malformed_invoke_end(text, start, dialect) else {
                    break;
                };
                search_from = end;
            }
        }
    }

    if calls.is_empty() {
        return None;
    }
    cleaned.push_str(&text[copied_through..]);
    Some((strip_empty_envelopes(cleaned), calls))
}

#[derive(Debug)]
struct ParsedTag {
    name: String,
    attrs: BTreeMap<String, String>,
    self_closing: bool,
    end: usize,
}

fn find_next_invoke(text: &str, from: usize) -> Option<(DsmlDialect, usize)> {
    DIALECTS
        .iter()
        .filter_map(|dialect| {
            let mut offset = from;
            while let Some(found) = text[offset..].find(dialect.invoke_open) {
                let start = offset + found;
                let after = start + dialect.invoke_open.len();
                if text[after..]
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_whitespace() || ch == '>')
                {
                    return Some((*dialect, start));
                }
                offset = after;
            }
            None
        })
        .min_by_key(|(_, start)| *start)
}

fn parse_invoke(
    text: &str,
    start: usize,
    dialect: DsmlDialect,
) -> Result<(usize, DsmlToolCall), ()> {
    let invoke = parse_open_tag(text, start, dialect)?;
    if invoke.name != "invoke" || invoke.self_closing {
        return Err(());
    }
    let name = invoke
        .attrs
        .get("name")
        .filter(|name| !name.is_empty())
        .ok_or(())?;
    let mut arguments = serde_json::Map::new();
    let mut cursor = invoke.end;

    loop {
        cursor = skip_whitespace(text, cursor);
        if text[cursor..].starts_with(dialect.invoke_close) {
            let end = cursor + dialect.invoke_close.len();
            return Ok((
                end,
                DsmlToolCall {
                    name: name.clone(),
                    arguments: Value::Object(arguments).to_string(),
                },
            ));
        }
        if !text[cursor..].starts_with(dialect.marker) {
            return Err(());
        }

        let parameter = parse_open_tag(text, cursor, dialect)?;
        let parameter_name = parameter
            .attrs
            .get("name")
            .filter(|name| !name.is_empty())
            .ok_or(())?
            .clone();
        if arguments.contains_key(&parameter_name) {
            return Err(());
        }

        let value = if dialect == DOUBLE_BAR && parameter.name == "invoke" && parameter.self_closing
        {
            Value::String(parameter.attrs.get("string").ok_or(())?.clone())
        } else if parameter.name == "parameter" && !parameter.self_closing {
            let string_kind = parameter
                .attrs
                .get("string")
                .map(String::as_str)
                .ok_or(())?;
            if !matches!(string_kind, "true" | "false") {
                return Err(());
            }
            let close = format!("</{}parameter>", dialect.marker.trim_start_matches('<'));
            let tail = &text[parameter.end..];
            let close_offset = tail.find(&close).ok_or(())?;
            if tail
                .find(dialect.marker)
                .into_iter()
                .chain(tail.find(dialect.invoke_close))
                .any(|offset| offset < close_offset)
            {
                return Err(());
            }
            let close_at = parameter.end + close_offset;
            let raw = &text[parameter.end..close_at];
            cursor = close_at + close.len();
            if string_kind == "false" {
                serde_json::from_str(raw.trim()).map_err(|_| ())?
            } else {
                Value::String(raw.to_string())
            }
        } else {
            return Err(());
        };
        if parameter.self_closing {
            cursor = parameter.end;
        }
        arguments.insert(parameter_name, value);
    }
}

/// Find the balanced end of a malformed invoke so recovery never descends
/// into a nested, valid-looking call and executes it out of context.
fn malformed_invoke_end(text: &str, start: usize, dialect: DsmlDialect) -> Option<usize> {
    let outer = scan_open_tag(text, start, dialect)?;
    if outer.name != "invoke" || outer.self_closing {
        return Some(outer.end);
    }
    let mut depth = 1usize;
    let mut cursor = outer.end;
    while cursor < text.len() {
        let next_foreign_open = DIALECTS
            .iter()
            .filter(|candidate| **candidate != dialect)
            .filter_map(|candidate| {
                text[cursor..]
                    .find(candidate.marker)
                    .map(|offset| cursor + offset)
            })
            .min();
        let next_open = text[cursor..]
            .find(dialect.marker)
            .map(|offset| cursor + offset);
        let next_close = text[cursor..]
            .find(dialect.invoke_close)
            .map(|offset| cursor + offset);
        let next_current = next_open.into_iter().chain(next_close).min();
        if next_foreign_open
            .is_some_and(|foreign| next_current.is_none_or(|current| foreign < current))
        {
            // Mixed-dialect structure inside a malformed invoke cannot be
            // balanced by this dialect's stack. Stop rather than recovering
            // at a forged close and exposing the foreign nested call.
            return None;
        }
        match (next_open, next_close) {
            (None, Some(close)) => {
                depth -= 1;
                cursor = close + dialect.invoke_close.len();
                if depth == 0 {
                    return Some(cursor);
                }
            }
            (Some(open), Some(close)) if close < open => {
                depth -= 1;
                cursor = close + dialect.invoke_close.len();
                if depth == 0 {
                    return Some(cursor);
                }
            }
            (Some(open), _) => {
                let tag = scan_open_tag(text, open, dialect)?;
                if tag.name == "invoke" && !tag.self_closing {
                    depth += 1;
                } else if tag.name == "parameter" && !tag.self_closing {
                    let close = parameter_close(dialect);
                    let tail = &text[tag.end..];
                    let close_offset = tail.find(&close)?;
                    // A nested opening tag makes the parameter boundary
                    // ambiguous. Stop recovery rather than borrowing its
                    // closing tag and exposing a nested call for execution.
                    if tail
                        .find(dialect.marker)
                        .is_some_and(|offset| offset < close_offset)
                    {
                        return None;
                    }
                    cursor = tag.end + close_offset + close.len();
                    continue;
                }
                cursor = tag.end;
            }
            (None, None) => return None,
        }
    }
    None
}

/// Locate a tag boundary without accepting its attributes. Recovery needs to
/// skip a malformed candidate before looking for a later sibling, but using
/// the strict parser here would make a bounded error such as `name=bad` stop
/// all scanning. Unclosed quotes remain unbounded and therefore fail closed.
fn scan_open_tag(text: &str, start: usize, dialect: DsmlDialect) -> Option<ParsedTag> {
    if !text[start..].starts_with(dialect.marker) {
        return None;
    }
    let mut cursor = start + dialect.marker.len();
    let name_start = cursor;
    while let Some(ch) = text[cursor..].chars().next() {
        if ch.is_whitespace() || matches!(ch, '/' | '>') {
            break;
        }
        cursor += ch.len_utf8();
    }
    if cursor == name_start {
        return None;
    }
    let name = text[name_start..cursor].to_string();
    let mut quoted = false;
    let mut escaped = false;
    while let Some(ch) = text[cursor..].chars().next() {
        cursor += ch.len_utf8();
        if quoted {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                quoted = false;
            }
        } else if ch == '"' {
            quoted = true;
        } else if ch == '>' {
            let before = text[..cursor - 1].trim_end();
            return Some(ParsedTag {
                name,
                attrs: BTreeMap::new(),
                self_closing: before.ends_with('/'),
                end: cursor,
            });
        }
    }
    None
}

fn parse_open_tag(text: &str, start: usize, dialect: DsmlDialect) -> Result<ParsedTag, ()> {
    if !text[start..].starts_with(dialect.marker) {
        return Err(());
    }
    let mut cursor = start + dialect.marker.len();
    let name_start = cursor;
    while let Some(ch) = text[cursor..].chars().next() {
        if ch.is_whitespace() || matches!(ch, '/' | '>') {
            break;
        }
        cursor += ch.len_utf8();
    }
    if cursor == name_start {
        return Err(());
    }
    let name = text[name_start..cursor].to_string();
    let mut attrs = BTreeMap::new();

    loop {
        cursor = skip_whitespace(text, cursor);
        match text[cursor..].chars().next().ok_or(())? {
            '>' => {
                return Ok(ParsedTag {
                    name,
                    attrs,
                    self_closing: false,
                    end: cursor + 1,
                });
            }
            '/' => {
                cursor += 1;
                cursor = skip_whitespace(text, cursor);
                if !text[cursor..].starts_with('>') {
                    return Err(());
                }
                return Ok(ParsedTag {
                    name,
                    attrs,
                    self_closing: true,
                    end: cursor + 1,
                });
            }
            _ => {}
        }

        let key_start = cursor;
        while let Some(ch) = text[cursor..].chars().next() {
            if ch.is_whitespace() || ch == '=' {
                break;
            }
            if matches!(ch, '/' | '>') {
                return Err(());
            }
            cursor += ch.len_utf8();
        }
        if cursor == key_start {
            return Err(());
        }
        let key = text[key_start..cursor].to_string();
        cursor = skip_whitespace(text, cursor);
        if !text[cursor..].starts_with('=') {
            return Err(());
        }
        cursor += 1;
        cursor = skip_whitespace(text, cursor);
        if !text[cursor..].starts_with('"') {
            return Err(());
        }
        cursor += 1;
        let (value, end) = parse_quoted_value(text, cursor)?;
        cursor = end;
        if attrs.insert(key, value).is_some() {
            return Err(());
        }
        if text[cursor..]
            .chars()
            .next()
            .is_some_and(|ch| !ch.is_whitespace() && !matches!(ch, '/' | '>'))
        {
            return Err(());
        }
    }
}

fn parse_quoted_value(text: &str, mut cursor: usize) -> Result<(String, usize), ()> {
    let mut value = String::new();
    loop {
        let ch = text[cursor..].chars().next().ok_or(())?;
        cursor += ch.len_utf8();
        match ch {
            '"' => return Ok((value, cursor)),
            '\\' => {
                let escaped = text[cursor..].chars().next().ok_or(())?;
                if matches!(escaped, '"' | '\\') {
                    value.push(escaped);
                    cursor += escaped.len_utf8();
                } else {
                    value.push('\\');
                }
            }
            other => value.push(other),
        }
    }
}

fn skip_whitespace(text: &str, mut cursor: usize) -> usize {
    while let Some(ch) = text[cursor..].chars().next() {
        if !ch.is_whitespace() {
            break;
        }
        cursor += ch.len_utf8();
    }
    cursor
}

fn parameter_close(dialect: DsmlDialect) -> String {
    format!("</{}parameter>", dialect.marker.trim_start_matches('<'))
}

/// Remove only matched envelope pairs whose contents became empty after all
/// successfully parsed invokes were removed. If malformed markup remains,
/// both wrappers stay intact rather than deleting only one side.
fn strip_empty_envelopes(mut text: String) -> String {
    loop {
        let mut removed = false;
        for dialect in DIALECTS {
            let mut search_from = 0;
            while let Some(open_offset) = text[search_from..].find(dialect.calls_open) {
                let open = search_from + open_offset;
                let inner = open + dialect.calls_open.len();
                let Some(close_offset) = text[inner..].find(dialect.calls_close) else {
                    break;
                };
                let close = inner + close_offset;
                if text[inner..close].trim().is_empty() {
                    text.replace_range(open..close + dialect.calls_close.len(), "");
                    removed = true;
                    break;
                }
                search_from = close + dialect.calls_close.len();
            }
            if removed {
                break;
            }
        }
        if !removed {
            return text;
        }
    }
}

pub(crate) fn synthesize_call_id() -> String {
    format!("call_dsml_{}", uuid::Uuid::new_v4().simple())
}

/// Heal a blocking Chat Completions assistant message in place: parse leaked
/// DSML markup in its text content into structured `tool_calls` entries and
/// strip the markup from the visible text.
pub fn heal_dsml_chat_message(message: &mut ChatMessage) {
    let text = message.text_content();
    if detect_dialect(text).is_none() {
        return;
    }
    let Some((cleaned, calls)) = parse_leaked_tool_calls(text) else {
        return;
    };
    let count = calls.len();
    tracing::warn!(
        "quirk dsml_heal fired: healed {count} leaked DSML tool call(s) from blocking response"
    );
    message.content = if cleaned.is_empty() {
        None
    } else {
        Some(Value::String(cleaned))
    };
    let tool_calls = message.tool_calls.get_or_insert_with(Vec::new);
    for call in calls {
        tool_calls.push(json!({
            "id": synthesize_call_id(),
            "type": "function",
            "function": { "name": call.name, "arguments": call.arguments }
        }));
    }
}

/// Incremental DSML filter for streamed text content.
///
/// Feed content deltas through [`DsmlStreamFilter::push`]; it returns the text
/// that is safe to emit downstream. Text that could be (part of) a DSML marker
/// is withheld. Once a marker is confirmed the rest of the stream is buffered
/// and [`DsmlStreamFilter::finish`] returns the cleaned leftover text plus the
/// healed tool calls.
#[derive(Debug)]
pub struct DsmlStreamFilter {
    pending: String,
    in_dsml: bool,
    enabled: bool,
    /// Whether real DSML markup was detected (a marker confirmed), as
    /// opposed to merely withholding a marker-prefix tail pending
    /// disambiguation. Gates `healing_fired` in the responses healer.
    fired: bool,
}

impl Default for DsmlStreamFilter {
    fn default() -> Self {
        Self::new(true)
    }
}

impl DsmlStreamFilter {
    /// A filter that heals when `enabled`, or passes all text through
    /// untouched when the `dsml_heal` quirk is disabled.
    pub fn new(enabled: bool) -> Self {
        Self {
            pending: String::new(),
            in_dsml: false,
            enabled,
            fired: false,
        }
    }

    /// Append a content delta; returns the portion that is safe to emit now.
    pub fn push(&mut self, delta: &str) -> String {
        if !self.enabled {
            return delta.to_string();
        }
        self.pending.push_str(delta);
        if self.in_dsml {
            return String::new();
        }
        if let Some((_, start)) = detect_dialect(&self.pending) {
            self.in_dsml = true;
            self.fired = true;
            let emit = self.pending[..start].to_string();
            self.pending.drain(..start);
            return emit;
        }
        // Withhold the longest tail that could still grow into the marker.
        let hold = longest_marker_prefix_suffix(&self.pending);
        let emit_len = self.pending.len() - hold;
        let emit = self.pending[..emit_len].to_string();
        self.pending.drain(..emit_len);
        emit
    }

    /// Whether the filter actually detected DSML markup (a marker
    /// confirmed), as opposed to just withholding a marker-prefix tail.
    pub fn fired(&self) -> bool {
        self.fired
    }

    /// Consume the filter at end of stream. Returns any remaining visible text
    /// and the tool calls healed from buffered DSML markup.
    pub fn finish(self) -> (String, Vec<DsmlToolCall>) {
        if !self.in_dsml {
            return (self.pending, Vec::new());
        }
        match parse_leaked_tool_calls(&self.pending) {
            Some((cleaned, calls)) => (cleaned, calls),
            // Incomplete or unparseable markup: pass the raw text through so
            // nothing is silently dropped.
            None => (self.pending, Vec::new()),
        }
    }

    /// Non-consuming variant of [`Self::finish`] for the responses-path
    /// healer: returns the same result and resets the filter to its empty
    /// state, preserving the `enabled` gate (idempotent afterwards).
    pub(crate) fn take(&mut self) -> (String, Vec<DsmlToolCall>) {
        let fresh = Self::new(self.enabled);
        let taken = std::mem::replace(self, fresh);
        taken.finish()
    }
}

/// Length in bytes of the longest suffix of `text` that is a proper prefix of
/// any dialect's marker.
///
/// Taking the maximum across dialects matters: `<｜` is a prefix of both, and
/// withholding only the shorter one would emit the first bar of a double-bar
/// marker before the rest of it arrives, splitting the marker across chunks so
/// it never matches.
fn longest_marker_prefix_suffix(text: &str) -> usize {
    let mut best = 0;
    for dialect in DIALECTS {
        for (i, _) in dialect.marker.char_indices().skip(1) {
            if text.ends_with(&dialect.marker[..i]) {
                best = best.max(i);
            }
        }
    }
    best
}

/// Output-index base for healer-injected items: upstream indexes are small
/// and sequential, so 10_000+ cannot collide.
pub(super) const INJECT_INDEX_BASE: usize = 10_000;
