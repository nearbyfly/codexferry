//! Unit tests for stream_tests module, extracted from `response.rs`
//! (module-split spec Phase 1; bodies are verbatim moves).
use super::*;

fn make_chunk(content: Option<&str>, finish: Option<&str>) -> ChatStreamChunk {
    ChatStreamChunk {
        choices: vec![ChatStreamChoice {
            index: 0,
            delta: ChatDelta {
                content: content.map(String::from),
                ..Default::default()
            },
            finish_reason: finish.map(String::from),
        }],
        usage: None,
    }
}

#[test]
fn eager_start_emits_created_once() {
    let mut conv = StreamConverter::new(
        "resp_eager".into(),
        "m".into(),
        HealGates::default(),
        NamespaceToolMap::new(),
    );

    // Eager call (proxy pre-loop): one response.created event.
    let started = conv.start().unwrap();
    assert_eq!(started.0, "response.created");
    let created: Value = serde_json::from_str(&started.1).unwrap();
    assert_eq!(created["type"], "response.created");
    assert_eq!(created["response"]["id"], "resp_eager");
    assert_eq!(created["response"]["status"], "in_progress");

    // Second eager call: nothing (already started).
    assert!(conv.start().is_none());

    // First chunk no longer re-emits response.created - the chunk's
    // events start directly at output_item.added.
    let e = conv.on_chunk(&make_chunk(Some("hi"), None));
    let names: Vec<&str> = e.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(
        names,
        vec!["response.output_item.added", "response.output_text.delta"]
    );
}

#[test]
fn text_response_event_sequence() {
    let mut conv = StreamConverter::new(
        "resp_test".into(),
        "m".into(),
        HealGates::default(),
        NamespaceToolMap::new(),
    );
    let e1 = conv.on_chunk(&make_chunk(Some("hel"), None));
    let _e2 = conv.on_chunk(&make_chunk(Some("lo"), None));
    let e3 = conv.on_chunk(&make_chunk(None, Some("stop")));

    // Chunk 1: response.created + output_item.added + output_text.delta
    let names: Vec<&str> = e1.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "response.created",
            "response.output_item.added",
            "response.output_text.delta",
        ]
    );

    // response.created has type + response wrapper
    let created: Value = serde_json::from_str(&e1[0].1).unwrap();
    assert_eq!(created["type"], "response.created");
    assert_eq!(created["response"]["id"], "resp_test");
    assert_eq!(created["response"]["status"], "in_progress");
    assert_eq!(created["response"]["model"], "m");

    // output_item.added has output_index, item with id + status
    let added: Value = serde_json::from_str(&e1[1].1).unwrap();
    assert_eq!(added["type"], "response.output_item.added");
    assert_eq!(added["output_index"], 0);
    assert_eq!(added["item"]["type"], "message");
    assert_eq!(added["item"]["role"], "assistant");
    assert_eq!(added["item"]["status"], "in_progress");
    assert!(added["item"]["id"].as_str().unwrap().starts_with("msg_"));

    // delta has type + item_id + output_index
    let delta: Value = serde_json::from_str(&e1[2].1).unwrap();
    assert_eq!(delta["type"], "response.output_text.delta");
    assert_eq!(delta["delta"], "hel");
    assert!(delta["item_id"].as_str().unwrap().starts_with("msg_"));
    assert_eq!(delta["output_index"], 0);

    // Chunk 3 (finish_reason): emits nothing; sequence deferred to finish().
    assert!(e3.is_empty());
    let ef = conv.finish();

    // Finish sequence: output_item.done + response.completed
    let names3: Vec<&str> = ef.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(
        names3,
        vec!["response.output_item.done", "response.completed"]
    );

    // output_item.done has full text in item
    let done: Value = serde_json::from_str(&ef[0].1).unwrap();
    assert_eq!(done["type"], "response.output_item.done");
    assert_eq!(done["item"]["content"][0]["text"], "hello");
    assert_eq!(done["item"]["status"], "completed");

    // response.completed has type + response wrapper + output + usage
    let completed: Value = serde_json::from_str(&ef[1].1).unwrap();
    assert_eq!(completed["type"], "response.completed");
    assert_eq!(completed["response"]["id"], "resp_test");
    assert_eq!(completed["response"]["status"], "completed");
    assert_eq!(
        completed["response"]["output"][0]["content"][0]["text"],
        "hello"
    );

    // Accumulated state
    assert_eq!(conv.acc.text, "hello");
    assert_eq!(conv.acc.items.len(), 1);
}

