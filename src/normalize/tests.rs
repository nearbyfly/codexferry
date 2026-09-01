//! Unit tests for tests module, extracted from `normalize.rs`
//! (module-split spec Phase 1; bodies are verbatim moves).
use super::*;
use serde_json::json;
use std::sync::Mutex;

/// Serializes tests that assert on the process-global unknown-type counter
/// map. Most tests are parallel-safe via unique probe names, but the cap
/// test FILLS the map to its limit, which would silently untrack the other
/// tests' probes if interleaved.
static COUNTER_TESTS: Mutex<()> = Mutex::new(());

/// The shape Codex 0.147 sends: an `additional_tools` input item wrapping
/// `namespace` entries whose inner entries are plain function tools, with
/// the top-level `tools` array empty.
#[test]
fn hoists_namespace_wrapped_tools_from_additional_tools_items() {
    let items = vec![json!({
        "type": "additional_tools",
        "role": "developer",
        "tools": [
            { "type": "namespace", "name": "functions", "tools": [
                { "type": "function", "name": "exec_command", "parameters": {"type": "object"} },
                { "type": "function", "name": "write_stdin" }
            ]},
            { "type": "function", "name": "plain_tool" }
        ]
    })];
    let mut tools = Vec::new();
    let (_unmappable, _schema_conflicts) =
        collect_additional_tools_reporting(&items, &mut tools, None);
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["exec_command", "write_stdin", "plain_tool"]);
}

#[test]
fn hoisting_is_name_deduplicated_across_repeats() {
    let item = json!({
        "type": "additional_tools",
        "tools": [{ "type": "function", "name": "exec_command" }]
    });
    let items = vec![item.clone(), item];
    let mut tools = vec![json!({ "type": "function", "name": "exec_command" })];
    let (_unmappable, _schema_conflicts) =
        collect_additional_tools_reporting(&items, &mut tools, None);
    assert_eq!(tools.len(), 1);
}

#[test]
fn stripping_removes_only_additional_tools_items() {
    let mut items = vec![
        json!({"type": "message", "role": "user", "content": []}),
        json!({"type": "additional_tools", "tools": []}),
    ];
    strip_additional_tools_items(&mut items);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["type"], "message");
}

#[test]
fn non_additional_items_are_ignored() {
    let items = vec![json!({
        "type": "message",
        "role": "user",
        "content": [{ "type": "input_text", "text": "hi" }]
    })];
    let mut tools = Vec::new();
    let (_unmappable, _schema_conflicts) =
        collect_additional_tools_reporting(&items, &mut tools, None);
    assert!(tools.is_empty());
}

/// The counters are process-global; each test uses a unique probe type
/// name so parallel tests never interfere.
#[test]
fn unknown_item_types_counted_once_per_call_and_known_ignored() {
    let _guard = COUNTER_TESTS.lock().unwrap();
    let probe = "normalize_test_unknown_a";
    let before = unknown_type_counts().get(probe).copied().unwrap_or(0);
    let items = [
        json!({"type": probe}),
        json!({"type": "message", "role": "user", "content": []}),
        json!({"type": probe}), // Duplicate type in one call: counted once.
    ];
    warn_unknown_item_types(items.iter(), "passed through");
    let after = unknown_type_counts().get(probe).copied().unwrap_or(0);
    assert_eq!(after, before + 1);
    // Known types are never counted.
    assert_eq!(unknown_type_counts().get("message"), None);
}

#[test]
fn known_only_items_never_bump_counters() {
    let before = unknown_type_counts()
        .get("normalize_test_unknown_b")
        .copied()
        .unwrap_or(0);
    let items = [
        json!({"type": "reasoning"}),
        json!({"type": "function_call"}),
        json!({"type": "function_call_output"}),
    ];
    warn_unknown_item_types(items.iter(), "passed through");
    let after = unknown_type_counts()
        .get("normalize_test_unknown_b")
        .copied()
        .unwrap_or(0);
    assert_eq!(after, before);
    // Known types never enter the counter table either.
    for known in ["reasoning", "function_call", "function_call_output"] {
        assert_eq!(
            unknown_type_counts().get(known),
            None,
            "known type {known} must not be counted"
        );
    }
}

// ---- responses request entrypoint ----

