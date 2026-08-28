# Hot-Reload Watcher Fix Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make config hot-reload immune to atomic-rename editor saves by watching the canonicalized config path's parent directory with a filename filter, replacing the inode-level watch that dies permanently on the first file replacement. Task 4 pins the
user-visible outcome end-to-end: a codex session created before a config
change can `resume` afterwards on a route that only exists after the
change (the 0.148.0-reported case; not a codex regression - spec §Problem).

**Architecture:** `spawn_watcher` resolves the config path through symlinks once at startup, watches the resolved file's parent directory (stable inode across replacements), and filters events by `EventKind::Modify(_) | Create(_)` plus a filename predicate. The parse/ENOENT-retry/applier plumbing is untouched.

**Tech Stack:** Rust, `notify` v7 (inotify backend); no new dependencies.

**Spec:** `docs/superpowers/specs/2026-08-28-hot-reload-watcher-fix-design.md` — read it first; this plan argues from it.

## Global Constraints

- No new crate dependencies; `notify` API usage only (`recommended_watcher`, `watch`, `EventKind`, `RecursiveMode`).
- Degradation posture: `canonicalize()` failure → `warn!` + watch the UN-resolved path's parent directory; never panic at startup (spec §Design).
- The parse + ENOENT-retry + channel-send callback body stays byte-identical in behavior (spec §Design "Event filter").
- Existing in-place-write hot-reload test (`models_endpoint_reflects_hot_reload`) must keep passing — the benign save style remains supported.
- Tracing format strings use `{field}` placeholders, never positional `{}` (AGENTS.md #3).
- Comments in English; AGENTS.md #7 and ARCHITECTURE.md §11 updated in the same change (AGENTS.md #13).
- Each task ends with its tests green + a commit; integration tests use `CARGO_BIN_EXE_codexferry` + the TOCTOU port retry + `/healthz` poll (AGENTS.md Testing Strategy).

---

### Task 1: Filename-relevance predicate + unit tests

**Files:**
- Modify: `src/config.rs` (new private fn next to `spawn_watcher`, ~line 800)
- Test: `src/config.rs` (new `#[cfg(test)] mod watcher_tests`)

**Interfaces:**
- Consumes: nothing new.
- Produces: `fn event_touches_config(paths: &[std::path::PathBuf], file_name: &std::ffi::OsStr) -> bool` (private; used by Task 2's watcher callback).

- [ ] **Step 1: Write the failing tests**

Append to `src/config.rs`:

```rust
#[cfg(test)]
mod watcher_tests {
    use super::*;

    #[test]
    fn event_touches_config_matches_only_the_exact_filename() {
        let name = std::ffi::OsStr::new("cxf.toml");
        // Direct hit (in-place write on the watched directory).
        assert!(event_touches_config(
            &[std::path::PathBuf::from("/etc/cxf.toml")],
            name
        ));
        // Rename events carry from+to paths; the to-path matches.
        assert!(event_touches_config(
            &[
                std::path::PathBuf::from("/etc/cxf.toml.editor-tmp"),
                std::path::PathBuf::from("/etc/cxf.toml"),
            ],
            name
        ));
        // Editor temporaries and unrelated files never match.
        assert!(!event_touches_config(
            &[std::path::PathBuf::from("/etc/.cxf.toml.swp")],
            name
        ));
        assert!(!event_touches_config(
            &[std::path::PathBuf::from("/etc/4913")],
            name
        ));
        // No paths -> not relevant.
        assert!(!event_touches_config(&[], name));
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test watcher_tests`
Expected: FAIL — `cannot find function event_touches_config` (compile error).

- [ ] **Step 3: Implement the predicate**

Insert above `spawn_watcher` in `src/config.rs`:

```rust
/// Whether any path in a notify event refers to the config file itself
/// (final-component match). The watcher observes the whole parent
/// DIRECTORY, so this filters out editor temporaries (`.swp`, `4913`,
/// rename sources) and unrelated directory activity (hot-reload-watcher
/// spec §Design "Event filter").
fn event_touches_config(
    paths: &[std::path::PathBuf],
    file_name: &std::ffi::OsStr,
) -> bool {
    paths.iter().any(|p| p.file_name() == Some(file_name))
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test watcher_tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat(config): event filename-relevance predicate for directory watches"
```

---

### Task 2: Directory watch in `spawn_watcher` + double-atomic-save regression test

**Files:**
- Modify: `src/config.rs` (`spawn_watcher`, ~lines 802-857)
- Test: `tests/endpoints_metrics.rs` (new test + two helpers)

**Interfaces:**
- Consumes: `event_touches_config` (Task 1).
- Produces: `spawn_watcher` with unchanged public signature; test helpers `fn editor_save(path: &std::path::Path, contents: &str)` and `async fn wait_for_slug(client: &reqwest::Client, base_url: &str, slug: &str) -> String` (used by Task 3).

- [ ] **Step 1: Write the failing integration test**

Append to `tests/endpoints_metrics.rs` (helpers first — Task 3 reuses them):

```rust
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
            &config_with_routes(
                port,
                "\"ds/old\" = { model = \"m\", context_window = 1000 }",
            ),
        )
        .expect("write config");
        let stderr_path = dir.path().join("router.stderr.log");
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
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test endpoints_metrics models_hot_reload_survives_editor_atomic_saves`
Expected: FAIL — `slug ds/new (or ds/final) did not appear within 5s`. On the current inode-watch, unlinking the watched inode surfaces as a Remove-class event the callback's `Modify(_) | Create(_)` gate ignores, so the FIRST atomic save already never reloads; whichever poll hits its deadline first is the red.

- [ ] **Step 3: Rework `spawn_watcher`**

Replace the head of `spawn_watcher` in `src/config.rs` (from `let path = path.to_path_buf();` down to `watcher.watch(&path, RecursiveMode::NonRecursive)?;`), keeping the channel/applier lines and the callback's parse-retry body byte-identical:

```rust
pub fn spawn_watcher(path: &Path, shared: SharedConfig) -> anyhow::Result<impl Watcher> {
    // Resolve symlinks ONCE and watch the REAL file's parent directory
    // with a filename filter (hot-reload-watcher spec §Design). An
    // inode-level file watch dies permanently on the first atomic-rename
    // editor save: the rename unlinks the watched inode, the kernel
    // delivers IN_IGNORED, and notify drops the watch with nothing to
    // re-arm it. The directory's inode is stable across file
    // replacements. Limitation: the symlink is resolved at startup only
    // — re-pointing it later requires a daemon restart.
    let resolved = path.canonicalize().unwrap_or_else(|e| {
        tracing::warn!(
            "config path {} could not be canonicalized ({e}); \
             watching the unresolved path's directory",
            path.display()
        );
        path.to_path_buf()
    });
    let file_name = resolved
        .file_name()
        .map(std::ffi::OsStr::to_owned)
        .ok_or_else(|| anyhow::anyhow!("config path has no file name: {}", resolved.display()))?;
    let watch_dir = resolved
        .parent()
        .map(|p| p.to_path_buf())
        .ok_or_else(|| anyhow::anyhow!("config path has no parent directory: {}", resolved.display()))?;
    let watch_path = resolved;
    // Decouple the synchronous notify callback from the async RwLock: the
    // callback only sends (never blocks, never loses an update), and the
    // applier task awaits the write lock on the tokio runtime.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<ValidatedConfig>();
    let _applier = tokio::spawn(spawn_config_applier(shared, rx));

    let mut watcher = notify::recommended_watcher(move |res: Result<notify::Event, _>| {
        if let Ok(event) = res {
            if matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_))
                && event_touches_config(&event.paths, &file_name)
            {
                // Editors that save atomically (vim `set backupcopy=no`,
                // emacs, many IDEs) write to a temp file then `rename(temp,
                // target)` - the rename briefly unlinks the target before
                // the new inode appears. The notify event lands during that
                // gap, the parse_file fails with ENOENT, and the user sees
                // `config reload failed` followed by a successful reload
                // on the next event. Retry a couple of times with a tiny
                // sleep so the spurious error disappears; non-ENOENT
                // errors and ENOENT past the retry budget still surface
                // (a real file deletion is a meaningful state change).
                for attempt in 0..3 {
                    match Config::parse_file(&watch_path).and_then(|c| c.validate()) {
                        Ok(new_cfg) => {
                            // Unbounded send: cannot fail while the
                            // applier task holds the receiver. If it
                            // has already shut down (daemon exit), the
                            // update is moot - ignore the error.
                            let _ = tx.send(new_cfg);
                            break;
                        }
                        Err(ConfigError::Io(io_err))
                            if io_err.kind() == std::io::ErrorKind::NotFound && attempt < 2 =>
                        {
                            std::thread::sleep(std::time::Duration::from_millis(20));
                            continue;
                        }
                        Err(e) => {
                            tracing::error!("config reload failed, keeping old config: {e}");
                            break;
                        }
                    }
                }
            }
        }
    })?;
    watcher.watch(&watch_dir, RecursiveMode::NonRecursive)?;
    Ok(watcher)
}
```

(Only the head of the function changes — canonicalize/filename/dir derivation — plus the `if` gaining the `event_touches_config` conjunct; the retry body above is the current code copied verbatim. Also delete the old misleading comment block claiming the parent directory "is what actually gets watched" — the new head comment replaces it.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test endpoints_metrics` — Expected: PASS, including the pre-existing `models_endpoint_reflects_hot_reload` (in-place write still works) and the new atomic-save test.
Run: `cargo test config::` — Expected: PASS (watcher_tests + existing config suites).