#[test]
fn completed_output_items_match_done_items() {
    // The response.completed "output" array must carry the SAME full-shape
    // items (with id + status) as the output_item.done events — not the
    // simplified session items.
    let mut conv = StreamConverter::new(
        "resp_t".into(),
        "m".into(),
        HealGates::default(),
        NamespaceToolMap::new(),
    );
    conv.on_chunk(&make_chunk(Some("hello"), None));
    conv.on_chunk(&make_chunk(None, Some("stop")));
    let ef = conv.finish();

    // Extract the output_item.done item and the response.completed output item.
    let done: Value = serde_json::from_str(
        &ef.iter()
            .find(|(t, _)| t == "response.output_item.done")
            .unwrap()
            .1,
    )
    .unwrap();
    let completed: Value = serde_json::from_str(
        &ef.iter()
            .find(|(t, _)| t == "response.completed")
            .unwrap()
            .1,
    )
    .unwrap();

    let done_item = &done["item"];
    let completed_item = &completed["response"]["output"][0];

    // The completed output item must have id + status (matching the done item).
    assert!(done_item["id"].as_str().unwrap().starts_with("msg_"));
    assert_eq!(done_item["status"], "completed");
    assert_eq!(completed_item["id"], done_item["id"]);
    assert_eq!(completed_item["status"], "completed");
    // And they carry the same content text.
    assert_eq!(
        completed_item["content"][0]["text"],
        done_item["content"][0]["text"]
    );

    // Session items (internal) are simplified and may omit id/status — that
    // is intentional; they are not sent to the client.
    assert_eq!(conv.acc.items.len(), 1);
    assert_eq!(conv.acc.items[0]["content"][0]["text"], "hello");
}

#[test]
fn streaming_namespace_tool_call_decoded_in_all_four_places() {
    // A namespaced tool-call delta (encoded `{namespace}-{name}`) must
    // decode back to namespace + name consistently in the output_item.added
    // event, the output_item.done event, the response.completed output
    // array, and the persisted session items (spec §7).
    let mut ns_map = NamespaceToolMap::new();
    ns_map.insert(
        "multi_agent_v1-spawn_agent".into(),
        NamespaceToolName {
            namespace: "multi_agent_v1".into(),
            name: "spawn_agent".into(),
        },
    );
    let mut conv = StreamConverter::new("resp_ns".into(), "m".into(), HealGates::default(), ns_map);

    // First delta announces the tool call with the encoded name.
    let mut chunk1 = make_chunk(None, None);
    chunk1.choices[0].delta.tool_calls = Some(vec![DeltaToolCall {
        index: 0,
        id: Some("call_ns_1".into()),
        function: Some(DeltaFunction {
            name: Some("multi_agent_v1-spawn_agent".into()),
            arguments: Some("{\"task\":".into()),
        }),
    }]);
    conv.on_chunk(&chunk1);

    // Second chunk finishes the arguments and signals tool_calls.
    let mut chunk2 = make_chunk(None, Some("tool_calls"));
    chunk2.choices[0].delta.tool_calls = Some(vec![DeltaToolCall {
        index: 0,
        function: Some(DeltaFunction {
            arguments: Some("\"ship\"}".into()),
            ..Default::default()
        }),
        ..Default::default()
    }]);
    conv.on_chunk(&chunk2);
    let ef = conv.finish();

    // 1) output_item.added carries namespace + decoded name.
    let added: Value = serde_json::from_str(
        &ef.iter()
            .find(|(t, _)| t == "response.output_item.added")
            .unwrap()
            .1,
    )
    .unwrap();
    assert_eq!(added["item"]["type"], "function_call");
    assert_eq!(added["item"]["namespace"], "multi_agent_v1");
    assert_eq!(added["item"]["name"], "spawn_agent");

    // 2) output_item.done carries the same namespace + name.
    let done: Value = serde_json::from_str(
        &ef.iter()
            .find(|(t, _)| t == "response.output_item.done")
            .unwrap()
            .1,
    )
    .unwrap();
    assert_eq!(done["item"]["namespace"], "multi_agent_v1");
    assert_eq!(done["item"]["name"], "spawn_agent");

    // 3) response.completed output array carries them too, matching the
    // done item exactly.
    let completed: Value = serde_json::from_str(
        &ef.iter()
            .find(|(t, _)| t == "response.completed")
            .unwrap()
            .1,
    )
    .unwrap();
    let comp_item = &completed["response"]["output"][0];
    assert_eq!(comp_item, &done["item"]);
    assert_eq!(comp_item["namespace"], "multi_agent_v1");
    assert_eq!(comp_item["name"], "spawn_agent");

    // 4) Persisted session item keeps namespace + decoded name for replay.
    assert_eq!(conv.acc.items.len(), 1);
    assert_eq!(conv.acc.items[0]["type"], "function_call");
    assert_eq!(conv.acc.items[0]["namespace"], "multi_agent_v1");
    assert_eq!(conv.acc.items[0]["name"], "spawn_agent");
    assert_eq!(conv.acc.items[0]["call_id"], "call_ns_1");
}