#[test]
fn responses_request_hoists_and_passes_unknown_through() {
    let mut obj: serde_json::Map<String, Value> = serde_json::from_value(json!({
        "model": "m",
        "tools": [],
        "input": [
            {"type": "additional_tools", "tools": [
                {"type": "namespace", "name": "functions", "tools": [
                    {"type": "function", "name": "exec_command"}
                ]}
            ]},
            {"type": "message", "role": "user", "content": []},
            {"type": "normalize_test_dialect_c"}
        ]
    }))
    .unwrap();
    normalize_responses_request(&mut obj);
    // Hoisted into the top-level tools.
    let tools = obj["tools"].as_array().unwrap();
    assert_eq!(tools[0]["name"], "exec_command");
    // additional_tools item stripped; unknown item passed through verbatim.
    let input = obj["input"].as_array().unwrap();
    let types: Vec<&str> = input.iter().map(|i| i["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["message", "normalize_test_dialect_c"]);
}

#[test]
fn responses_request_without_input_is_noop() {
    let mut obj: serde_json::Map<String, Value> = serde_json::from_value(json!({
        "model": "m", "input": "plain string"
    }))
    .unwrap();
    normalize_responses_request(&mut obj);
    assert_eq!(obj["input"], json!("plain string"));
}

// ---- chat tool flattening ----

#[test]
fn chat_tools_flattens_and_encodes_namespace_entries() {
    let tools = vec![
        json!({"type": "function", "name": "exec_command", "parameters": {}}),
        json!({"type": "namespace", "name": "multi_agent_v1", "tools": [
            {"type": "function", "name": "get_goal"},
            {"type": "function", "name": "spawn_agent"},
        ]}),
        json!({"type": "web_search"}),
    ];
    let (flat, map) = normalize_chat_tools(&tools);
    let names: Vec<&str> = flat.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "exec_command",
            "multi_agent_v1-get_goal",
            "multi_agent_v1-spawn_agent",
        ]
    );
    assert_eq!(map.len(), 2);
    assert_eq!(map["multi_agent_v1-spawn_agent"].name, "spawn_agent");
}

#[test]
fn chat_tools_keeps_flat_tool_names_and_empty_map_for_no_namespaces() {
    let tools = vec![
        json!({"type": "function", "name": "exec_command", "parameters": {}}),
        json!({"type": "function", "name": "view_image", "parameters": {}}),
    ];
    let (flat, map) = normalize_chat_tools(&tools);
    let names: Vec<&str> = flat.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["exec_command", "view_image"]);
    assert!(map.is_empty());
}

#[test]
fn chat_tools_cross_namespace_same_name_both_kept() {
    let tools = vec![
        json!({"type": "namespace", "name": "multi_agent_v1", "tools": [
            {"type": "function", "name": "get_goal"},
        ]}),
        json!({"type": "namespace", "name": "collaboration", "tools": [
            {"type": "function", "name": "get_goal"},
        ]}),
    ];
    let (flat, map) = normalize_chat_tools(&tools);
    let names: Vec<&str> = flat.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["multi_agent_v1-get_goal", "collaboration-get_goal"]);
    assert_eq!(map.len(), 2);
}

#[test]
fn chat_tools_encoded_name_collision_first_wins() {
    // Two namespace/name combos colliding to the same encoded name: the
    // first declaration wins in both the flattened tools and the decode map.
    let tools = vec![
        json!({"type": "namespace", "name": "a", "tools": [
            {"type": "function", "name": "b-c"},
        ]}),
        json!({"type": "namespace", "name": "a-b", "tools": [
            {"type": "function", "name": "c"},
        ]}),
    ];
    let (flat, map) = normalize_chat_tools(&tools);
    assert_eq!(flat.len(), 1);
    let ns = map.get("a-b-c").expect("encoded name key");
    assert_eq!(ns.namespace, "a");
    assert_eq!(ns.name, "b-c");
}

#[test]
fn chat_tools_keeps_hyphen_in_tool_name_encoded_once() {
    let tools = vec![
        json!({"type": "namespace", "name": "multi_agent_v1", "tools": [
            {"type": "function", "name": "spawn-agent"},
        ]}),
    ];
    let (flat, map) = normalize_chat_tools(&tools);
    assert_eq!(flat[0]["name"], "multi_agent_v1-spawn-agent");
    let ns = map
        .get("multi_agent_v1-spawn-agent")
        .expect("encoded name key");
    assert_eq!(ns.name, "spawn-agent");
}

#[test]
fn chat_tools_top_level_flat_tool_not_in_map() {
    let tools = vec![
        json!({"type": "function", "name": "exec_command", "parameters": {}}),
        json!({"type": "namespace", "name": "multi_agent_v1", "tools": [
            {"type": "function", "name": "spawn_agent"},
        ]}),
    ];
    let (flat, map) = normalize_chat_tools(&tools);
    let names: Vec<&str> = flat.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["exec_command", "multi_agent_v1-spawn_agent"]);
    assert!(!map.contains_key("exec_command"));
    assert_eq!(map.len(), 1);
}

