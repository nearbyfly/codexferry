//! Unit tests for DSML healing (chat + stream filter), extracted from `heal.rs`
//! (module-split spec Phase 1; bodies are verbatim moves).
use super::*;
use crate::wire::chat::ChatMessage;
use serde_json::Value;

const ENVELOPE: &str = "<｜DSML｜tool_calls>\n<｜DSML｜invoke name=\"shell\">\n<｜DSML｜parameter name=\"command\" string=\"true\">Get-Content 'D:\\data\\a.csv' -Encoding UTF8 -TotalCount 5</｜DSML｜parameter>\n<｜DSML｜parameter name=\"context\" string=\"true\">preview csv headers</｜DSML｜parameter>\n</｜DSML｜invoke>\n</｜DSML｜tool_calls>";

#[test]
fn parses_leaked_envelope() {
    let text = format!("我来读取文件。\n{ENVELOPE}");
    let (cleaned, calls) = parse_leaked_tool_calls(&text).expect("healed");
    assert_eq!(cleaned, "我来读取文件。\n");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "shell");
    let args: Value = serde_json::from_str(&calls[0].arguments).unwrap();
    assert_eq!(
        args["command"],
        "Get-Content 'D:\\data\\a.csv' -Encoding UTF8 -TotalCount 5"
    );
    assert_eq!(args["context"], "preview csv headers");
}

#[test]
fn parses_multiline_and_nonstring_parameters() {
    let text = "<｜DSML｜invoke name=\"bash\">\n<｜DSML｜parameter name=\"command\" string=\"true\">line one \\\nline two > out.txt</｜DSML｜parameter>\n<｜DSML｜parameter name=\"timeout\" string=\"false\">15</｜DSML｜parameter>\n</｜DSML｜invoke>";
    let (cleaned, calls) = parse_leaked_tool_calls(text).expect("healed");
    assert_eq!(cleaned, "");
    let args: Value = serde_json::from_str(&calls[0].arguments).unwrap();
    assert_eq!(args["command"], "line one \\\nline two > out.txt");
    assert_eq!(args["timeout"], 15);
}

#[test]
fn parses_multiple_invokes() {
    let text = format!("{ENVELOPE}\n{ENVELOPE}");
    let (_, calls) = parse_leaked_tool_calls(&text).expect("healed");
    assert_eq!(calls.len(), 2);
}

#[test]
fn plain_text_is_untouched() {
    assert!(parse_leaked_tool_calls("no markup here, just a < b").is_none());
    assert!(parse_leaked_tool_calls("<｜DSML｜tool_calls>dangling").is_none());
    assert!(parse_leaked_tool_calls("<｜｜DSML｜｜tool_calls>dangling").is_none());
}

/// Verbatim capture from Codex 0.144.5 → relay → Command Code →
/// deepseek/deepseek-v4-flash, 2026-08-07. Doubled delimiters, and the
/// parameter is a self-closing `invoke` carrying its value in `string`.
const DOUBLE_BAR_ENVELOPE: &str = "<｜｜DSML｜｜tool_calls>\n<｜｜DSML｜｜invoke name=\"exec_command\">\n<｜｜DSML｜｜invoke name=\"cmd\" string=\"echo alpha\" />\n</｜｜DSML｜｜invoke>\n</｜｜DSML｜｜tool_calls>";

#[test]
fn parses_double_bar_envelope_with_self_closing_parameters() {
    let text = format!("Let me run it.\n{DOUBLE_BAR_ENVELOPE}");
    let (cleaned, calls) = parse_leaked_tool_calls(&text).expect("healed");
    assert_eq!(cleaned, "Let me run it.\n");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "exec_command");
    let args: Value = serde_json::from_str(&calls[0].arguments).unwrap();
    assert_eq!(args["cmd"], "echo alpha");
}

