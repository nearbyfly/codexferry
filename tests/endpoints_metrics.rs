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
        // Pin CODEX_HOME to a subdir of the test's TempDir so the applier's
        // invalidate_codex_catalog_cache() can't reach the developer's real
        // ~/.codex/models_cache.json during a reload.
        let codex_home = dir.path().join("codex-home");
        std::fs::create_dir_all(&codex_home).expect("create codex-home");
        let child = std::process::Command::new(bin)
            .env("CODEXFERRY_CONFIG", &config_path)
            .env("CODEX_HOME", &codex_home)
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
/// Reload-staleness spec S3 (the 2026-09-01 incident shape): deleting a
/// route and hot-reloading must remove its slug from the catalog for EVERY
/// request after the reload — including the rapid bursts a restarted codex
/// fires — because such a response gets persisted into codex's own 300s
/// cache and would keep the removed model selectable for ~5 minutes.
async fn models_endpoint_reflects_route_removal() {
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
"ds/keep" = {{ model = "m", context_window = 1000 }}
"ds/doomed" = {{ model = "m", context_window = 1000 }}
"#
        );
        std::fs::write(&config_path, &config_text).expect("write config");

        let stderr = std::fs::File::create(&stderr_path).expect("create stderr log");
        let bin = env!("CARGO_BIN_EXE_codexferry");
        let codex_home = dir.path().join("codex-home");
        std::fs::create_dir_all(&codex_home).expect("create codex-home");
        let child = std::process::Command::new(bin)
            .env("CODEXFERRY_CONFIG", &config_path)
            .env("CODEX_HOME", &codex_home)
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
                drop(guard);
                if attempt == 1 {
                    panic!("router did not become healthy within 10s; stderr:\n{log}");
                }
            }
        }
    }
    let (_guard, router_url, config_path, port) =
        started.expect("retry loop always returns or panics");
    let client = reqwest::Client::new();

    // Baseline: both slugs present.
    let body = client
        .get(format!("{router_url}/v1/models?client_version=0.0.0"))
        .send()
        .await
        .expect("baseline models get")
        .text()
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let models = v["models"].as_array().expect("models array");
    assert!(models.iter().any(|m| m["slug"] == "ds/doomed"), "{body}");
    assert!(models.iter().any(|m| m["slug"] == "ds/keep"), "{body}");

    // Delete ds/doomed: rewrite the config without it (editor-style write).
    let removed = format!(
        r#"
[server]
host = "127.0.0.1"
port = {port}

[providers.ds]
base_url = "http://x"
api_key = "k"
format = "chat"

[routes]
"ds/keep" = {{ model = "m", context_window = 1000 }}
"#
    );
    editor_save(&config_path, &removed);

    // Poll until the slug is gone, then hammer: it must NEVER reappear (the
    // incident had it re-persisted by a racing fetch for ~5 minutes).
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let body = client
            .get(format!("{router_url}/v1/models?client_version=0.0.0"))
            .send()
            .await
            .expect("poll models get")
            .text()
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let models = v["models"].as_array().expect("models array");
        if !models.iter().any(|m| m["slug"] == "ds/doomed") {
            assert!(models.iter().any(|m| m["slug"] == "ds/keep"), "{body}");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "removed slug still served after 10s; last body:\n{body}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for _ in 0..20 {
        let body = client
            .get(format!("{router_url}/v1/models?client_version=0.0.0"))
            .send()
            .await
            .expect("burst models get")
            .text()
            .await
            .unwrap();
        assert!(
            !body.contains("ds/doomed"),
            "removed slug reappeared in a burst response: {body}"
        );
    }
}