#[test]
fn no_in_progress_or_content_part_events() {
    // codex-relay does NOT emit these events; Codex CLI does not require them.
    // Assert over ALL events: opening chunk, finish_reason chunk, and the
    // finish() sequence.
    let mut conv = StreamConverter::new(
        "r".into(),
        "m".into(),
        HealGates::default(),
        NamespaceToolMap::new(),
    );
    let mut events = conv.on_chunk(&make_chunk(Some("hi"), None));
    events.extend(conv.on_chunk(&make_chunk(None, Some("stop"))));
    events.extend(conv.finish());
    let all_types: Vec<String> = events.iter().map(|(t, _)| t.clone()).collect();
    assert!(!all_types.iter().any(|t| t == "response.in_progress"));
    assert!(!all_types.iter().any(|t| t.contains("content_part")));
}

#[test]
fn reasoning_then_text_event_order() {
    let mut conv = StreamConverter::new(
        "r".into(),
        "m".into(),
        HealGates::default(),
        NamespaceToolMap::new(),
    );

    // Reasoning chunk
    let mut chunk_r = make_chunk(None, None);
    chunk_r.choices[0].delta.reasoning_content = Some("thinking".into());
    let er = conv.on_chunk(&chunk_r);

    // reasoning: added + delta
    let names_r: Vec<&str> = er.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(
        names_r[1..],
        [
            "response.output_item.added",
            "response.reasoning_summary_text.delta"
        ]
    );
    let r_added: Value = serde_json::from_str(&er[1].1).unwrap();
    assert_eq!(r_added["item"]["type"], "reasoning");
    assert!(r_added["item"]["id"].as_str().unwrap().starts_with("rs_"));
    assert_eq!(r_added["output_index"], 0);
    let r_delta: Value = serde_json::from_str(&er[2].1).unwrap();
    assert_eq!(r_delta["type"], "response.reasoning_summary_text.delta");
    assert_eq!(r_delta["summary_index"], 0);
    assert_eq!(r_delta["output_index"], 0);

    // Text chunk
    let et = conv.on_chunk(&make_chunk(Some("answer"), None));
    // text: added + delta (message gets output_index 1)
    let t_added: Value = serde_json::from_str(&et[0].1).unwrap();
    assert_eq!(t_added["item"]["type"], "message");
    assert_eq!(t_added["output_index"], 1);

    // Finish: reasoning.done + message.done + completed
    conv.on_chunk(&make_chunk(None, Some("stop")));
    let ef = conv.finish();
    let names_f: Vec<&str> = ef.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(
        names_f,
        vec![
            "response.output_item.done",
            "response.output_item.done",
            "response.completed",
        ]
    );
    // First done = reasoning (output_index 0)
    let r_done: Value = serde_json::from_str(&ef[0].1).unwrap();
    assert_eq!(r_done["output_index"], 0);
    assert_eq!(r_done["item"]["type"], "reasoning");
    // Second done = message (output_index 1)
    let m_done: Value = serde_json::from_str(&ef[1].1).unwrap();
    assert_eq!(m_done["output_index"], 1);
    assert_eq!(m_done["item"]["content"][0]["text"], "answer");
}

#[test]
fn tool_only_no_ghost_message_item() {
    // Pure tool-call response: no text, must NOT create a message item.
    let mut conv = StreamConverter::new(
        "r".into(),
        "m".into(),
        HealGates::default(),
        NamespaceToolMap::new(),
    );

    let mut chunk1 = make_chunk(None, None);
    chunk1.choices[0].delta.tool_calls = Some(vec![DeltaToolCall {
        index: 0,
        id: Some("call_1".into()),
        function: Some(DeltaFunction {
            name: Some("exec".into()),
            arguments: Some("{\"x\":".into()),
        }),
    }]);
    let e1 = conv.on_chunk(&chunk1);

    // Only response.created, no output_item.added yet (lazy)
    let names1: Vec<&str> = e1.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(names1, vec!["response.created"]);

    let mut chunk2 = make_chunk(None, Some("tool_calls"));
    chunk2.choices[0].delta.tool_calls = Some(vec![DeltaToolCall {
        index: 0,
        function: Some(DeltaFunction {
            arguments: Some("1}".into()),
            ..Default::default()
        }),
        ..Default::default()
    }]);
    chunk2.usage = Some(ChatUsage {
        prompt_tokens: 5,
        completion_tokens: 3,
        total_tokens: 8,
    });
    assert!(conv.on_chunk(&chunk2).is_empty());
    let ef = conv.finish();

    // Finish: added(fc) + delta(args) + done(fc) + completed
    let names_f: Vec<&str> = ef.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(
        names_f,
        vec![
            "response.output_item.added",
            "response.function_call_arguments.delta",
            "response.output_item.done",
            "response.completed",
        ]
    );

    // The added item is a function_call, NOT a message
    let fc_added: Value = serde_json::from_str(&ef[0].1).unwrap();
    assert_eq!(fc_added["item"]["type"], "function_call");
    assert_eq!(fc_added["item"]["call_id"], "call_1");
    assert_eq!(fc_added["item"]["name"], "exec");
    assert_eq!(fc_added["item"]["status"], "in_progress");

    // Arguments delta carries full accumulated arguments
    let args_delta: Value = serde_json::from_str(&ef[1].1).unwrap();
    assert_eq!(args_delta["delta"], "{\"x\":1}");

    // Done item has completed status + full arguments
    let fc_done: Value = serde_json::from_str(&ef[2].1).unwrap();
    assert_eq!(fc_done["item"]["status"], "completed");
    assert_eq!(fc_done["item"]["arguments"], "{\"x\":1}");

    // Session has only the function_call item, no message
    assert_eq!(conv.acc.items.len(), 1);
    assert_eq!(conv.acc.items[0]["type"], "function_call");
}