#[test]
fn double_bar_self_closing_parameter_keeps_special_characters() {
    // Quotes inside the value would end the header scan early if the `>`
    // search were not quote-aware; `/` inside it must not read as
    // self-closing on its own either.
    let text = "<｜｜DSML｜｜invoke name=\"exec_command\">\n<｜｜DSML｜｜invoke name=\"cmd\" string=\"grep -r a/b > out.txt\" />\n</｜｜DSML｜｜invoke>";
    let (_, calls) = parse_leaked_tool_calls(text).expect("healed");
    let args: Value = serde_json::from_str(&calls[0].arguments).unwrap();
    assert_eq!(args["cmd"], "grep -r a/b > out.txt");
}

#[test]
fn double_bar_attribute_parser_handles_escapes_and_tag_text() {
    let text = "<｜｜DSML｜｜invoke name=\"exec_command\">\n<｜｜DSML｜｜invoke name=\"cmd\" string=\"printf \\\"a>b\\\" C:\\temp\\file </｜｜DSML｜｜invoke>\" />\n</｜｜DSML｜｜invoke>";
    let (_, calls) = parse_leaked_tool_calls(text).expect("healed");
    let args: Value = serde_json::from_str(&calls[0].arguments).unwrap();
    assert_eq!(
        args["cmd"],
        "printf \"a>b\" C:\\temp\\file </｜｜DSML｜｜invoke>"
    );
}

#[test]
fn double_bar_supports_bodied_parameters_too() {
    // Not observed in the wild, but the dialect differs only in the
    // delimiter; a bodied parameter must not silently drop its value.
    let text = "<｜｜DSML｜｜invoke name=\"bash\">\n<｜｜DSML｜｜parameter name=\"command\" string=\"true\">ls -la</｜｜DSML｜｜parameter>\n<｜｜DSML｜｜parameter name=\"timeout\" string=\"false\">15</｜｜DSML｜｜parameter>\n</｜｜DSML｜｜invoke>";
    let (_, calls) = parse_leaked_tool_calls(text).expect("healed");
    let args: Value = serde_json::from_str(&calls[0].arguments).unwrap();
    assert_eq!(args["command"], "ls -la");
    assert_eq!(args["timeout"], 15);
}

#[test]
fn parses_mixed_dialects_in_source_order() {
    for text in [
        format!("{ENVELOPE}\n{DOUBLE_BAR_ENVELOPE}"),
        format!("{DOUBLE_BAR_ENVELOPE}\n{ENVELOPE}"),
    ] {
        let (cleaned, calls) = parse_leaked_tool_calls(&text).expect("healed");
        assert_eq!(cleaned, "\n");
        assert_eq!(calls.len(), 2);
        if text.starts_with("<｜DSML｜") {
            assert_eq!(calls[0].name, "shell");
            assert_eq!(calls[1].name, "exec_command");
        } else {
            assert_eq!(calls[0].name, "exec_command");
            assert_eq!(calls[1].name, "shell");
        }
    }
}

#[test]
fn malformed_marker_does_not_block_later_valid_dialect() {
    let malformed = "<｜DSML｜invoke_extra name=\"not_a_call\">raw</｜DSML｜invoke_extra>";
    let text = format!("{malformed}\n{DOUBLE_BAR_ENVELOPE}");
    let (cleaned, calls) = parse_leaked_tool_calls(&text).expect("later call healed");
    assert_eq!(cleaned, format!("{malformed}\n"));
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "exec_command");
}

#[test]
fn malformed_calls_fail_closed() {
    let cases = [
            "<｜｜DSML｜｜invoke_extra name=\"exec_command\"></｜｜DSML｜｜invoke>",
            "<｜｜DSML｜｜invoke name=\"exec_command\"><｜｜DSML｜｜metadata name=\"cmd\" string=\"echo bad\" /></｜｜DSML｜｜invoke>",
            "<｜｜DSML｜｜invoke name=\"exec_command\"><｜｜DSML｜｜invoke name=\"cmd\" /></｜｜DSML｜｜invoke>",
            "<｜｜DSML｜｜invoke name=\"exec_command\" name=\"other\"></｜｜DSML｜｜invoke>",
            "<｜｜DSML｜｜invoke name=\"exec_command\"><｜｜DSML｜｜invoke name=\"cmd\" string=\"one\" /><｜｜DSML｜｜invoke name=\"cmd\" string=\"two\" /></｜｜DSML｜｜invoke>",
            "<｜｜DSML｜｜invoke name=\"exec_command\"><｜｜DSML｜｜invoke name=\"cmd\" string=\"unterminated /></｜｜DSML｜｜invoke>",
        ];
    for text in cases {
        assert!(
            parse_leaked_tool_calls(text).is_none(),
            "malformed DSML must not execute: {text}"
        );
    }
}

