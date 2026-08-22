//! Think-tag healing (chat path): split leaked `<thinking>` / `◁think▷`
//! markup onto the reasoning channel — blocking `heal_think_chat_message`
//! plus the incremental `ThinkStreamFilter`. Extracted from `heal.rs`
//! (module-split spec Phase 3).

use crate::wire::chat::ChatMessage;

/// Opening markers. `◁think▷` is Kimi's variant.
const OPEN_TAGS: &[&str] = &["<thinking>", "<think>", "◁think▷"];
/// Closing markers, matching [`OPEN_TAGS`].
const CLOSE_TAGS: &[&str] = &["</thinking>", "</think>", "◁/think▷"];

/// The two channels a content delta can be split across.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ThinkSplit {
    /// Text that belongs on the reasoning channel.
    pub reasoning: String,
    /// Text that belongs in the visible assistant message.
    pub text: String,
}

/// Incremental `<think>` splitter for streamed text content.
///
/// Feed content deltas through [`ThinkStreamFilter::push`]; it returns the
/// reasoning and visible text that are safe to emit now. Text that could be
/// (part of) a tag is withheld until the next delta disambiguates it, so a tag
/// split across SSE chunks (`"<thi"` then `"nk>"`) is still recognized.
#[derive(Debug)]
pub struct ThinkStreamFilter {
    pending: String,
    in_think: bool,
    fired: bool,
    enabled: bool,
}

impl Default for ThinkStreamFilter {
    fn default() -> Self {
        Self::new(true)
    }
}

impl ThinkStreamFilter {
    /// A filter that splits when `enabled`, or passes all text through
    /// untouched as visible text when the `think_tags` quirk is disabled.
    pub fn new(enabled: bool) -> Self {
        Self {
            pending: String::new(),
            in_think: false,
            fired: false,
            enabled,
        }
    }

    /// Whether any think markup was actually seen (quirk telemetry).
    pub fn fired(&self) -> bool {
        self.fired
    }

    /// Append a content delta; returns the portions safe to emit now.
    pub fn push(&mut self, delta: &str) -> ThinkSplit {
        let mut out = ThinkSplit::default();
        if !self.enabled {
            out.text.push_str(delta);
            return out;
        }
        self.pending.push_str(delta);
        loop {
            if self.in_think {
                if let Some((at, tag)) = first_tag(&self.pending, CLOSE_TAGS) {
                    out.reasoning.push_str(&self.pending[..at]);
                    self.pending.drain(..at + tag.len());
                    self.in_think = false;
                    continue;
                }
                let keep =
                    self.pending.len() - longest_tag_prefix_suffix(&self.pending, CLOSE_TAGS);
                out.reasoning.push_str(&self.pending[..keep]);
                self.pending.drain(..keep);
                return out;
            }

            match first_tag(&self.pending, OPEN_TAGS) {
                Some((at, tag)) => {
                    self.emit_text(&mut out, at);
                    self.pending.drain(..at + tag.len());
                    self.in_think = true;
                    self.fired = true;
                }
                None => {
                    let keep =
                        self.pending.len() - longest_tag_prefix_suffix(&self.pending, OPEN_TAGS);
                    self.emit_text(&mut out, keep);
                    self.pending.drain(..keep);
                    return out;
                }
            }
        }
    }

    /// Consume the filter at end of stream, returning anything still withheld.
    /// An unterminated `<think>` block is treated as reasoning, not text.
    pub fn finish(mut self) -> ThinkSplit {
        let mut out = ThinkSplit::default();
        let rest = std::mem::take(&mut self.pending);
        if self.in_think {
            out.reasoning = rest;
        } else {
            out.text = rest;
        }
        out
    }

    /// Non-consuming variant of [`Self::finish`] for the responses-path
    /// healer: returns the same result and resets the filter to its empty
    /// state, preserving the `enabled` gate (idempotent afterwards).
    pub(crate) fn take(&mut self) -> ThinkSplit {
        let fresh = Self::new(self.enabled);
        let taken = std::mem::replace(self, fresh);
        taken.finish()
    }

    /// Emit `self.pending[..upto]` as visible text without changing it.
    fn emit_text(&mut self, out: &mut ThinkSplit, upto: usize) {
        let chunk = &self.pending[..upto];
        if !chunk.is_empty() {
            out.text.push_str(chunk);
        }
    }
}

/// Heal a blocking Chat Completions assistant message in place: move leaked
/// think markup out of the visible content and into `reasoning_content`.
pub fn heal_think_chat_message(message: &mut ChatMessage) {
    let text = message.text_content().to_string();
    if !contains_think_markup(&text) {
        return;
    }
    if first_tag(&text, OPEN_TAGS).is_none() {
        return;
    }
    let mut filter = ThinkStreamFilter::new(true);
    let mut split = filter.push(&text);
    let fired = filter.fired();
    let tail = filter.finish();
    split.reasoning.push_str(&tail.reasoning);
    split.text.push_str(&tail.text);
    if !fired {
        return;
    }
    tracing::warn!("quirk think_tags fired: healed leaked <think> markup from blocking response");
    message.content = if split.text.is_empty() {
        None
    } else {
        Some(serde_json::Value::String(split.text))
    };
    if !split.reasoning.is_empty() {
        match &mut message.reasoning_content {
            Some(existing) => existing.push_str(&split.reasoning),
            slot @ None => *slot = Some(split.reasoning),
        }
    }
}

/// Whether `text` contains any think marker at all (cheap pre-check).
pub fn contains_think_markup(text: &str) -> bool {
    OPEN_TAGS
        .iter()
        .chain(CLOSE_TAGS)
        .any(|tag| text.contains(tag))
}

/// Byte offset and matched tag of the earliest tag in `text`, preferring the
/// longest tag when several start at the same offset.
fn first_tag<'a>(text: &str, tags: &[&'a str]) -> Option<(usize, &'a str)> {
    tags.iter()
        .filter_map(|tag| text.find(tag).map(|at| (at, *tag)))
        .min_by_key(|(at, tag)| (*at, std::cmp::Reverse(tag.len())))
}

/// Length in bytes of the longest suffix of `text` that is a proper prefix of
/// any tag in `tags`.
fn longest_tag_prefix_suffix(text: &str, tags: &[&str]) -> usize {
    let mut best = 0;
    for tag in tags {
        for (i, _) in tag.char_indices().skip(1) {
            if i > best && text.ends_with(&tag[..i]) {
                best = i;
            }
        }
    }
    best
}
