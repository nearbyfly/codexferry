//! Integration tests: sessions (module-split refactor).

mod common;

pub use common::*;

#[tokio::test]
/// Multi-turn session support (spec §8): turn 2 references turn 1's
/// `response_id` via `previous_response_id`, and the router must
/// replay the stored conversation into the forwarded Chat request.
///
/// Proves the session store was consulted and full-context history was
/// converted back to Chat messages: the forwarded messages must be
/// exactly 3 — the original user input, the assistant's accumulated
/// reply ("Hello world"), then the new user input. Also verifies the
/// AGENTS.md convention that `previous_response_id` is *consumed,
/// never forwarded*: the upstream request must not contain that
/// field.
async fn previous_response_id_merges_history() {
    let env = setup().await;

    // Turn 1: streaming request whose id seeds the session.
    let resp = env
        .client
        .post(format!("{}/v1/responses", env.router_url))
        .json(&json!({"model": "mock/chat", "input": "first message", "stream": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.unwrap();
    let events = parse_sse(&body);
    let created: Value = serde_json::from_str(&events[0].data).unwrap();
    let response_id = created["response"]["id"]
        .as_str()
        .expect("response id")
        .to_string();

    // Turn 2: reference turn 1 via previous_response_id.
    let resp = env
        .client
        .post(format!("{}/v1/responses", env.router_url))
        .json(&json!({
            "model": "mock/chat",
            "input": "second message",
            "stream": true,
            "previous_response_id": response_id,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body2 = resp.text().await.unwrap();
    assert!(!body2.is_empty());

    // The mock received two requests; the second must carry merged history:
    // original user message + assistant reply + new user input.
    let received = env.mock_state.received_requests.lock().await;
    assert_eq!(received.len(), 2);
    let second = &received[1];
    let messages = second["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 3, "expected merged history + new input");
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "first message");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"], "Hello world");
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[2]["content"], "second message");
    // previous_response_id must not be forwarded to the upstream.
    assert!(second.get("previous_response_id").is_none());
}

#[tokio::test]
/// Issue #9 / #14: `store: false` must skip session persistence — turn 2's
/// `previous_response_id` degrades to a store miss and the upstream receives
/// only the new input (Codex replays its full transcript inline, so the
/// skipped snapshot would never be read back).
async fn store_false_skips_session_persistence() {
    let env = setup().await;

    // Turn 1 with store:false — the response still streams normally.
    let resp = env
        .client
        .post(format!("{}/v1/responses", env.router_url))
        .json(&json!({
            "model": "mock/chat",
            "input": "first message",
            "stream": true,
            "store": false,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.unwrap();
    let events = parse_sse(&body);
    let created: Value = serde_json::from_str(&events[0].data).unwrap();
    let response_id = created["response"]["id"].as_str().expect("response id");

    // Turn 2 referencing turn 1: store miss → only the new input forwards.
    let resp = env
        .client
        .post(format!("{}/v1/responses", env.router_url))
        .json(&json!({
            "model": "mock/chat",
            "input": "second message",
            "stream": true,
            "previous_response_id": response_id,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let received = env.mock_state.received_requests.lock().await;
    assert_eq!(received.len(), 2);
    let messages = received[1]["messages"].as_array().expect("messages array");
    assert_eq!(
        messages.len(),
        1,
        "store:false turn must not be persisted; upstream must see only the new input"
    );
    assert_eq!(messages[0]["content"], "second message");
}

#[tokio::test]
/// A truncated stream (no `[DONE]`, no finish_reason) must NOT persist a
/// session (spec §4): the `!saw_completed` → `on_error` path surfaces
/// `response.failed` and skips `save_session`. This test proves the
/// consequence on the next turn — a follow-up request referencing the
/// truncated response id via `previous_response_id` hits a SessionStore miss
/// and replays ONLY its own new input, never the partial assistant text.
async fn truncated_stream_session_is_not_persisted() {
    let (mock_url, state) = spawn_mock_upstream().await;
    // Serve the truncated (content-only, no finish_reason, no [DONE]) stream.
    state.truncated.store(true, Ordering::SeqCst);
    let (_router, router_url) = spawn_router_two_providers(&mock_url, "").await;
    let client = reqwest::Client::new();

    // Turn 1: streaming request against mocknd — truncated, so the client
    // must see response.failed (and never response.completed), and the id
    // from response.created must NOT become a session key.
    let resp = client
        .post(format!("{router_url}/v1/responses"))
        .json(&json!({"model": "mocknd/chat", "input": "hello", "stream": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.unwrap();
    assert!(body.contains("response.failed"));
    assert!(!body.contains("response.completed"));
    let events = parse_sse(&body);
    let created: Value = serde_json::from_str(&events[0].data).unwrap();
    let response_id = created["response"]["id"]
        .as_str()
        .expect("response id")
        .to_string();

    // Turn 2: reference turn 1's id. On a SessionStore hit this would merge
    // the assistant reply into the forwarded messages; on the miss expected
    // here it degrades to new-input-only (SessionStore::get logs at debug).
    let resp = client
        .post(format!("{router_url}/v1/responses"))
        .json(&json!({
            "model": "mocknd/chat",
            "input": "second message",
            "stream": true,
            "previous_response_id": response_id,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = resp.text().await.unwrap();

    // The mock recorded both requests; the SECOND must carry no history —
    // only the new user input — proving the truncated session was not
    // persisted.
    let received = state.received_requests.lock().await;
    assert_eq!(received.len(), 2);
    let first = &received[0];
    let messages1 = first["messages"].as_array().expect("messages array");
    assert_eq!(messages1.len(), 1);
    assert_eq!(messages1[0]["content"], "hello");
    let second = &received[1];
    let messages2 = second["messages"].as_array().expect("messages array");
    assert_eq!(
        messages2.len(),
        1,
        "truncated session must not be replayed as history"
    );
    assert_eq!(messages2[0]["role"], "user");
    assert_eq!(messages2[0]["content"], "second message");
    // previous_response_id is consumed, never forwarded (AGENTS.md #5).
    assert!(second.get("previous_response_id").is_none());
}
