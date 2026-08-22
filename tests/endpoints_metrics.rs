//! Integration tests: endpoints_metrics (module-split refactor).

mod common;

pub use common::*;

#[tokio::test]
/// The router's liveness endpoint must answer `200 ok` once it is up.
///
/// This is the baseline sanity check for the whole harness: if it
/// passes, the binary started, read the temp config, and bound its
/// listener — so failures in later tests are about behavior, not
/// setup.
async fn healthz_ok() {
    let env = setup().await;
    let resp = env
        .client
        .get(format!("{}/healthz", env.router_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text().await.unwrap(), "ok");
}

#[tokio::test]
/// Upstream returns 429 — the router must pass the status through and
/// record `error_class="http_429"` in /metrics (spec §7).
async fn metrics_reflects_upstream_429() {
    let env = setup().await;

    // Configure the mock to return 429 for the next request.
    env.mock_state.error_status.store(429, Ordering::SeqCst);

    // Send a request that hits the upstream; the mock will return 429.
    let resp = env
        .client
        .post(format!("{}/v1/responses", env.router_url))
        .json(&json!({"model": "mock/chat", "input": "hello", "stream": false}))
        .send()
        .await
        .unwrap();

    // The router must propagate the 429 status.
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);

    // Scrape /metrics and assert the error counter.
    let metrics_resp = env
        .client
        .get(format!("{}/metrics", env.router_url))
        .send()
        .await
        .unwrap();
    assert_eq!(metrics_resp.status(), StatusCode::OK);
    let body = metrics_resp.text().await.unwrap();

    // Assert error_class="http_429" (spec label value) was incremented.
    assert!(
        body.contains(r#"upstream_requests_total{provider="mock",route="mock/chat",model="upstream-chat-model",error_class="http_429"} 1"#),
        "expected http_429 counter in /metrics, got:
{body}"
    );
    // Error responses must also record TTFT and duration (spec §3-4).
    assert!(
        body.contains(r#"upstream_ttft_seconds_count{provider="mock",route="mock/chat",model="upstream-chat-model"} 1"#),
        "expected ttft histogram count for the 429 in /metrics, got:
{body}"
    );
    assert!(
        body.contains(r#"upstream_duration_seconds_count{provider="mock",route="mock/chat",model="upstream-chat-model"} 1"#),
        "expected duration histogram count for the 429 in /metrics, got:
{body}"
    );

    // Assert no success counter for this route.
    assert!(
        !body.contains(r#"upstream_requests_total{provider="mock",route="mock/chat",model="upstream-chat-model",error_class=""} 1"#),
        "unexpected success counter in /metrics after 429:
{body}"
    );

    // The mock still recorded the forwarded request.
    let received = env.mock_state.received_requests.lock().await;
    assert_eq!(received.len(), 1);
}

#[tokio::test]
async fn metrics_in_flight_gauge_tracks_active_and_completed_stream() {
    let (mock_url, _state) = spawn_mock_upstream().await;
    let mut env = None;
    for _ in 0..2 {
        let (router, router_url) = start_router(&format!("{mock_url}/slow"));
        if wait_for_healthz(&router_url, &router).await.is_ok() {
            env = Some((router, router_url));
            break;
        }
    }
    let (_router, router_url) = env.expect("router failed to start against the slow upstream");
    let client = reqwest::Client::new();

    let mut resp = client
        .post(format!("{router_url}/v1/responses"))
        .json(&json!({"model": "mock/chat", "input": "hello", "stream": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // While the upstream first chunk is still pending (~800ms), the gauge
    // must show one request in flight.
    let mut seen_in_flight = false;
    for _ in 0..30 {
        let body = client
            .get(format!("{router_url}/metrics"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        if body.contains(r#"upstream_requests_in_flight{provider="mock",route="mock/chat"} 1"#) {
            seen_in_flight = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        seen_in_flight,
        "in-flight gauge must be 1 while the stream is active"
    );

    // Drain the stream to completion, then the gauge must return to zero.
    let mut _stream_text = String::new();
    while let Some(chunk) = resp.chunk().await.expect("stream chunk") {
        _stream_text.push_str(&String::from_utf8_lossy(&chunk));
    }
    let mut back_to_zero = false;
    for _ in 0..40 {
        let body = client
            .get(format!("{router_url}/metrics"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        if !body.contains(r#"upstream_requests_in_flight{provider="mock",route="mock/chat"} 1"#) {
            back_to_zero = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        back_to_zero,
        "in-flight gauge must return to 0 after the stream ends"
    );
}

#[tokio::test]
async fn metrics_ignore_client_disconnect_as_upstream_truncation() {
    let (mock_url, _state) = spawn_mock_upstream().await;
    let mut env = None;
    for _ in 0..2 {
        let (router, router_url) = start_router(&format!("{mock_url}/slow"));
        if wait_for_healthz(&router_url, &router).await.is_ok() {
            env = Some((router, router_url));
            break;
        }
    }
    let (_router, router_url) = env.expect("router failed to start against the slow upstream");
    let client = reqwest::Client::new();

    let mut resp = client
        .post(format!("{router_url}/v1/responses"))
        .json(&json!({"model": "mock/chat", "input": "hello", "stream": true}))
        .send()
        .await
        .unwrap();

    // Read the synthesized response.created, then abort the connection
    // before the (delayed) upstream content arrives.
    let mut buf = String::new();
    loop {
        let chunk = resp
            .chunk()
            .await
            .expect("stream chunk")
            .expect("stream ended early");
        buf.push_str(&String::from_utf8_lossy(&chunk));
        if buf.contains("\n\n") {
            break;
        }
    }
    drop(resp);
    // Give the router's spawned task time to observe the failed send and
    // finish (the upstream first chunk arrives at ~800ms).
    tokio::time::sleep(Duration::from_millis(1100)).await;

    let body = client
        .get(format!("{router_url}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        !body.contains(r#"error_class="stream_truncated""#),
        "client disconnect must not be counted as upstream truncation:\n{body}"
    );
}

#[tokio::test]
/// Unroutable models must fail fast with a Responses-shaped 400 before
/// any upstream call (spec §9 error handling).
///
/// Verifies the error type is `invalid_request_error`, the unknown
/// route key (`nope/nope`) appears in the message, and — critically —
/// that the mock recorded zero requests: the router rejects unknown
/// models at routing time and never opens an upstream connection for
/// them.
async fn unknown_model_returns_400() {
    let env = setup().await;

    let resp = env
        .client
        .post(format!("{}/v1/responses", env.router_url))
        .json(&json!({"model": "nope/nope", "input": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "invalid_request_error");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("nope/nope"), "message: {message}");

    // Unknown route must never reach the upstream.
    assert!(env.mock_state.received_requests.lock().await.is_empty());
}

#[tokio::test]
/// `GET /v1/models` must list every configured route as a
/// Responses-style model object (see the endpoint table in
/// ARCHITECTURE.md), with `owned_by` set to the provider name. With a
/// single `mock/chat` route the expected list is exactly one entry,
/// keeping the assertion simple.
async fn models_endpoint_returns_sorted_routes() {
    let env = setup().await;

    let resp = env
        .client
        .get(format!("{}/v1/models", env.router_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["object"], "list");
    let data = body["data"].as_array().expect("data array");
    assert_eq!(data.len(), 1, "expected 1 route (mock/chat)");
    assert_eq!(data[0]["id"], "mock/chat");
    assert_eq!(data[0]["object"], "model");
    assert_eq!(data[0]["owned_by"], "mock");
}

#[tokio::test]
/// Duplicate of `healthz_ok` hitting the same `/healthz` path; kept
/// as a direct regression check that the health route answers
/// `200 ok`.
async fn healthz_returns_ok() {
    let env = setup().await;

    let resp = env
        .client
        .get(format!("{}/healthz", env.router_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text().await.unwrap(), "ok");
}

#[tokio::test]
async fn models_endpoint_reflects_hot_reload() {
    // The free_port() probe has a small TOCTOU window; mirror setup()'s
    // retry loop with a fresh port (and thus fresh temp config) per attempt.
    let mut started = None;
    for attempt in 0..2 {
        let port = free_port();
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let stderr_path = dir.path().join("router.stderr.log");

        let config_text = format!(
            r#"
[server]
host = "127.0.0.1"
port = {port}

[providers.ds]
base_url = "http://x"
api_key = "k"
format = "chat"

[routes]
"ds/old" = {{ model = "m", context_window = 1000 }}
"#
        );
        std::fs::write(&config_path, &config_text).expect("write config");

        let stderr = std::fs::File::create(&stderr_path).expect("create stderr log");
        let bin = env!("CARGO_BIN_EXE_codexferry");
        let child = std::process::Command::new(bin)
            .env("CODEXFERRY_CONFIG", &config_path)
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .expect("spawn codexferry");

        let guard = RouterGuard {
            child,
            stderr_path,
            _dir: dir,
        };
        let router_url = format!("http://127.0.0.1:{port}");
        match wait_for_healthz(&router_url, &guard).await {
            Ok(()) => {
                started = Some((guard, router_url, config_path, port));
                break;
            }
            Err(log) => {
                drop(guard); // kills the child
                if attempt == 1 {
                    panic!("router did not become healthy within 10s; stderr:\n{log}");
                }
            }
        }
    }
    let (_guard, router_url, config_path, port) =
        started.expect("retry loop always returns or panics");

    let client = reqwest::Client::new();

    // 1. First catalog snapshot: ds/old present, ds/new absent.
    let resp = client
        .get(format!("{router_url}/v1/models?client_version=0.0.0"))
        .send()
        .await
        .expect("first models get");
    assert_eq!(resp.status(), StatusCode::OK);
    let etag1 = resp
        .headers()
        .get("etag")
        .expect("etag header")
        .to_str()
        .unwrap()
        .to_owned();
    let body1 = resp.text().await.unwrap();
    let v1: serde_json::Value = serde_json::from_str(&body1).unwrap();
    let models1 = v1["models"].as_array().expect("models array");
    assert!(
        models1.iter().any(|m| m["slug"] == "ds/old"),
        "first catalog must contain ds/old:\n{body1}"
    );
    assert!(
        !models1.iter().any(|m| m["slug"] == "ds/new"),
        "first catalog must NOT contain ds/new:\n{body1}"
    );

    // 2. Rewrite the config file, adding a new route; the router hot-reloads.
    let new_config = format!(
        r#"
[server]
host = "127.0.0.1"
port = {port}

[providers.ds]
base_url = "http://x"
api_key = "k"
format = "chat"

[routes]
"ds/old" = {{ model = "m", context_window = 1000 }}
"ds/new" = {{ model = "m", context_window = 2000 }}
"#
    );
    std::fs::write(&config_path, &new_config).expect("rewrite config for hot reload");

    // 3. Poll until the watcher swaps the config and CatalogCache rebuilds:
    //    ds/new must appear while ds/old remains, and the ETag must change.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let resp = client
            .get(format!("{router_url}/v1/models?client_version=0.0.0"))
            .send()
            .await
            .expect("poll models get");
        let etag = resp
            .headers()
            .get("etag")
            .expect("etag header")
            .to_str()
            .unwrap()
            .to_owned();
        let body = resp.text().await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let models = v["models"].as_array().expect("models array");
        if models.iter().any(|m| m["slug"] == "ds/old")
            && models.iter().any(|m| m["slug"] == "ds/new")
        {
            assert_ne!(etag, etag1, "catalog ETag must change after hot reload");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "catalog did not reflect hot reload within 5s; last body:\n{body}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
#[ignore = "requires local codex CLI; run explicitly: cargo test --release -- --ignored"]
async fn doctor_live_probe_report_has_no_failures() {
    if std::process::Command::new("codex")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: codex CLI not found");
        return;
    }
    // doctor --live prints its own report and exits 1 on failure; run the
    // binary the way a user would and require a clean exit.
    let bin = env!("CARGO_BIN_EXE_codexferry");
    let out = std::process::Command::new(bin)
        .args(["doctor", "--live", "--config", "config.toml"])
        .env("CODEXFERRY_CONFIG", "config.toml")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    println!("{stdout}");
    assert!(out.status.success(), "doctor --live failed:\n{stdout}");
    assert!(!stdout.contains("FAIL"), "report contained FAIL:\n{stdout}");
}