#[test]
fn chat_tools_drop_unmappable_types_with_counter() {
    let probe = "normalize_test_tool_d";
    let before = unknown_type_counts().get(probe).copied().unwrap_or(0);
    let tools = vec![json!({"type": probe})];
    assert!(normalize_chat_tools(&tools).0.is_empty());
    let after = unknown_type_counts().get(probe).copied().unwrap_or(0);
    assert_eq!(after, before + 1);
}

#[test]
fn chat_tools_flatten_dedupes_by_name() {
    // Dedup is by ENCODED name: a top-level flat tool already named
    // `{namespace}-{name}` and a namespace-inner tool encoding to the
    // same string are ONE tool to the upstream — the first declaration
    // wins, so the flattened list keeps 2 entries, not 3. The flat
    // winner's name gets NO decode mapping (only the appended
    // spawn_agent does).
    let tools = vec![
        json!({"type": "function", "name": "multi_agent_v1-get_goal"}),
        json!({"type": "namespace", "name": "multi_agent_v1", "tools": [
            {"type": "function", "name": "get_goal"},
            {"type": "function", "name": "spawn_agent"},
        ]}),
    ];
    let (flat, map) = normalize_chat_tools(&tools);
    assert_eq!(flat.len(), 2);
    assert!(!map.contains_key("multi_agent_v1-get_goal"));
    assert!(map.contains_key("multi_agent_v1-spawn_agent"));
    assert_eq!(map.len(), 1);
}

#[test]
fn chat_tools_flat_first_collision_keeps_flat_and_no_map_entry() {
    let tools = vec![
        json!({"type": "function", "name": "multi_agent_v1-get_goal"}),
        json!({"type": "namespace", "name": "multi_agent_v1", "tools": [
            {"type": "function", "name": "get_goal"},
        ]}),
    ];
    let (flat, map) = normalize_chat_tools(&tools);
    assert_eq!(flat.len(), 1);
    assert_eq!(flat[0]["name"], "multi_agent_v1-get_goal");
    assert!(
        map.is_empty(),
        "map must not record a deduped (flat-bound) tool"
    );
}

#[test]
fn chat_tools_namespace_custom_tool_passes_unencoded() {
    let tools = vec![
        json!({"type": "namespace", "name": "multi_agent_v1", "tools": [
            {"type": "custom", "name": "my_tool"},
        ]}),
    ];
    let (flat, map) = normalize_chat_tools(&tools);
    assert_eq!(flat[0]["name"], "my_tool");
    assert!(map.is_empty());
}

#[test]
fn responses_request_never_counts_additional_tools_as_unknown() {
    // Pins the strip-before-warn ordering directly: the consumed
    // `additional_tools` item must never be counted as an unknown type,
    // because stripping removes it before the warn runs.
    let mut obj: serde_json::Map<String, Value> = serde_json::from_value(json!({
        "model": "m",
        "input": [{"type": "additional_tools", "tools": [
            {"type": "function", "name": "exec_command"}
        ]}]
    }))
    .unwrap();
    normalize_responses_request(&mut obj);
    assert!(!unknown_type_counts().contains_key("additional_tools"));
}

#[test]
fn responses_request_dedupes_against_existing_tools() {
    // Parity with the chat path: a tool bound both top-level and inside
    // `additional_tools` must not be double-bound upstream — the hoist
    // dedups against the pre-existing top-level tools.
    let mut obj: serde_json::Map<String, Value> = serde_json::from_value(json!({
        "model": "m",
        "tools": [{"type": "function", "name": "exec_command"}],
        "input": [{"type": "additional_tools", "tools": [
            {"type": "function", "name": "exec_command"}
        ]}]
    }))
    .unwrap();
    normalize_responses_request(&mut obj);
    let tools = obj["tools"].as_array().unwrap();
    let execs: Vec<&Value> = tools
        .iter()
        .filter(|t| t["name"] == "exec_command")
        .collect();
    assert_eq!(execs.len(), 1);
    assert_eq!(tools.len(), 1);
}

#[test]
fn bump_and_warn_dedupes_duplicate_types() {
    let _guard = COUNTER_TESTS.lock().unwrap();
    // spec §4 (at most one line per type per request): duplicate
    // unmappable tool types in one call (top-level repeats) must bump
    // the counter exactly once, not once per occurrence. Unique probe
    // name keeps the test parallel-safe (other tests never touch this
    // type).
    let probe = "normalize_test_dup_g";
    let before = unknown_type_counts().get(probe).copied().unwrap_or(0);
    let tools = vec![json!({"type": probe}), json!({"type": probe})];
    assert!(normalize_chat_tools(&tools).0.is_empty());
    let after = unknown_type_counts().get(probe).copied().unwrap_or(0);
    assert_eq!(after, before + 1);
}