#[test]
fn malformed_call_is_preserved_when_later_call_heals() {
    let malformed =
        "<｜｜DSML｜｜invoke name=\"bad\"><｜｜DSML｜｜invoke name=\"cmd\" /></｜｜DSML｜｜invoke>";
    let text = format!("{malformed}\n{ENVELOPE}");
    let (cleaned, calls) = parse_leaked_tool_calls(&text).expect("later call healed");
    assert_eq!(cleaned, format!("{malformed}\n"));
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "shell");
}

#[test]
fn nested_call_inside_malformed_invoke_never_executes() {
    let malformed = "<｜｜DSML｜｜invoke name=\"bad\">\n<｜｜DSML｜｜invoke name=\"exec_command\">\n<｜｜DSML｜｜invoke name=\"cmd\" string=\"echo bad\" />\n</｜｜DSML｜｜invoke>\n</｜｜DSML｜｜invoke>";
    let text = format!("{malformed}\n{ENVELOPE}");
    let (cleaned, calls) = parse_leaked_tool_calls(&text).expect("later sibling healed");
    assert_eq!(cleaned, format!("{malformed}\n"));
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "shell");
}

#[test]
fn invoke_close_inside_parameter_cannot_expose_nested_call() {
    let malformed = "<｜DSML｜invoke name=\"bad\">\n<｜DSML｜parameter name=\"x\" string=\"true\">literal </｜DSML｜invoke></｜DSML｜parameter>\n<｜DSML｜invoke name=\"shell\">\n<｜DSML｜parameter name=\"command\" string=\"true\">echo bad</｜DSML｜parameter>\n</｜DSML｜invoke>\n</｜DSML｜invoke>";
    assert!(parse_leaked_tool_calls(malformed).is_none());

    let text = format!("{malformed}\n{ENVELOPE}");
    let (cleaned, calls) = parse_leaked_tool_calls(&text).expect("later sibling healed");
    assert!(cleaned.starts_with(malformed));
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "shell");
}

#[test]
fn foreign_dialect_parameter_cannot_expose_nested_call() {
    let cases = [
            "<｜DSML｜invoke name=\"bad\"><｜｜DSML｜｜parameter name=\"x\" string=\"true\">literal </｜DSML｜invoke></｜｜DSML｜｜parameter><｜｜DSML｜｜invoke name=\"exec_command\"><｜｜DSML｜｜invoke name=\"cmd\" string=\"echo bad\" /></｜｜DSML｜｜invoke></｜DSML｜invoke>",
            "<｜｜DSML｜｜invoke name=\"bad\"><｜DSML｜parameter name=\"x\" string=\"true\">literal </｜｜DSML｜｜invoke></｜DSML｜parameter><｜DSML｜invoke name=\"shell\"><｜DSML｜parameter name=\"command\" string=\"true\">echo bad</｜DSML｜parameter></｜DSML｜invoke></｜｜DSML｜｜invoke>",
        ];
    for malformed in cases {
        assert!(
            parse_leaked_tool_calls(malformed).is_none(),
            "foreign nested call must not execute: {malformed}"
        );
    }
}

#[test]
fn attributes_require_whitespace_separators() {
    for malformed in [
            "<｜DSML｜invoke name=\"shell\"><｜DSML｜parameter name=\"command\"string=\"true\">echo bad</｜DSML｜parameter></｜DSML｜invoke>",
            "<｜｜DSML｜｜invoke name=\"exec_command\"><｜｜DSML｜｜invoke name=\"cmd\"string=\"echo bad\" /></｜｜DSML｜｜invoke>",
        ] {
            assert!(parse_leaked_tool_calls(malformed).is_none());
        }
}