#[test]
fn text_and_tool_call_combined() {
    let mut conv = StreamConverter::new(
        "r".into(),
        "m".into(),
        HealGates::default(),
        NamespaceToolMap::new(),
    );

    // Text first
    conv.on_chunk(&make_chunk(Some("running"), None));
    // Then tool call fragments
    let mut tc_chunk = make_chunk(None, None);
    tc_chunk.choices[0].delta.tool_calls = Some(vec![DeltaToolCall {
        index: 0,
        id: Some("c1".into()),
        function: Some(DeltaFunction {
            name: Some("exec".into()),
            arguments: Some("{}".into()),
        }),
    }]);
    conv.on_chunk(&tc_chunk);

    // Finish
    let mut fin = make_chunk(None, Some("tool_calls"));
    fin.usage = Some(ChatUsage {
        prompt_tokens: 1,
        completion_tokens: 1,
        total_tokens: 2,
    });
    conv.on_chunk(&fin);
    let ef = conv.finish();

    // Done sequence: message.done, then fc.added + fc.delta + fc.done, then completed
    let names: Vec<&str> = ef.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "response.output_item.done",  // message
            "response.output_item.added", // function_call
            "response.function_call_arguments.delta",
            "response.output_item.done", // function_call
            "response.completed",
        ]
    );

    // Session has both items
    assert_eq!(conv.acc.items.len(), 2);
    assert_eq!(conv.acc.items[0]["type"], "message");
    assert_eq!(conv.acc.items[1]["type"], "function_call");
}

#[test]
fn tool_name_resent_each_chunk_is_not_concatenated() {
    // Some OpenAI-compatible upstreams re-send the FULL function name on
    // every tool-call delta for an index. The accumulated name must stay
    // "exec", not become "execexec".
    let mut conv = StreamConverter::new(
        "r".into(),
        "m".into(),
        HealGates::default(),
        NamespaceToolMap::new(),
    );
    for args in ["{\"x\"", ":1}"] {
        let mut ch = make_chunk(None, None);
        ch.choices[0].delta.tool_calls = Some(vec![DeltaToolCall {
            index: 0,
            id: None,
            function: Some(DeltaFunction {
                name: Some("exec".into()),
                arguments: Some(args.into()),
            }),
        }]);
        conv.on_chunk(&ch);
    }
    conv.on_chunk(&make_chunk(None, Some("tool_calls")));
    conv.finish();
    assert_eq!(conv.acc.items.len(), 1);
    assert_eq!(conv.acc.items[0]["name"], "exec");
    assert_eq!(conv.acc.items[0]["arguments"], "{\"x\":1}");
}

#[test]
fn tool_name_fragments_are_appended() {
    // A name split across chunks like the arguments (non-extending
    // continuation) is still stitched together.
    let mut conv = StreamConverter::new(
        "r".into(),
        "m".into(),
        HealGates::default(),
        NamespaceToolMap::new(),
    );
    for name in ["get_", "weather"] {
        let mut ch = make_chunk(None, None);
        ch.choices[0].delta.tool_calls = Some(vec![DeltaToolCall {
            index: 0,
            id: Some("c1".into()),
            function: Some(DeltaFunction {
                name: Some(name.into()),
                arguments: None,
            }),
        }]);
        conv.on_chunk(&ch);
    }
    conv.on_chunk(&make_chunk(None, Some("tool_calls")));
    conv.finish();
    assert_eq!(conv.acc.items[0]["name"], "get_weather");
}