#[test]
fn collect_additional_tools_reports_unmappable_namespace_inner_tools() {
    // An unmappable tool type inside an `additional_tools` namespace must
    // be reported, not silently ignored (spec §1 visibility; shared
    // classifier with `normalize_chat_tools`, spec §2).
    let items = vec![json!({
        "type": "additional_tools",
        "tools": [
            {"type": "namespace", "name": "functions", "tools": [
                {"type": "function", "name": "exec_command"},
                {"type": "web_search"}
            ]},
            {"type": "local_shell"}
        ]
    })];
    let mut tools = Vec::new();
    let (unmappable, schema_conflicts) =
        collect_additional_tools_reporting(&items, &mut tools, None);
    assert_eq!(tools.len(), 1, "only the function tool is hoisted");
    assert_eq!(unmappable, ["web_search", "local_shell"]);
    assert!(schema_conflicts.is_empty());
}

#[test]
fn responses_request_warns_unmappable_tool_dropped_from_additional_tools() {
    let probe = "normalize_test_tool_unmappable_i";
    let before = unknown_type_counts().get(probe).copied().unwrap_or(0);
    let mut obj: serde_json::Map<String, Value> = serde_json::from_value(json!({
        "model": "m",
        "input": [{"type": "additional_tools", "tools": [
            {"type": "namespace", "name": "functions", "tools": [
                {"type": "function", "name": "exec_command"},
                {"type": probe}
            ]}
        ]}]
    }))
    .unwrap();
    normalize_responses_request(&mut obj);
    // The function tool is hoisted; the unmappable one is dropped and
    // counted (was silently lost before).
    let tools = obj["tools"].as_array().unwrap();
    assert_eq!(tools[0]["name"], "exec_command");
    let after = unknown_type_counts().get(probe).copied().unwrap_or(0);
    assert_eq!(after, before + 1);
}

#[test]
fn chat_tools_warns_same_named_tool_with_different_schema_dropped() {
    // Dedup keeps the first same-named definition; when a later one
    // carries a different schema the drop must be visible, not silent.
    let probe_name = "normalize_test_conflict_tool";
    let before = unknown_type_counts().get(probe_name).copied().unwrap_or(0);
    let tools = vec![
        json!({"type": "function", "name": probe_name, "parameters": {"type": "object"}}),
        json!({"type": "function", "name": probe_name,
                   "parameters": {"type": "object", "properties": {"x": {"type": "string"}}}}),
    ];
    let (flat, _) = normalize_chat_tools(&tools);
    assert_eq!(flat.len(), 1);
    let after = unknown_type_counts().get(probe_name).copied().unwrap_or(0);
    assert_eq!(after, before + 1, "schema-conflict drop must be counted");
}

#[test]
fn responses_request_warns_same_named_tool_schema_conflict() {
    let probe_name = "normalize_test_conflict_resp";
    let before = unknown_type_counts().get(probe_name).copied().unwrap_or(0);
    let mut obj: serde_json::Map<String, Value> = serde_json::from_value(json!({
        "model": "m",
        "tools": [{"type": "function", "name": probe_name, "parameters": {"type": "object"}}],
        "input": [{"type": "additional_tools", "tools": [
            {"type": "function", "name": probe_name,
             "parameters": {"type": "object", "properties": {"x": {"type": "string"}}}}
        ]}]
    }))
    .unwrap();
    normalize_responses_request(&mut obj);
    let after = unknown_type_counts().get(probe_name).copied().unwrap_or(0);
    assert_eq!(after, before + 1);
}

/// Review C4: the counter map is capped at MAX_UNKNOWN_TYPE_KEYS distinct
/// keys — client-controlled type strings must not grow the heap without
/// bound. Filling past the cap keeps new keys warned-but-untracked, and the
/// map never exceeds the cap. Serialized against the snapshot tests via
/// COUNTER_TESTS (this test changes the map's capacity semantics).
#[test]
fn unknown_type_counter_caps_distinct_keys() {
    let _guard = COUNTER_TESTS.lock().unwrap();
    let base = unknown_type_counts().len();
    // Distinct unknown keys: the first (capacity - base) land in the map,
    // the rest must be refused without exceeding the cap.
    for i in 0..(MAX_UNKNOWN_TYPE_KEYS + 16) {
        let probe = format!("cap_probe_{i}");
        let tools = vec![json!({"type": probe})];
        normalize_chat_tools(&tools);
    }
    let counts = unknown_type_counts();
    assert!(
        counts.len() <= MAX_UNKNOWN_TYPE_KEYS,
        "counter map must never exceed the cap, got {}",
        counts.len()
    );
    assert!(
        counts.len() >= base,
        "pre-existing entries must not be evicted"
    );
    // Restore capacity for the other counter tests: drop the cap-probe keys.
    UNKNOWN_TYPE_COUNTS
        .lock()
        .unwrap()
        .retain(|k, _| !k.starts_with("cap_probe_"));
}