#[test]
fn bounded_malformed_open_tag_does_not_block_later_sibling() {
    let malformed = "<｜｜DSML｜｜invoke name=bad></｜｜DSML｜｜invoke>";
    let text = format!("{malformed}\n{ENVELOPE}");
    let (cleaned, calls) = parse_leaked_tool_calls(&text).expect("later sibling healed");
    assert!(cleaned.starts_with(malformed));
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "shell");
}

#[test]
fn unbounded_malformed_open_tag_fails_closed() {
    let malformed = "<｜｜DSML｜｜invoke name=\"unterminated></｜｜DSML｜｜invoke>";
    let text = format!("{malformed}\n{ENVELOPE}");
    assert!(parse_leaked_tool_calls(&text).is_none());
}

#[test]
fn mixed_validity_envelope_keeps_both_wrappers() {
    let valid = "<｜｜DSML｜｜invoke name=\"exec_command\"><｜｜DSML｜｜invoke name=\"cmd\" string=\"echo good\" /></｜｜DSML｜｜invoke>";
    let malformed =
        "<｜｜DSML｜｜invoke name=\"bad\"><｜｜DSML｜｜metadata name=\"x\" /></｜｜DSML｜｜invoke>";
    for body in [
        format!("{valid}\n{malformed}"),
        format!("{malformed}\n{valid}"),
    ] {
        let text = format!("<｜｜DSML｜｜tool_calls>\n{body}\n</｜｜DSML｜｜tool_calls>");
        let (cleaned, calls) = parse_leaked_tool_calls(&text).expect("valid call healed");
        assert!(cleaned.contains("<｜｜DSML｜｜tool_calls>"));
        assert!(cleaned.contains("</｜｜DSML｜｜tool_calls>"));
        assert!(cleaned.contains(malformed));
        assert_eq!(calls.len(), 1);
    }
}

#[test]
fn blocking_and_streaming_visible_text_match() {
    for (prefix, expected) in [("prefix\n", "prefix\n"), ("\n", "\n")] {
        let text = format!("{prefix}{ENVELOPE}");
        let (blocking, blocking_calls) = parse_leaked_tool_calls(&text).expect("blocking healed");
        assert_eq!(blocking, expected);

        for split in text
            .char_indices()
            .map(|(at, _)| at)
            .chain(std::iter::once(text.len()))
        {
            let mut filter = DsmlStreamFilter::default();
            let mut streamed = filter.push(&text[..split]);
            streamed.push_str(&filter.push(&text[split..]));
            let (leftover, calls) = filter.finish();
            streamed.push_str(&leftover);
            assert_eq!(streamed, blocking, "split at byte {split}");
            assert_eq!(calls, blocking_calls, "split at byte {split}");
        }
    }
}

#[test]
fn unclosed_parameter_cannot_borrow_close_from_nested_call() {
    let text = "<｜DSML｜invoke name=\"shell\">\n<｜DSML｜parameter name=\"command\" string=\"true\">echo bad\n<｜DSML｜invoke name=\"shell\">\n<｜DSML｜parameter name=\"command\" string=\"true\">echo good</｜DSML｜parameter>\n</｜DSML｜invoke>";
    assert!(parse_leaked_tool_calls(text).is_none());
}

#[test]
fn heal_chat_message_fires_on_double_bar() {
    let mut message = ChatMessage {
        role: "assistant".into(),
        content: Some(Value::String(DOUBLE_BAR_ENVELOPE.to_string())),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
        name: None,
    };
    heal_dsml_chat_message(&mut message);
    let calls = message.tool_calls.expect("tool calls synthesized");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["function"]["name"], "exec_command");
    assert!(message.content.is_none());
}