#[test]
fn tool_call_without_id_gets_synthesized_call_id() {
    // An upstream that omits tool-call ids must not produce
    // function_call items with call_id "" - the replayed assistant
    // tool_calls would carry id "" and strict Chat upstreams reject the
    // whole follow-up request. A call_<uuid> is synthesized instead, and
    // the client-visible event carries the same id.
    let mut conv = StreamConverter::new(
        "r".into(),
        "m".into(),
        HealGates::default(),
        NamespaceToolMap::new(),
    );
    let mut ch = make_chunk(None, None);
    ch.choices[0].delta.tool_calls = Some(vec![DeltaToolCall {
        index: 0,
        id: None,
        function: Some(DeltaFunction {
            name: Some("exec".into()),
            arguments: Some("{}".into()),
        }),
    }]);
    conv.on_chunk(&ch);
    conv.on_chunk(&make_chunk(None, Some("tool_calls")));
    let ef = conv.finish();

    let session_call_id = conv.acc.items[0]["call_id"].as_str().unwrap();
    assert!(session_call_id.starts_with("call_"), "{session_call_id}");

    // The client-visible function_call item carries the same call_id, so
    // the tool result the client sends back references it.
    let added: Value = serde_json::from_str(
        &ef.iter()
            .find(|(t, _)| t == "response.output_item.added")
            .unwrap()
            .1,
    )
    .unwrap();
    assert_eq!(added["item"]["call_id"].as_str().unwrap(), session_call_id);
}

#[test]
fn zero_payload_tool_call_deltas_produce_no_phantom_item() {
    // Some upstreams announce an index with a delta carrying neither
    // id nor function content, and an id-only variant exists too. Neither
    // may become a client-visible function_call with name "" /
    // arguments "{}" - the client would try to execute an unnamed
    // function and replay would send a bogus tool_call upstream.
    let mut conv = StreamConverter::new(
        "r".into(),
        "m".into(),
        HealGates::default(),
        NamespaceToolMap::new(),
    );
    // Announcement-only delta: no id, no function payload.
    let mut announce = make_chunk(None, None);
    announce.choices[0].delta.tool_calls = Some(vec![DeltaToolCall {
        index: 0,
        id: None,
        function: None,
    }]);
    assert!(!conv
        .on_chunk(&announce)
        .iter()
        .any(|(t, _)| t == "response.output_item.added"));
    // Id-only delta: id present, but no name and no arguments ever arrive.
    let mut id_only = make_chunk(None, Some("tool_calls"));
    id_only.choices[0].delta.tool_calls = Some(vec![DeltaToolCall {
        index: 1,
        id: Some("call_x".into()),
        function: Some(DeltaFunction::default()),
    }]);
    conv.on_chunk(&id_only);
    let ef = conv.finish();

    // No function_call items emitted or stored.
    assert!(
        !ef.iter().any(|(t, _)| t == "response.output_item.added"),
        "unexpected output_item.added: {ef:?}"
    );
    assert!(conv.acc.items.is_empty());
    // The completed event still fires with an empty output.
    let completed: Value = serde_json::from_str(
        &ef.iter()
            .find(|(t, _)| t == "response.completed")
            .unwrap()
            .1,
    )
    .unwrap();
    assert_eq!(completed["response"]["output"].as_array().unwrap().len(), 0);
}

#[test]
fn on_error_format() {
    let conv = StreamConverter::new(
        "resp_1".into(),
        "m".into(),
        HealGates::default(),
        NamespaceToolMap::new(),
    );
    let events = conv.on_error("boom");
    assert_eq!(events[0].0, "error");
    let err: Value = serde_json::from_str(&events[0].1).unwrap();
    assert!(err["message"].as_str().unwrap().contains("boom"));

    assert_eq!(events[1].0, "response.failed");
    let failed: Value = serde_json::from_str(&events[1].1).unwrap();
    assert_eq!(failed["type"], "response.failed");
    assert_eq!(failed["response"]["id"], "resp_1");
    assert_eq!(failed["response"]["status"], "failed");
}

#[test]
fn completed_uses_accumulated_usage_when_finish_omits_it() {
    let mut conv = StreamConverter::new(
        "r".into(),
        "m".into(),
        HealGates::default(),
        NamespaceToolMap::new(),
    );
    let usage_chunk = ChatStreamChunk {
        choices: vec![ChatStreamChoice {
            index: 0,
            delta: ChatDelta {
                content: Some("hi".into()),
                ..Default::default()
            },
            finish_reason: None,
        }],
        usage: Some(ChatUsage {
            prompt_tokens: 4,
            completion_tokens: 2,
            total_tokens: 6,
        }),
    };
    conv.on_chunk(&usage_chunk);
    conv.on_chunk(&make_chunk(None, Some("stop")));
    let finish_events = conv.finish();
    let completed = finish_events
        .iter()
        .find(|(t, _)| t == "response.completed")
        .expect("completed event");
    let val: Value = serde_json::from_str(&completed.1).unwrap();
    assert_eq!(val["response"]["usage"]["total_tokens"], 6);
}