#[tokio::test]
/// hide_bundled_models (dynamic mode): with the flag on, the catalog-shape
/// /models response must carry `visibility: "hide"` overrides cloned from
/// the (faked) bundled catalog so codex's slug merge hides them; with the
/// flag off they must be absent. The chat-list shape must never contain
/// them. Toggling the flag via config hot-reload must rebuild the catalog.
async fn models_catalog_hides_bundled_models_when_enabled() {
    // Fake `codex` binary: `codex debug models --bundled` prints one
    // list-visible and one already-hidden model. Prepending its directory
    // to the child's PATH shadows any real codex install.
    let bin_dir = tempfile::tempdir().expect("bin tempdir");
    let fake_codex = bin_dir.path().join("codex");
    std::fs::write(
        &fake_codex,
        "#!/bin/sh\necho '{\"models\":[{\"slug\":\"gpt-fake-sol\",\"visibility\":\"list\",\"priority\":1},{\"slug\":\"gpt-fake-old\",\"visibility\":\"hide\",\"priority\":2}]}'",
    )
    .expect("write fake codex");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_codex, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake codex");
    }
    let child_path = format!(
        "{}:{}",
        bin_dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let base_config = |port: u16, hide: bool| {
        format!(
            r#"
[server]
host = "127.0.0.1"
port = {port}
hide_bundled_models = {hide}

[providers.ds]
base_url = "http://x"
api_key = "k"
format = "chat"

[routes]
"ds/a" = {{ model = "m", context_window = 1000 }}
"#
        )
    };

    // TOCTOU retry, mirroring models_endpoint_reflects_hot_reload.
    let mut started = None;
    for attempt in 0..2 {
        let port = free_port();
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, base_config(port, false)).expect("write config");
        let stderr_path = dir.path().join("router.stderr.log");
        let stderr = std::fs::File::create(&stderr_path).expect("create stderr log");
        let bin = env!("CARGO_BIN_EXE_codexferry");
        let child = std::process::Command::new(bin)
            .env("CODEXFERRY_CONFIG", &config_path)
            .env("PATH", &child_path)
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
                drop(guard);
                if attempt == 1 {
                    panic!("router did not become healthy within 10s; stderr:\n{log}");
                }
            }
        }
    }
    let (_guard, router_url, config_path, port) =
        started.expect("retry loop always returns or panics");

    let client = reqwest::Client::new();

    // 1. Flag off: no hide entries in the catalog shape.
    let resp = client
        .get(format!("{router_url}/v1/models?client_version=0.0.0"))
        .send()
        .await
        .expect("models get (flag off)");
    let body = resp.text().await.unwrap();
    assert!(
        !body.contains("gpt-fake-sol"),
        "flag off must not append hide entries:\n{body}"
    );

    // 2. Chat-list shape (no client_version): routes only, BEFORE the flag
    //    is turned on (spec §Decisions 2).
    let resp = client
        .get(format!("{router_url}/v1/models"))
        .send()
        .await
        .expect("chat-list models get");
    let list: Value = resp.json().await.unwrap();
    let ids: Vec<&str> = list["data"]
        .as_array()
        .expect("data array")
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["ds/a"]);

    // 3. Toggle the flag via hot-reload and poll until the hide entry
    //    appears with visibility "hide".
    std::fs::write(&config_path, base_config(port, true)).expect("rewrite config with hide on");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let resp = client
            .get(format!("{router_url}/v1/models?client_version=0.0.0"))
            .send()
            .await
            .expect("poll models get");
        let body = resp.text().await.unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
        let models = v["models"].as_array().expect("models array");
        let sol = models.iter().find(|m| m["slug"] == "gpt-fake-sol");
        let done = match sol {
            Some(m) => {
                assert_eq!(m["visibility"], "hide", "override must hide:\n{body}");
                true
            }
            None => false,
        };
        if done {
            assert!(
                !models.iter().any(|m| m["slug"] == "gpt-fake-old"),
                "already-hidden bundled entries need no override:\n{body}"
            );
            assert!(
                models
                    .iter()
                    .any(|m| m["slug"] == "ds/a" && m["visibility"] == "list"),
                "route stays list-visible:\n{body}"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "hide entry did not appear within 5s of hot-reload; last body:\n{body}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // 4. Chat-list shape stays routes-only even after the flag is on
    //    (spec §Decisions 2): the no-client_version branch never reads
    //    hide entries.
    let resp = client
        .get(format!("{router_url}/v1/models"))
        .send()
        .await
        .expect("chat-list models get (after toggle)");
    let list: Value = resp.json().await.unwrap();
    let ids: Vec<&str> = list["data"]
        .as_array()
        .expect("data array")
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["ds/a"]);
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
    //
    // `XDG_STATE_HOME` MUST point at a tempdir: `apply_record` (and any
    // future green-run write) persists `~/.local/state/codexferry/doctor.json`.
    // Without the override, every CI run writes the developer's real state
    // file, polluting later runs and breaking isolation. `dir` must outlive
    // the child: `XDG_STATE_HOME` points into it.
    let dir = tempfile::tempdir().expect("tempdir");
    let state_home = dir.path().join("state");
    std::fs::create_dir_all(&state_home).expect("mkdir state");
    let bin = env!("CARGO_BIN_EXE_codexferry");
    let out = std::process::Command::new(bin)
        .args(["doctor", "--live", "--config", "config.toml"])
        .env("CODEXFERRY_CONFIG", "config.toml")
        .env("XDG_STATE_HOME", &state_home)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    println!("{stdout}");
    assert!(out.status.success(), "doctor --live failed:\n{stdout}");
    assert!(!stdout.contains("FAIL"), "report contained FAIL:\n{stdout}");
}

