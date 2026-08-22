//! Unit tests for thinking-tag healing (chat + stream filter), extracted from `heal.rs`
//! (module-split spec Phase 1; bodies are verbatim moves).
use super::*;
use crate::wire::chat::ChatMessage;

/// Drive a filter over a sequence of deltas, returning the joined channels.
fn run(deltas: &[&str]) -> ThinkSplit {
    let mut filter = ThinkStreamFilter::new(true);
    let mut all = ThinkSplit::default();
    for delta in deltas {
        let split = filter.push(delta);
        all.reasoning.push_str(&split.reasoning);
        all.text.push_str(&split.text);
    }
    let tail = filter.finish();
    all.reasoning.push_str(&tail.reasoning);
    all.text.push_str(&tail.text);
    all
}

#[test]
fn splits_a_complete_think_block() {
    let out = run(&["<think>musing</think>\n\nHello!"]);
    assert_eq!(out.reasoning, "musing");
    assert_eq!(out.text, "\n\nHello!");
}

#[test]
fn recognizes_tags_split_across_deltas() {
    let out = run(&["<thi", "nk>mus", "ing</th", "ink>Hi"]);
    assert_eq!(out.reasoning, "musing");
    assert_eq!(out.text, "Hi");
}

#[test]
fn streaming_bare_close_tag_is_chunk_invariant_visible_text() {
    let out = run(&["musing", "</think>", "Hi"]);
    assert_eq!(out.reasoning, "");
    assert_eq!(out.text, "musing</think>Hi");
    assert_eq!(out, run(&["musing</think>Hi"]));
}

#[test]
fn passes_plain_text_through_untouched() {
    let out = run(&["Hello", " world"]);
    assert_eq!(out.reasoning, "");
    assert_eq!(out.text, "Hello world");
    assert!(!ThinkStreamFilter::new(true).fired());
}

#[test]
fn preserves_plain_text_leading_whitespace() {
    assert_eq!(run(&["  indented"]).text, "  indented");
    assert_eq!(run(&["\n", "  indented"]).text, "\n  indented");
}

#[test]
fn explicit_tags_are_chunk_invariant() {
    let expected = run(&["<think>musing</think>Hi"]);
    assert_eq!(expected, run(&["<thi", "nk>musing</th", "ink>Hi"]));
    assert_eq!(expected, run(&["<think>musing</think>", "Hi"]));
}

#[test]
fn preserves_whitespace_after_think_block() {
    assert_eq!(run(&["<think>x</think>  indented"]).text, "  indented");
    assert_eq!(run(&["<think>x</think>\n    code"]).text, "\n    code");
}

#[test]
fn preserves_whitespace_around_mid_message_think_block() {
    let out = run(&["prefix ", "<think>x</think>", " suffix"]);
    assert_eq!(out.reasoning, "x");
    assert_eq!(out.text, "prefix  suffix");
}

#[test]
fn close_tag_after_visible_text_is_not_reasoning() {
    // Once real text has been emitted a bare `</think>` is just text; only
    // a matched `<think>…</think>` pair splits.
    let out = run(&["see the </think> tag"]);
    assert_eq!(out.reasoning, "");
    assert_eq!(out.text, "see the </think> tag");
}

#[test]
fn unterminated_think_block_stays_reasoning() {
    let out = run(&["<think>cut off mid-thought"]);
    assert_eq!(out.reasoning, "cut off mid-thought");
    assert_eq!(out.text, "");
}

#[test]
fn kimi_markers_are_recognized() {
    let out = run(&["◁think▷musing◁/think▷Hi"]);
    assert_eq!(out.reasoning, "musing");
    assert_eq!(out.text, "Hi");
}

#[test]
fn disabled_filter_passes_markup_through() {
    let mut filter = ThinkStreamFilter::new(false);
    let out = filter.push("<think>musing</think>Hi");
    assert_eq!(out.reasoning, "");
    assert_eq!(out.text, "<think>musing</think>Hi");
}

#[test]
fn heals_blocking_message() {
    let mut message = ChatMessage {
        role: "assistant".into(),
        content: Some(serde_json::Value::String(
            "<think>musing</think>Hello!".into(),
        )),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
        name: None,
    };
    heal_think_chat_message(&mut message);
    assert_eq!(message.text_content(), "Hello!");
    assert_eq!(message.reasoning_content.as_deref(), Some("musing"));
}

#[test]
fn blocking_bare_close_tag_stays_visible_like_streaming() {
    let mut message = ChatMessage {
        role: "assistant".into(),
        content: Some("musing</think>Hi".into()),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
        name: None,
    };
    heal_think_chat_message(&mut message);
    assert_eq!(message.text_content(), "musing</think>Hi");
    assert!(message.reasoning_content.is_none());
}

#[test]
fn removes_empty_blocking_think_block() {
    let mut message = ChatMessage {
        role: "assistant".into(),
        content: Some("<think></think>".into()),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
        name: None,
    };
    heal_think_chat_message(&mut message);
    assert!(message.content.is_none());
}