#[test]
fn trailing_usage_chunk_after_finish_included_in_completed() {
    // include_usage delivers usage in a final chunk with EMPTY choices
    // AFTER the finish_reason chunk. response.completed must carry it,
    // not zeros: the finish sequence is deferred to stream end.
    let mut conv = StreamConverter::new(
        "r".into(),
        "m".into(),
        HealGates::default(),
        NamespaceToolMap::new(),
    );
    conv.on_chunk(&make_chunk(Some("hi"), Some("stop")));
    let trailing = ChatStreamChunk {
        choices: vec![],
        usage: Some(ChatUsage {
            prompt_tokens: 11,
            completion_tokens: 7,
            total_tokens: 18,
        }),
    };
    assert!(conv.on_chunk(&trailing).is_empty());
    let ef = conv.finish();
    let completed: Value = serde_json::from_str(
        &ef.iter()
            .find(|(t, _)| t == "response.completed")
            .unwrap()
            .1,
    )
    .unwrap();
    assert_eq!(completed["response"]["usage"]["input_tokens"], 11);
    assert_eq!(completed["response"]["usage"]["output_tokens"], 7);
    assert_eq!(completed["response"]["usage"]["total_tokens"], 18);
}

#[test]
fn text_before_reasoning_session_items_canonical_order() {
    // Text deltas arriving BEFORE reasoning deltas give the message the
    // lower output_index, so client done events fire message-first. The
    // session items must still be stored reasoning-first (canonical turn
    // order), or history replay leaves the reasoning dangling after the
    // message (dropped / misattached on the next turn).
    let mut conv = StreamConverter::new(
        "r".into(),
        "m".into(),
        HealGates::default(),
        NamespaceToolMap::new(),
    );
    conv.on_chunk(&make_chunk(Some("answer"), None)); // message idx 0
    let mut chunk_r = make_chunk(None, None);
    chunk_r.choices[0].delta.reasoning_content = Some("thinking".into());
    conv.on_chunk(&chunk_r); // reasoning idx 1
    conv.on_chunk(&make_chunk(None, Some("stop")));
    let ef = conv.finish();

    // Client events follow output_index: message done (0), then
    // reasoning done (1).
    let done_events: Vec<Value> = ef
        .iter()
        .filter(|(t, _)| t == "response.output_item.done")
        .map(|(_, d)| serde_json::from_str::<Value>(d).unwrap())
        .collect();
    assert_eq!(done_events.len(), 2);
    assert_eq!(done_events[0]["output_index"], 0);
    assert_eq!(done_events[0]["item"]["type"], "message");
    assert_eq!(done_events[1]["output_index"], 1);
    assert_eq!(done_events[1]["item"]["type"], "reasoning");

    // Session storage is canonical: reasoning BEFORE message.
    assert_eq!(conv.acc.items.len(), 2);
    assert_eq!(conv.acc.items[0]["type"], "reasoning");
    assert_eq!(conv.acc.items[1]["type"], "message");
}

#[test]
fn finish_requires_finish_reason_and_is_idempotent() {
    let mut conv = StreamConverter::new(
        "r".into(),
        "m".into(),
        HealGates::default(),
        NamespaceToolMap::new(),
    );
    conv.on_chunk(&make_chunk(Some("hi"), None));
    // No finish_reason seen: finish() emits nothing (caller errors out).
    assert!(conv.finish().is_empty());
    conv.on_chunk(&make_chunk(None, Some("stop")));
    assert!(!conv.finish().is_empty());
    // A second finish() must not re-emit events or duplicate items.
    assert!(conv.finish().is_empty());
    assert_eq!(conv.acc.items.len(), 1);
}

#[test]
fn finish_reason_accessor_returns_last_reason() {
    let mut conv = StreamConverter::new(
        "resp_x".into(),
        "m".into(),
        HealGates::default(),
        NamespaceToolMap::new(),
    );
    assert!(conv.finish_reason().is_none());
    let chunk: ChatStreamChunk =
        serde_json::from_str(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#)
            .unwrap();
    conv.on_chunk(&chunk);
    assert_eq!(conv.finish_reason(), Some("stop"));
    // A chunk with finish_reason null must not reset the field.
    let null_chunk: ChatStreamChunk =
        serde_json::from_str(r#"{"choices":[{"index":0,"delta":{},"finish_reason":null}]}"#)
            .unwrap();
    conv.on_chunk(&null_chunk);
    assert_eq!(conv.finish_reason(), Some("stop"));
    // A later non-null finish_reason wins.
    let length_chunk: ChatStreamChunk =
        serde_json::from_str(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"length"}]}"#)
            .unwrap();
    conv.on_chunk(&length_chunk);
    assert_eq!(conv.finish_reason(), Some("length"));
}