// ---------------------------------------------------------------------------
// Hot-reload atomic-save regression tests (Task 2)
// ---------------------------------------------------------------------------

/// Editor-style atomic save: write a sibling temp file, then rename(2)
/// over the config. The file's inode is REPLACED each time — exactly the
/// save style that permanently killed the old inode-level watch.
fn editor_save(path: &std::path::Path, contents: &str) {
    let tmp = path
        .parent()
        .expect("config has a parent dir")
        .join("config.toml.editor-tmp");
    std::fs::write(&tmp, contents).expect("write editor temp config");
    std::fs::rename(&tmp, path).expect("atomic rename over config");
}

/// Poll the catalog until `slug` appears; 5s deadline with a failure
/// message carrying the last body.
async fn wait_for_slug(client: &reqwest::Client, base_url: &str, slug: &str) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let resp = client
            .get(format!("{base_url}/v1/models?client_version=t"))
            .send()
            .await
            .expect("models get");
        let body = resp.text().await.unwrap();
        if body.contains(&format!("\"{slug}\"")) {
            return body;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "slug {slug} did not appear within 5s; last body:\n{body}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
/// Two successive editor-style (atomic-rename) config saves must EACH
/// trigger a hot reload. The second save is the assertion's soul: it
/// proves the first inode replacement did not kill the watch
/// (hot-reload-watcher spec §Testing).
async fn models_hot_reload_survives_editor_atomic_saves() {
    let config_with_routes = |port: u16, routes: &str| {
        format!(
            r#"
[server]
host = "127.0.0.1"
port = {port}

[providers.ds]
base_url = "http://x"
api_key = "k"
format = "chat"

[routes]
{routes}
"#
        )
    };

    // TOCTOU retry, mirroring models_endpoint_reflects_hot_reload.
    let mut started = None;
    for attempt in 0..2 {
        let port = free_port();
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            config_with_routes(
                port,
                "\"ds/old\" = { model = \"m\", context_window = 1000 }",
            ),
        )
        .expect("write config");
        let stderr_path = dir.path().join("router.stderr.log");
        let stderr = std::fs::File::create(&stderr_path).expect("create stderr log");
        let bin = env!("CARGO_BIN_EXE_codexferry");
        // Pin CODEX_HOME to a subdir of the test's TempDir so the applier's
        // invalidate_codex_catalog_cache() can't reach the developer's real
        // ~/.codex/models_cache.json during a reload.
        let codex_home = dir.path().join("codex-home");
        std::fs::create_dir_all(&codex_home).expect("create codex-home");
        let child = std::process::Command::new(bin)
            .env("CODEXFERRY_CONFIG", &config_path)
            .env("CODEX_HOME", &codex_home)
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
                drop(guard);
                if attempt == 1 {
                    panic!("router did not become healthy within 10s; stderr:\n{log}");
                }
            }
        }
    }
    let (_guard, router_url, config_path, port) =
        started.expect("retry loop always returns or panics");
    let client = reqwest::Client::new();

    let _ = wait_for_slug(&client, &router_url, "ds/old").await;

    // Save #1: ds/old -> ds/new (inode replaced).
    editor_save(
        &config_path,
        &config_with_routes(
            port,
            "\"ds/new\" = { model = \"m\", context_window = 2000 }",
        ),
    );
    let _ = wait_for_slug(&client, &router_url, "ds/new").await;

    // Save #2: ds/new -> ds/final. On the OLD inode-watch this never
    // appears — the watch died at save #1 (or at the last-gasp event),
    // and this poll is the guaranteed red point.
    editor_save(
        &config_path,
        &config_with_routes(
            port,
            "\"ds/final\" = { model = \"m\", context_window = 3000 }",
        ),
    );
    let body = wait_for_slug(&client, &router_url, "ds/final").await;
    assert!(
        !body.contains("\"ds/old\""),
        "stale route must be gone after reload:\n{body}"
    );
}