#[test]
fn stream_filter_holds_split_double_bar_marker_and_heals() {
    let mut filter = DsmlStreamFilter::default();
    let mut emitted = String::new();
    // Split inside the doubled delimiter: withholding only a single-bar
    // prefix here would leak "<｜" and desync the marker.
    emitted.push_str(&filter.push("Let me run it.<｜"));
    emitted.push_str(&filter.push("｜DSML｜｜tool_calls>\n<｜｜DSML｜｜invoke name=\"exec_command\">\n<｜｜DSML｜｜invoke name=\"cmd\" string=\"echo alpha\" />\n</｜｜DSML｜｜invoke>\n</｜｜DSML｜｜"));
    emitted.push_str(&filter.push("tool_calls>"));
    assert_eq!(emitted, "Let me run it.");
    let (leftover, calls) = filter.finish();
    assert_eq!(leftover, "");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "exec_command");
    let args: Value = serde_json::from_str(&calls[0].arguments).unwrap();
    assert_eq!(args["cmd"], "echo alpha");
}

#[test]
fn stream_filter_handles_mixed_dialects_one_character_at_a_time() {
    let text = format!("prefix\n{ENVELOPE}\n{DOUBLE_BAR_ENVELOPE}");
    let mut filter = DsmlStreamFilter::default();
    let mut emitted = String::new();
    for ch in text.chars() {
        emitted.push_str(&filter.push(&ch.to_string()));
    }
    let (leftover, calls) = filter.finish();
    assert_eq!(format!("{emitted}{leftover}"), "prefix\n\n");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].name, "shell");
    assert_eq!(calls[1].name, "exec_command");
}

#[test]
fn stream_filter_passes_plain_text() {
    let mut filter = DsmlStreamFilter::default();
    assert_eq!(filter.push("hello "), "hello ");
    assert_eq!(filter.push("a < b and c > d"), "a < b and c > d");
    let (leftover, calls) = filter.finish();
    assert_eq!(leftover, "");
    assert!(calls.is_empty());
}

#[test]
fn stream_filter_holds_split_marker_and_heals() {
    let mut filter = DsmlStreamFilter::default();
    let mut emitted = String::new();
    emitted.push_str(&filter.push("先看文件。<｜DS"));
    emitted.push_str(&filter.push("ML｜tool_calls>\n<｜DSML｜invoke name=\"shell\">\n<｜DSML｜parameter name=\"command\" string=\"true\">ls</｜DSML｜parameter>\n</｜DSML｜invoke>\n</｜DSML｜"));
    emitted.push_str(&filter.push("tool_calls>"));
    assert_eq!(emitted, "先看文件。");
    let (leftover, calls) = filter.finish();
    assert_eq!(leftover, "");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "shell");
    let args: Value = serde_json::from_str(&calls[0].arguments).unwrap();
    assert_eq!(args["command"], "ls");
}

#[test]
fn stream_filter_releases_false_marker_prefix() {
    let mut filter = DsmlStreamFilter::default();
    let first = filter.push("a <");
    let second = filter.push("b> c");
    assert_eq!(format!("{first}{second}"), "a <b> c");
}

#[test]
fn disabled_stream_filter_passes_markup_through() {
    let mut filter = DsmlStreamFilter::new(false);
    assert_eq!(filter.push(ENVELOPE), ENVELOPE);
    let (leftover, calls) = filter.finish();
    assert_eq!(leftover, "");
    assert!(calls.is_empty());
}

#[test]
fn stream_filter_passes_incomplete_markup_through() {
    let mut filter = DsmlStreamFilter::default();
    filter.push("<｜DSML｜tool_calls>\n<｜DSML｜invoke name=\"shell\">truncat");
    let (leftover, calls) = filter.finish();
    assert!(calls.is_empty());
    assert!(leftover.contains("truncat"), "raw text must not be lost");
}

#[test]
fn heal_chat_message_moves_markup_into_tool_calls() {
    let mut message = ChatMessage {
        role: "assistant".into(),
        content: Some(Value::String(format!("我来逐步完成这个任务。\n{ENVELOPE}"))),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
        name: None,
    };
    heal_dsml_chat_message(&mut message);
    assert_eq!(message.text_content(), "我来逐步完成这个任务。\n");
    let tool_calls = message.tool_calls.as_ref().expect("healed tool_calls");
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0]["function"]["name"], "shell");
    assert!(tool_calls[0]["id"]
        .as_str()
        .unwrap()
        .starts_with("call_dsml_"));
}