#[test]
fn multiple_tool_calls_emitted_in_index_order() {
    let mut conv = StreamConverter::new(
        "r".into(),
        "m".into(),
        HealGates::default(),
        NamespaceToolMap::new(),
    );

    // Deliver index 1 before index 0
    let mut ch = make_chunk(None, None);
    ch.choices[0].delta.tool_calls = Some(vec![DeltaToolCall {
        index: 1,
        id: Some("c2".into()),
        function: Some(DeltaFunction {
            name: Some("b".into()),
            arguments: Some("{}".into()),
        }),
    }]);
    conv.on_chunk(&ch);

    let mut ch2 = make_chunk(None, Some("tool_calls"));
    ch2.choices[0].delta.tool_calls = Some(vec![DeltaToolCall {
        index: 0,
        id: Some("c1".into()),
        function: Some(DeltaFunction {
            name: Some("a".into()),
            arguments: Some("{}".into()),
        }),
    }]);
    conv.on_chunk(&ch2);
    let ef = conv.finish();

    // Find the two "added" events and verify order
    let added_items: Vec<Value> = ef
        .iter()
        .filter(|(t, _)| t == "response.output_item.added")
        .map(|(_, d)| serde_json::from_str::<Value>(d).unwrap())
        .collect();
    assert_eq!(added_items.len(), 2);
    // Index 0 first, index 1 second
    assert_eq!(added_items[0]["item"]["call_id"], "c1");
    assert_eq!(added_items[1]["item"]["call_id"], "c2");

    // Session items also in index order
    let fc_items: Vec<_> = conv
        .acc
        .items
        .iter()
        .filter(|i| i["type"] == "function_call")
        .collect();
    assert_eq!(fc_items[0]["call_id"], "c1");
    assert_eq!(fc_items[1]["call_id"], "c2");
}
use crate::heal::HealGates;

fn make_reasoning_chunk(text: &str) -> ChatStreamChunk {
    let mut c = make_chunk(None, None);
    c.choices[0].delta.reasoning_content = Some(text.into());
    c
}

fn event_names(events: &[(String, String)]) -> Vec<&str> {
    events.iter().map(|(t, _)| t.as_str()).collect()
}

#[test]
fn think_healed_content_routes_to_reasoning_channel() {
    let mut conv = StreamConverter::new(
        "resp_h1".into(),
        "m".into(),
        HealGates::default(),
        NamespaceToolMap::new(),
    );
    // Eager start (as the proxy drives the converter): consumes the lazy
    // `response.created` so the assertions below see only content events.
    conv.start();
    let e = conv.on_chunk(&make_chunk(Some("<think>musing</think>Hi"), None));
    assert_eq!(
        event_names(&e),
        vec![
            "response.output_item.added", // reasoning item
            "response.reasoning_summary_text.delta",
            "response.output_item.added", // message item
            "response.output_text.delta",
        ]
    );
    let delta0: Value = serde_json::from_str(&e[1].1).unwrap();
    assert_eq!(delta0["delta"], "musing");
    let delta1: Value = serde_json::from_str(&e[3].1).unwrap();
    assert_eq!(delta1["delta"], "Hi");
    assert_eq!(conv.acc.reasoning, "musing");
    assert_eq!(conv.acc.text, "Hi");
}

#[test]
fn native_reasoning_precedes_think_healed_reasoning() {
    let mut conv = StreamConverter::new(
        "resp_h2".into(),
        "m".into(),
        HealGates::default(),
        NamespaceToolMap::new(),
    );
    // Eager start (as the proxy drives the converter): consumes the lazy
    // `response.created` so the assertions below see only content events.
    conv.start();
    // Native reasoning arrives on one chunk, healed on the next.
    let e1 = conv.on_chunk(&make_reasoning_chunk("native "));
    let e2 = conv.on_chunk(&make_chunk(Some("<think>healed</think>ok"), None));
    assert_eq!(
        event_names(&e1),
        vec![
            "response.output_item.added",
            "response.reasoning_summary_text.delta"
        ]
    );
    let d2: Value = serde_json::from_str(&e2[0].1).unwrap();
    assert_eq!(d2["delta"], "healed");
    assert_eq!(conv.acc.reasoning, "native healed");
}

#[test]
fn split_think_tag_across_chunks_is_recognized() {
    let mut conv = StreamConverter::new(
        "resp_h3".into(),
        "m".into(),
        HealGates::default(),
        NamespaceToolMap::new(),
    );
    // Eager start (as the proxy drives the converter): consumes the lazy
    // `response.created` so the assertions below see only content events.
    conv.start();
    let _ = conv.on_chunk(&make_chunk(Some("<thi"), None));
    let e = conv.on_chunk(&make_chunk(Some("nk>musing</think>Hi"), None));
    // The reasoning item is lazily created on THIS chunk, so the delta
    // lands at e[1] (e[0] is the output_item.added).
    let d_reason: Value = serde_json::from_str(&e[1].1).unwrap();
    assert_eq!(d_reason["delta"], "musing");
    assert_eq!(conv.acc.text, "Hi");
}