- [ ] **Step 5: Commit**

```bash
git add src/config.rs tests/endpoints_metrics.rs
git commit -m "fix(config): watch the canonicalized parent directory - hot-reload survives atomic-rename saves"
```

---

### Task 3: Symlink-layout integration test + docs sync + full regression

**Files:**
- Test: `tests/endpoints_metrics.rs` (new test; consumes Task 2's `editor_save` / `wait_for_slug`)
- Modify: `AGENTS.md` (§7, one appended sentence)
- Modify: `ARCHITECTURE.md` (§11 line counts)

**Interfaces:**
- Consumes: `editor_save`, `wait_for_slug`, the fixed watcher behavior.
- Produces: documentation only.

- [ ] **Step 1: Write the symlink-layout test**

Append to `tests/endpoints_metrics.rs`:

```rust
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
    std::os::unix::fs::symlink(&real_config, &config_link)
        .expect("symlink config");
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
    let child = std::process::Command::new(bin)
        .env("CODEXFERRY_CONFIG", &config_link)
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
    assert_ne!(new_text, config_text, "fixture replace must change the config");
    editor_save(&real_config, &new_text);
    let _ = wait_for_slug(&client, &router_url, "ds/new").await;
}
```

- [ ] **Step 2: Run the new test**

Run: `cargo test --test endpoints_metrics models_hot_reload_via_symlinked_config_path`
Expected: PASS (post-Task-2 the canonicalized directory watch covers it). On pre-fix code this would fail — if it passes before Task 2 for any environmental reason, STOP and investigate rather than proceeding.

- [ ] **Step 3: AGENTS.md §7 — append to the end of the channel-design paragraph:**

```markdown
The watcher itself binds to the canonicalized config path's PARENT
directory with a filename filter — an inode-level file watch would die
permanently on the first atomic-rename editor save (IN_IGNORED, no
re-arm); the symlink is resolved once at startup, so re-pointing it
requires a daemon restart (see
`docs/superpowers/specs/2026-08-28-hot-reload-watcher-fix-design.md`).
```

- [ ] **Step 4: ARCHITECTURE.md §11 — refresh line counts**

Run: `wc -l src/config.rs tests/endpoints_metrics.rs` and update those rows.

- [ ] **Step 5: Full verification**

Run: `cargo fmt` then `cargo test`
Expected: whole suite PASS.

- [ ] **Step 6: Commit**

```bash
git add tests/endpoints_metrics.rs AGENTS.md ARCHITECTURE.md
git commit -m "test(config): symlink-layout hot-reload coverage + watcher docs sync"
```

---

### Task 4: E2E `resume_after_reload` - the 0.148.0-reported case end-to-end

**Files:**
- Modify: `scripts/e2e-lib.sh` (start_router forwards CODEX_HOME; two wait helpers)
- Modify: `scripts/e2e.sh` (new scenario + dispatch/usage registration)

**Interfaces:**
- Consumes: the fixed watcher (Task 2), `run_codex` / `run_codex_resume` /
  `assert_live_catalog_fetched` / `write_router_config` (e2e-lib.sh),
  the reload-side cache invalidation (`invalidate_codex_catalog_cache`,
  already shipped).
- Produces: `scenario_resume_after_reload`, helpers `wait_models_route` /
  `wait_cache_gone`.

**Why a lib change is needed:** `start_router` currently spawns the daemon
WITHOUT `CODEX_HOME`, so every e2e daemon's reload-side invalidation
targets the REAL `~/.codex/models_cache.json` (a latent harness bug: e2e
must never touch the developer's home). Forwarding the scratch
`$ARTIFACT_DIR/codex-home` fixes that for all scenarios and lets this
one assert the invalidation against a file it owns.

- [ ] **Step 1: `start_router` forwards CODEX_HOME + two wait helpers**

In `scripts/e2e-lib.sh`, change the daemon invocation inside
`start_router`:

```bash
  CODEXFERRY_CONFIG="$1" CODEX_HOME="$ARTIFACT_DIR/codex-home" \
    "$REPO_ROOT/target/debug/codexferry" \
    >"$ARTIFACT_DIR/router.log" 2>&1 &
```

(keep the surrounding cache-clear + ROUTER_PID lines as-is; the
`rm -f "$ARTIFACT_DIR/codex-home/models_cache.json"` line above it stays).
Append the helpers (near `wait_healthz`):

```bash
# Poll the router's live catalog until $1 (route slug) appears; 5s
# deadline. The failing message names the hot-reload contract because on
# a deaf watcher this is the red point.
wait_models_route() { # $1 route slug
  local deadline=$((SECONDS + 5))
  until curl -sf "http://127.0.0.1:$ROUTER_PORT/v1/models?client_version=e2e" \
      | grep -qF "\"$1\""; do
    [ "$SECONDS" -lt "$deadline" ] || fail "route $1 did not appear in /v1/models within 5s (daemon did not hot-reload)"
    sleep 0.1
  done
}

# Poll until $1 no longer exists; 5s deadline. Deletion happens in the
# config applier right after the write lock, so it trails the reload by
# at most a few ms.
wait_cache_gone() { # $1 file path
  local deadline=$((SECONDS + 5))
  while [ -f "$1" ]; do
    [ "$SECONDS" -lt "$deadline" ] || fail "$1 was not invalidated within 5s (reload missing codex cache invalidation)"
    sleep 0.05
  done
}
```

- [ ] **Step 2: Write the scenario in `scripts/e2e.sh`**

Insert after `scenario_cross_format_switch` (mirrors its structure; mock
scenario `multiturn` serves both turns like scenario_multiturn):

```bash
# Resume-after-reload (2026-08-28 second incident, reported against codex
# 0.148.0; source-verified NOT a codex regression - the 0.147/0.148 fetch
# chains are identical, see spec §Problem). A codex session created under
# config v1 must be able to RESUME and use a route that only exists in
# config v2 (atomic editor-style save). Codex's catalog is a
# process-startup snapshot + models_cache.json; the resumed process sees
# new routes only when the daemon hot-reloads AND the reload invalidates
# codex's cache so the resume's startup fetch refetches. This scenario
# pins both links plus the resumed turn itself.
scenario_resume_after_reload() {
  log "scenario: codex resume sees post-session config reload"
  local rec="$ARTIFACT_DIR/record-resume-reload.jsonl"
  start_mock multiturn "$rec"

  # Config v1: only mocka/chat (mockb/mockr route lines removed; unused
  # provider entries are valid - every provider has an api_key).
  local cfg="$ARTIFACT_DIR/config-resume.toml"
  write_router_config "$cfg" "$(free_port)" "$MOCK_PORT"
  sed -i '/"mockb\/chat"/d; /"mockr\/resp"/d' "$cfg"
  start_router "$cfg"

  # Turn 1: session created under v1; codex fetches the v1 catalog and
  # leaves a models cache that the reload must later invalidate.
  run_codex -m mocka/chat "Say exactly E2E_RR_T1_OK"
  grep -qF 'E2E_RR_T1_OK' <<<"$(sed -n '/^codex$/, $p' "$(last_codex_output)")" \
    || fail "turn 1 marker missing"
  assert_live_catalog_fetched mocka/chat
  local cache="$ARTIFACT_DIR/codex-home/models_cache.json"
  [ -f "$cache" ] || fail "turn 1 must leave a models cache to invalidate"

  # Atomic editor-style edit -> config v2 ADDS mockb/chat (same port; the
  # daemon is already bound). mv = rename(2) over the config: the exact
  # save style that killed the old inode-level watch.
  local cfg_v2="$ARTIFACT_DIR/config-resume-v2.toml"
  write_router_config "$cfg_v2" "$ROUTER_PORT" "$MOCK_PORT"
  sed -i '/"mockr\/resp"/d' "$cfg_v2"
  mv "$cfg_v2" "$cfg"

  # Link 1: the daemon hot-reloads (red on pre-fix code right here).
  wait_models_route mockb/chat
  # Link 2: the reload invalidates codex's cache.
  wait_cache_gone "$cache"

  # The resumed process: refetches the catalog at startup, must resolve
  # the route that did not exist when the session was created.
  run_codex_resume -m mockb/chat "Say exactly E2E_RR_T2_OK"
  grep -qF 'E2E_RR_T2_OK' <<<"$(sed -n '/^codex$/, $p' "$(last_codex_output)")" \
    || fail "turn 2 (resume) marker missing"
  assert_live_catalog_fetched mocka/chat mockb/chat

  # Both turns went to the (chat) upstream; turn 2 replays turn 1 inline.
  record_assert "$rec" '
    len(e) == 2
    and e[0]["path"] == "/v1/chat/completions"
    and e[1]["path"] == "/v1/chat/completions"
    and "Say exactly E2E_RR_T1_OK" in json.dumps(e[1]["body"])
  '
  cleanup_procs
  pass "resume_after_reload"
}
```

Register it (three spots at the bottom of `scripts/e2e.sh`):
- usage line: add `|resume_after_reload` between `cross_format_switch`
  and `stale_catalog`;
- the `want` validation `case`: add `resume_after_reload) ;;` arm;
- dispatch: `resume_after_reload) scenario_resume_after_reload ;;` and
  append `scenario_resume_after_reload;` to the `all` chain right after
  `scenario_cross_format_switch;`.

- [ ] **Step 3: Run the scenario (red on pre-fix code)**

Before Task 2 (or on a pre-fix checkout): `scripts/e2e.sh resume_after_reload`
Expected: FAIL - "route mockb/chat did not appear in /v1/models within 5s
(daemon did not hot-reload)" - the atomic `mv` killed the inode watch.

After Tasks 1-3: Expected: PASS end-to-end (turn-1 marker, route appears,
cache invalidated, resumed turn-2 marker, cache refetched with both
routes, record shape).

- [ ] **Step 4: Full e2e regression (CODEX_HOME forwarding touched every scenario)**

Run: `scripts/e2e.sh all`
Expected: PASS - the `start_router` CODEX_HOME change affects all
scenarios, so the whole ladder must be re-run, not just the new one.

- [ ] **Step 5: Commit**

```bash
git add scripts/e2e-lib.sh scripts/e2e.sh
git commit -m "test(e2e): resume-after-reload scenario + forward CODEX_HOME to the daemon"
```
