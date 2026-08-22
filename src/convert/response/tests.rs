//! Unit tests for tests module, extracted from `response.rs`
//! (module-split spec Phase 1; bodies are verbatim moves).
use super::*;

#[test]
fn converts_text_response() {
    let resp = ChatResponse {
        choices: vec![ChatChoice {
            message: ChatMessage {
                role: "assistant".into(),
                content: Some(Value::String("hello world".into())),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
        }],
        usage: Some(ChatUsage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
        }),
    };
    let items = chat_response_to_items(&resp, &NamespaceToolMap::new());
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["type"], "message");
    assert_eq!(items[0]["content"][0]["text"], "hello world");
}

#[test]
fn converts_reasoning_and_text() {
    let resp = ChatResponse {
        choices: vec![ChatChoice {
            message: ChatMessage {
                role: "assistant".into(),
                content: Some(Value::String("answer".into())),
                reasoning_content: Some("thinking...".into()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
        }],
        usage: None,
    };
    let items = chat_response_to_items(&resp, &NamespaceToolMap::new());
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["type"], "reasoning");
    assert_eq!(items[1]["type"], "message");
}

#[test]
fn converts_tool_calls() {
    let resp = ChatResponse {
        choices: vec![ChatChoice {
            message: ChatMessage {
                role: "assistant".into(),
                content: None,
                reasoning_content: None,
                tool_calls: Some(vec![json!({
                    "id": "call_1",
                    "type": "function",
                    "function": { "name": "exec", "arguments": "{\"cmd\":\"ls\"}" }
                })]),
                tool_call_id: None,
                name: None,
            },
        }],
        usage: None,
    };
    let items = chat_response_to_items(&resp, &NamespaceToolMap::new());
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["type"], "function_call");
    assert_eq!(items[0]["call_id"], "call_1");
    assert_eq!(items[0]["name"], "exec");
}

#[test]
fn empty_content_produces_no_message_item() {
    let resp = ChatResponse {
        choices: vec![ChatChoice {
            message: ChatMessage {
                role: "assistant".into(),
                content: None,
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
        }],
        usage: None,
    };
    let items = chat_response_to_items(&resp, &NamespaceToolMap::new());
    assert!(items.is_empty());
}

#[test]
fn tool_call_missing_fields_uses_fallbacks() {
    let resp = ChatResponse {
        choices: vec![ChatChoice {
            message: ChatMessage {
                role: "assistant".into(),
                content: None,
                reasoning_content: None,
                tool_calls: Some(vec![json!({})]),
                tool_call_id: None,
                name: None,
            },
        }],
        usage: None,
    };
    let items = chat_response_to_items(&resp, &NamespaceToolMap::new());
    assert_eq!(items.len(), 1);
    // Missing id synthesizes call_<uuid>; name/arguments
    // keep their old fallbacks.
    assert!(items[0]["call_id"].as_str().unwrap().starts_with("call_"));
    assert_eq!(items[0]["name"], "");
    assert_eq!(items[0]["arguments"], "{}");
}

#[test]
fn chat_response_to_items_decodes_namespace_tool_call() {
    let mut map = NamespaceToolMap::new();
    map.insert(
        "multi_agent_v1-spawn_agent".to_string(),
        NamespaceToolName {
            namespace: "multi_agent_v1".to_string(),
            name: "spawn_agent".to_string(),
        },
    );
    let resp: ChatResponse = serde_json::from_value(json!({
        "id": "x",
        "object": "chat.completion",
        "created": 0,
        "model": "m",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "multi_agent_v1-spawn_agent", "arguments": "{}"}
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    }))
    .unwrap();
    let items = chat_response_to_items(&resp, &map);
    let fc = items
        .iter()
        .find(|i| i["type"] == "function_call")
        .expect("function_call item");
    assert_eq!(fc["name"], "spawn_agent");
    assert_eq!(fc["namespace"], "multi_agent_v1");
}

#[test]
fn chat_response_to_items_keeps_flat_name_when_not_in_map() {
    let resp: ChatResponse = serde_json::from_value(json!({
        "id": "x",
        "object": "chat.completion",
        "created": 0,
        "model": "m",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "exec_command", "arguments": "{\"cmd\":\"ls\"}"}
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    }))
    .unwrap();
    let items = chat_response_to_items(&resp, &NamespaceToolMap::new());
    let fc = items
        .iter()
        .find(|i| i["type"] == "function_call")
        .expect("function_call item");
    assert_eq!(fc["name"], "exec_command");
    assert!(fc.get("namespace").is_none());
}

#[test]
fn completed_response_maps_usage() {
    let usage = ChatUsage {
        prompt_tokens: 7,
        completion_tokens: 3,
        total_tokens: 10,
    };
    let out = build_completed_response("resp_1", "m", &[json!({"type": "message"})], Some(&usage));
    assert_eq!(out["id"], "resp_1");
    assert_eq!(out["object"], "response");
    assert_eq!(out["status"], "completed");
    assert_eq!(out["model"], "m");
    assert_eq!(out["output"][0]["type"], "message");
    assert!(out["output"][0]["id"].as_str().unwrap().starts_with("msg_"));
    assert_eq!(out["usage"]["input_tokens"], 7);
    assert_eq!(out["usage"]["output_tokens"], 3);
    assert_eq!(out["usage"]["total_tokens"], 10);
}

#[test]
fn completed_response_without_usage_zeros() {
    let out = build_completed_response("resp_1", "m", &[], None);
    assert_eq!(out["usage"]["input_tokens"], 0);
    assert_eq!(out["usage"]["output_tokens"], 0);
    assert_eq!(out["usage"]["total_tokens"], 0);
}