#[test]
fn dsml_markup_is_withheld_during_streaming() {
    let mut conv = StreamConverter::new(
        "resp_h4".into(),
        "m".into(),
        HealGates::default(),
        NamespaceToolMap::new(),
    );
    // Eager start (as the proxy drives the converter): consumes the lazy
    // `response.created` so the assertions below see only content events.
    conv.start();
    // Prefix + full single-bar envelope: the visible prefix is emitted
    // immediately, but the DSML markup itself is withheld until finish.
    let e = conv.on_chunk(&make_chunk(Some(
            "我先看文件。\n<｜DSML｜tool_calls>\n<｜DSML｜invoke name=\"shell\">\n<｜DSML｜parameter name=\"command\" string=\"true\">ls</｜DSML｜parameter>\n</｜DSML｜invoke>\n</｜DSML｜",
        ), None));
    assert_eq!(
        event_names(&e),
        vec!["response.output_item.added", "response.output_text.delta"]
    );
    let d: Value = serde_json::from_str(&e[1].1).unwrap();
    assert_eq!(d["delta"], "我先看文件。\n");
    let _ = conv.on_chunk(&make_chunk(Some("tool_calls>"), None));
    assert_eq!(conv.acc.text, "我先看文件。\n"); // markup withheld; healed only at finish (Task 5)
}

#[test]
fn disabled_heal_gates_pass_content_through_untouched() {
    let gates = HealGates {
        dsml: false,
        think: false,
    };
    let mut conv =
        StreamConverter::new("resp_h5".into(), "m".into(), gates, NamespaceToolMap::new());
    // Eager start (as the proxy drives the converter): consumes the lazy
    // `response.created` so the assertions below see only content events.
    conv.start();
    let e = conv.on_chunk(&make_chunk(Some("<think>musing</think>Hi"), None));
    // Both events target the message item - markup stayed in text.
    assert_eq!(
        event_names(&e),
        vec!["response.output_item.added", "response.output_text.delta"]
    );
    let d: Value = serde_json::from_str(&e[1].1).unwrap();
    assert_eq!(d["delta"], "<think>musing</think>Hi");
    assert_eq!(conv.acc.text, "<think>musing</think>Hi");
}

#[test]
fn finish_flushes_healed_dsml_calls_as_function_call_items() {
    let mut conv = StreamConverter::new(
        "resp_f1".into(),
        "m".into(),
        HealGates::default(),
        NamespaceToolMap::new(),
    );
    conv.start();
    let _ = conv.on_chunk(&make_chunk(Some(
            "我先看文件。\n<｜DSML｜tool_calls>\n<｜DSML｜invoke name=\"shell\">\n<｜DSML｜parameter name=\"command\" string=\"true\">ls</｜DSML｜parameter>\n</｜DSML｜invoke>\n</｜DSML｜tool_calls>",
        ), None));
    let _ = conv.on_chunk(&make_chunk(None, Some("stop")));
    let events = conv.finish();
    // The visible text prefix (the fixture's first line above) was
    // already emitted by on_chunk (heal.rs pins streaming == blocking
    // visible text), so finish() emits the message's done event, then
    // the healed call's function_call sequence, then completed - no
    // DSML residue anywhere.
    assert_eq!(
        event_names(&events),
        vec![
            "response.output_item.done",  // message (index 0)
            "response.output_item.added", // function_call (index 1)
            "response.function_call_arguments.delta",
            "response.output_item.done", // function_call (index 1)
            "response.completed",
        ]
    );
    let args: Value = serde_json::from_str(&events[2].1).unwrap();
    assert_eq!(args["delta"], "{\"command\":\"ls\"}");
    // Session items: message + function_call (no DSML residue).
    let kinds: Vec<&str> = conv
        .acc
        .items
        .iter()
        .map(|i| i["type"].as_str().unwrap_or(""))
        .collect();
    assert!(kinds.contains(&"function_call"), "items: {kinds:?}");
    assert!(!conv.acc.text.contains("DSML"));
}

#[test]
fn finish_flushes_think_tail_before_done_events() {
    let mut conv = StreamConverter::new(
        "resp_f2".into(),
        "m".into(),
        HealGates::default(),
        NamespaceToolMap::new(),
    );
    conv.start();
    // Unterminated think block: the partial close tag `</th` is withheld
    // during streaming. At finish the still-in-think filter puts it on the
    // reasoning channel BEFORE the done/completed sequence - never as
    // visible text.
    let _ = conv.on_chunk(&make_chunk(Some("<think>cut off</th"), None));
    let _ = conv.on_chunk(&make_chunk(None, Some("stop")));
    let events = conv.finish();
    let delta: Value = serde_json::from_str(&events[0].1).unwrap();
    assert_eq!(delta["type"], "response.reasoning_summary_text.delta");
    assert_eq!(delta["delta"], "</th");
    assert_eq!(conv.acc.reasoning, "cut off</th");
    assert_eq!(conv.acc.text, "");
}