#[tokio::test]
/// Production layout from the 2026-08-28 incident: CODEXFERRY_CONFIG
/// points at a SYMLINK while the real config lives in another directory.
/// canonicalize() must bind the watch to the real file's directory so an
/// atomic-rename edit of the real file triggers the reload
/// (hot-reload-watcher spec §Testing).
async fn models_hot_reload_via_symlinked_config_path() {
    let port = free_port();
    let real_dir = tempfile::tempdir().expect("real config dir");
    let link_dir = tempfile::tempdir().expect("symlink dir");
    let real_config = real_dir.path().join("cxf.toml");
    let config_link = link_dir.path().join("cxf.toml");
    std::os::unix::fs::symlink(&real_config, &config_link).expect("symlink config");
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
    std::fs::write(&real_config, &config_text).expect("write real config");

    let stderr_path = real_dir.path().join("router.stderr.log");
    let stderr = std::fs::File::create(&stderr_path).expect("create stderr log");
    let bin = env!("CARGO_BIN_EXE_codexferry");
    // Pin CODEX_HOME to a subdir of the test's TempDir so the applier's
    // invalidate_codex_catalog_cache() can't reach the developer's real
    // ~/.codex/models_cache.json during a reload.
    let codex_home = real_dir.path().join("codex-home");
    std::fs::create_dir_all(&codex_home).expect("create codex-home");
    let child = std::process::Command::new(bin)
        .env("CODEXFERRY_CONFIG", &config_link)
        .env("CODEX_HOME", &codex_home)
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .expect("spawn codexferry");
    let guard = RouterGuard {
        child,
        stderr_path,
        _dir: link_dir,
    };
    // NOTE: `_dir` holds the symlink dir; `real_dir` must ALSO outlive the
    // test — bind it here so both guards drop at test end.
    let _real_guard = real_dir;
    let router_url = format!("http://127.0.0.1:{port}");
    wait_for_healthz(&router_url, &guard)
        .await
        .expect("router healthy");
    let client = reqwest::Client::new();

    let _ = wait_for_slug(&client, &router_url, "ds/old").await;

    // Editor-style save on the REAL file (not through the symlink path):
    // must still fire the reload.
    let new_text = config_text.replace("ds/old", "ds/new");
    assert_ne!(
        new_text, config_text,
        "fixture replace must change the config"
    );
    editor_save(&real_config, &new_text);
    let _ = wait_for_slug(&client, &router_url, "ds/new").await;
}
