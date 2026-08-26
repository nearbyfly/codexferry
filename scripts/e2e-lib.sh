#!/usr/bin/env bash
# Shared helpers for scripts/e2e.sh and scripts/e2e-real.sh (spec 2026-08-21).
# Sourced, not executed. Callers must `set -euo pipefail` themselves.

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ARTIFACT_DIR="$(mktemp -d /tmp/codexferry-e2e.XXXXXX)"
# EXIT trap: cleanup (safe when PIDs are unset/reaped — cleanup_procs guards
# with :-) and then print the artifact dir. Callers that install their own
# EXIT trap must re-include the print (Task 6 is pointed at this).
trap 'cleanup_procs 2>/dev/null || true; echo "[e2e] artifacts: $ARTIFACT_DIR"' EXIT

log()  { printf '[e2e] %s\n' "$*"; }
pass() { printf '[PASS] %s\n' "$*"; }
fail() { printf '[FAIL] %s\n' "$*" >&2; exit 1; }

# The port can be stolen between close() and the child's bind; that surfaces
# loudly (bind failure / wait_healthz timeout).
free_port() {
  python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'
}

wait_healthz() { # $1 = base url
  for _ in $(seq 1 100); do
    curl -sf "$1/healthz" >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  fail "router did not become healthy at $1"
}

# Temp router config: three routes over one mock instance. mocka/mockb are
# distinct providers so a mid-session switch stays cross-provider. $4 is an
# optional extra [server] line (e.g. "hide_bundled_models = true" for the
# hide_bundled scenario); empty renders a harmless blank line.
write_router_config() { # $1 path, $2 router port, $3 mock port, $4 optional extra [server] line
  cat > "$1" <<EOF
[server]
host = "127.0.0.1"
port = $2
${4:-}

[providers.mocka]
base_url = "http://127.0.0.1:$3/v1"
api_key = "test-key"
format = "chat"
timeout_ms = 30000

[providers.mockb]
base_url = "http://127.0.0.1:$3/v1"
api_key = "test-key"
format = "chat"
timeout_ms = 30000

[providers.mockr]
base_url = "http://127.0.0.1:$3/v1"
api_key = "test-key"
format = "responses"
timeout_ms = 30000

[routes]
"mocka/chat" = { model = "e2e-model" }
"mockb/chat" = { model = "e2e-model" }
"mockr/resp" = { model = "e2e-model" }
EOF
}

start_mock() { # $1 scenario, $2 record path — sets MOCK_PORT/MOCK_PID
  MOCK_PORT=$(free_port)
  "$REPO_ROOT/target/debug/e2e-mock" --port "$MOCK_PORT" --scenario "$1" --record "$2" \
    >"$ARTIFACT_DIR/mock-$1.log" 2>&1 &
  MOCK_PID=$!
  sleep 0.2
  kill -0 "$MOCK_PID" 2>/dev/null || fail "mock died (see $ARTIFACT_DIR/mock-$1.log)"
}

start_router() { # $1 config path — sets ROUTER_PORT/ROUTER_PID
  ROUTER_PORT=$(awk -F' = ' '/^port = / {print $2; exit}' "$1")
  # Each scenario starts a NEW router on a new port, but codex's models cache
  # ($CODEX_HOME/models_cache.json, TTL 300s) survives from the previous
  # scenario and is keyed only by client_version - a warm cache would let the
  # next scenario's run_codex skip the /models fetch entirely and false-green
  # the live-catalog assertions (scenario_models). Clear it per router start.
  rm -f "$ARTIFACT_DIR/codex-home/models_cache.json"
  CODEXFERRY_CONFIG="$1" "$REPO_ROOT/target/debug/codexferry" \
    >"$ARTIFACT_DIR/router.log" 2>&1 &
  ROUTER_PID=$!
  wait_healthz "http://127.0.0.1:$ROUTER_PORT"
}

# Dynamic-mode run (canonical): codex exec with the e2e provider baked in
# via auth.command, no model_catalog_json pin — codex fetches the live
# /v1/models catalog (run_codex_dynamic delegates here, so there is a
# single dynamic wiring body). Never touches ~/.codex or the
# resident router's config: CODEX_HOME is redirected into the artifact dir so
# sessions/state stay out of the real ~/.codex (`-s read-only` only bounds the
# workspace sandbox, not the CLI's own storage).
# `model_provider="e2e"` must be selected explicitly: defining
# model_providers.e2e alone does not change which provider the CLI uses for
# `-m` routes, and the resident ~/.codex/config.toml may set a different
# default model_provider (e.g. "codexferry") that would hijack the request.
# Auth is COMMAND-based (`auth = {command="echo", args=["dummy"]}`): codex
# only fetches the live /v1/models?client_version= catalog when the active
# auth is Codex-backed (ChatGPT OAuth, headers, agent identity, or personal
# access token) or the provider has an auth command - an env_key-only
# provider with no Codex-backed auth never fetches, so the old env_key
# wiring silently skipped catalog discovery in every scenario. The
# router does not authenticate clients; the echoed token is a dummy.
run_codex() { # args…: -m <route> "<prompt>"
  mkdir -p "$ARTIFACT_DIR/codex-home"
  # Sandbox selection: default read-only (Task 4). E2E_CODEX_SANDBOX=bypass
  # switches to --dangerously-bypass-approvals-and-sandbox for the tools
  # scenario: the E2E harness runs in an externally sandboxed container
  # where bubblewrap cannot create user namespaces (`unshare` -> EPERM), so
  # a real sandbox makes even `echo` fail before it runs. Codex documents
  # the bypass flag as intended for such environments; the scripted mock
  # only ever emits `echo E2E_TOOL_OK`, so the exposure is one read-only
  # command.
  local sandbox_flags=(-s read-only)
  if [ "${E2E_CODEX_SANDBOX:-read-only}" = bypass ]; then
    sandbox_flags=(--dangerously-bypass-approvals-and-sandbox)
  fi
  CODEX_HOME="$ARTIFACT_DIR/codex-home" codex exec "${sandbox_flags[@]}" --skip-git-repo-check \
    -c 'model_provider="e2e"' \
    -c 'model_providers.e2e.name="e2e"' \
    -c "model_providers.e2e.base_url=\"http://127.0.0.1:${ROUTER_PORT}/v1\"" \
    -c 'model_providers.e2e.wire_api="responses"' \
    -c 'model_providers.e2e.auth={command="echo",args=["dummy"]}' \
    "$@" >"$ARTIFACT_DIR/codex-$(date +%s%N).log" 2>&1
}

# Dynamic-mode resume: same command-auth wiring as run_codex (no
# model_catalog_json pin), for `codex exec resume --last`. Codex CLI
# 0.147's `exec resume` takes its OWN -c/-m but NO -s/--sandbox short flag
# (parse-time exit 2), so unlike
# run_codex it is invoked without -s. Without -s, resume silently inherits
# the CLI default sandbox (`workspace-write`) with approvals never, which
# would let a resumed tool command write unsandboxed; the
# -c 'sandbox_mode="read-only"' override below restores the read-only
# posture (verified parse-clean on 0.147). The LONG-form
# --dangerously-bypass-approvals-and-sandbox DOES exist on resume — only the
# short -s flag does not. The provider overrides (including the
# model_provider selection; see run_codex) are repeated here. CODEX_HOME
# points at the same scratch home as run_codex so `--last` finds the session
# recorded there. Auth stays command-based for the same reason as run_codex
# (env_key providers never fetch the live catalog).
run_codex_resume() { # args…: -m <route> "<prompt>"
  mkdir -p "$ARTIFACT_DIR/codex-home"
  CODEX_HOME="$ARTIFACT_DIR/codex-home" codex exec resume --last --skip-git-repo-check \
    -c 'model_provider="e2e"' \
    -c 'sandbox_mode="read-only"' \
    -c 'model_providers.e2e.name="e2e"' \
    -c "model_providers.e2e.base_url=\"http://127.0.0.1:${ROUTER_PORT}/v1\"" \
    -c 'model_providers.e2e.wire_api="responses"' \
    -c 'model_providers.e2e.auth={command="echo",args=["dummy"]}' \
    "$@" >"$ARTIFACT_DIR/codex-resume-$(date +%s%N).log" 2>&1
}

# Pinned-mode wiring (canonical static-mode helper): env_key + a
# pre-generated model_catalog.json pin. Under this wiring codex MUST NOT
# fetch /v1/models (env_key providers never fetch, and the pin forces
# StaticModelsManager regardless of auth). The
# catalog path is taken from $ARTIFACT_DIR/catalog.json by convention -
# the scenario that calls this is responsible for generating the file with
# `codexferry gen-catalog` before invoking run_codex_static. The path is
# emitted as an absolute path so the `-c model_catalog_json=...` value
# resolves against CODEX_HOME the same way codex resolves it for users
# (relative paths in codex would otherwise be relative to ~/.codex/, but
# here CODEX_HOME is the artifact dir so an absolute path keeps things
# unambiguous).
run_codex_static() { # args…: -m <route> "<prompt>"
  mkdir -p "$ARTIFACT_DIR/codex-home"
  CODEX_HOME="$ARTIFACT_DIR/codex-home" E2E_DUMMY_KEY=dummy codex exec -s read-only --skip-git-repo-check \
    -c 'model_provider="e2e"' \
    -c 'model_providers.e2e.name="e2e"' \
    -c "model_providers.e2e.base_url=\"http://127.0.0.1:${ROUTER_PORT}/v1\"" \
    -c 'model_providers.e2e.wire_api="responses"' \
    -c 'model_providers.e2e.env_key="E2E_DUMMY_KEY"' \
    -c "model_catalog_json=\"$ARTIFACT_DIR/catalog.json\"" \
    "$@" >"$ARTIFACT_DIR/codex-$(date +%s%N).log" 2>&1
}

# Pinned-mode resume: same env_key + model_catalog_json wiring as
# run_codex_static (pinned mode), but for `codex exec resume --last`.
# Mirrors the dynamic run_codex_resume shape (no -s short flag, restored via
# sandbox_mode override; the catalog pin stays absolute so resume sees
# the same model metadata the original turn saw).
run_codex_resume_static() { # args…: -m <route> "<prompt>"
  mkdir -p "$ARTIFACT_DIR/codex-home"
  CODEX_HOME="$ARTIFACT_DIR/codex-home" E2E_DUMMY_KEY=dummy codex exec resume --last --skip-git-repo-check \
    -c 'model_provider="e2e"' \
    -c 'sandbox_mode="read-only"' \
    -c 'model_providers.e2e.name="e2e"' \
    -c "model_providers.e2e.base_url=\"http://127.0.0.1:${ROUTER_PORT}/v1\"" \
    -c 'model_providers.e2e.wire_api="responses"' \
    -c 'model_providers.e2e.env_key="E2E_DUMMY_KEY"' \
    -c "model_catalog_json=\"$ARTIFACT_DIR/catalog.json\"" \
    "$@" >"$ARTIFACT_DIR/codex-resume-$(date +%s%N).log" 2>&1
}

# Dynamic-mode run: auth command and NO model_catalog_json pin. Identical
# wiring to run_codex (the canonical dynamic helper), so this delegates to
# it — a single body, and the two names cannot diverge. Under this wiring
# codex fetches the live /v1/models catalog from the router each session
# start (OpenAiModelsManager, has_command_auth gate), so scenarios using it
# can assert the codex-side live-fetch proof (assert_live_catalog_fetched).
# Deliberately no env_key: an env_key-only provider never fetches, which
# would silently turn the scenario into the degraded fallback path.
run_codex_dynamic() { # args…: -m <route> "<prompt>"
  run_codex "$@"
}

# Fallback-mode run: env_key only, NO model_catalog_json pin and NO auth
# command. Under this wiring codex never fetches /v1/models and resolves
# routes with degraded fallback metadata — the probe still tests the wire
# shape, but the live-catalog discovery path is not exercised.
run_codex_fallback() { # args…: -m <route> "<prompt>"
  mkdir -p "$ARTIFACT_DIR/codex-home"
  local sandbox_flags=(-s read-only)
  if [ "${E2E_CODEX_SANDBOX:-read-only}" = bypass ]; then
    sandbox_flags=(--dangerously-bypass-approvals-and-sandbox)
  fi
  CODEX_HOME="$ARTIFACT_DIR/codex-home" E2E_DUMMY_KEY=dummy codex exec "${sandbox_flags[@]}" --skip-git-repo-check \
    -c 'model_provider="e2e"' \
    -c 'model_providers.e2e.name="e2e"' \
    -c "model_providers.e2e.base_url=\"http://127.0.0.1:${ROUTER_PORT}/v1\"" \
    -c 'model_providers.e2e.wire_api="responses"' \
    -c 'model_providers.e2e.env_key="E2E_DUMMY_KEY"' \
    "$@" >"$ARTIFACT_DIR/codex-$(date +%s%N).log" 2>&1
}

# Fallback-mode resume: same env_key-only wiring as run_codex_fallback, but
# for `codex exec resume --last`. Added by symmetry with
# run_codex_resume_static (no -s short flag, sandbox restored via the
# sandbox_mode override) so every mode has a first-turn + resume pair; no
# current scenario uses it, but e2e-real.sh-style suites can pick it up.
run_codex_resume_fallback() { # args…: -m <route> "<prompt>"
  mkdir -p "$ARTIFACT_DIR/codex-home"
  CODEX_HOME="$ARTIFACT_DIR/codex-home" E2E_DUMMY_KEY=dummy codex exec resume --last --skip-git-repo-check \
    -c 'model_provider="e2e"' \
    -c 'sandbox_mode="read-only"' \
    -c 'model_providers.e2e.name="e2e"' \
    -c "model_providers.e2e.base_url=\"http://127.0.0.1:${ROUTER_PORT}/v1\"" \
    -c 'model_providers.e2e.wire_api="responses"' \
    -c 'model_providers.e2e.env_key="E2E_DUMMY_KEY"' \
    "$@" >"$ARTIFACT_DIR/codex-resume-$(date +%s%N).log" 2>&1
}

# `codex debug models` under the canonical dynamic wiring (auth command, no
# pin): prints the MERGED catalog codex would use — bundled base + the
# router's /v1/models overlay applied by apply_remote_models. This is the
# observation point for hide_bundled_models: the merged JSON shows whether
# the router's visibility:"hide" overrides actually suppressed the bundled
# models INSIDE codex (the cargo integration test can only see the wire).
# No sandbox flags: no session is started. Stderr goes to a log so the
# caller can capture stdout as the catalog JSON.
run_codex_debug_models() {
  mkdir -p "$ARTIFACT_DIR/codex-home"
  CODEX_HOME="$ARTIFACT_DIR/codex-home" codex debug models \
    -c 'model_provider="e2e"' \
    -c 'model_providers.e2e.name="e2e"' \
    -c "model_providers.e2e.base_url=\"http://127.0.0.1:${ROUTER_PORT}/v1\"" \
    -c 'model_providers.e2e.wire_api="responses"' \
    -c 'model_providers.e2e.auth={command="echo",args=["dummy"]}' \
    2>"$ARTIFACT_DIR/codex-debug-models-$(date +%s%N).log"
}

# Assert that codex actually FETCHED the live catalog: the models cache file
# codex writes after a successful /models fetch must exist and contain every
# given route slug. Only codex writes this file (a curl probe never does), so
# its presence after a cleared-cache run_codex is the codex-side discovery
# proof - the missing piece of the old "discovery implied" assumption, which
# was false under env_key wiring (codex never fetched and still started fine
# on fallback metadata).
assert_live_catalog_fetched() { # $1… route slugs
  local cache="$ARTIFACT_DIR/codex-home/models_cache.json"
  [ -f "$cache" ] || fail "codex did not write $cache - live /models fetch never happened"
  python3 - "$cache" "$@" <<'PY'
import json, sys
cache = json.load(open(sys.argv[1]))
slugs = {m.get("slug") for m in cache.get("models", [])}
missing = [r for r in sys.argv[2:] if r not in slugs]
if missing:
    print(f"live catalog cache missing routes: {missing} (has {sorted(slugs)})", file=sys.stderr)
    sys.exit(1)
PY
}

last_codex_output() { # newest codex-*.log in ARTIFACT_DIR
  local f
  f=$(ls -t "$ARTIFACT_DIR"/codex*.log 2>/dev/null | head -1) || true
  [ -n "$f" ] || fail "no codex logs yet"
  printf '%s\n' "$f"
}

# Assert a python expression over the record entries (list `e`, module json).
# Example: record_assert "$REC" 'len(e) == 2 and "E2E_TOOL_OK" in json.dumps(e[1])'
record_assert() { # $1 record path, $2 expression
  python3 - "$1" "$2" <<'PY'
import json, sys
try:
    entries = [json.loads(line) for line in open(sys.argv[1]) if line.strip()]
    # Multi-line expressions (plan style) may carry incidental indentation
    # from the shell call site; trim per line and join with spaces so eval
    # sees clean Python. Limitation: expressions must NOT contain inline `#`
    # comments or multi-line string literals — the line-wise trim/join would
    # mangle them. (All current e2e expressions avoid both.)
    expr = " ".join(l.strip() for l in sys.argv[2].splitlines() if l.strip())
    ok = eval(expr, {"e": entries, "json": json})
except Exception as exc:
    print(f"record assertion failed: {sys.argv[2]} ({exc})", file=sys.stderr)
    sys.exit(1)
if not ok:
    print(f"record assertion failed: {sys.argv[2]}", file=sys.stderr)
    sys.exit(1)
PY
}

metrics_assert_contains() { # $1 router port, $2 substring
  local body
  body=$(curl -sf "http://127.0.0.1:$1/metrics") || fail "cannot scrape /metrics on $1"
  grep -qF "$2" <<<"$body" || fail "metrics missing: $2"
}

metrics_assert_absent() { # $1 router port, $2 substring
  local body
  body=$(curl -sf "http://127.0.0.1:$1/metrics") || fail "cannot scrape /metrics on $1"
  ! grep -qF "$2" <<<"$body" || fail "metrics unexpectedly contain: $2"
}

# Doctor offline quick-check runner (Task 10): scopes HOME to the artifact
# doctor-home so `codexferry doctor` reads the scenario-supplied
# ~/.codex/config.toml and never touches the user's real home. Stores the
# captured report in DOCTOR_LAST_OUTPUT so scenarios can assert extra lines
# after the common shape. `|| true` is required: a FAILing doctor exits 1 and
# the script runs with `set -euo pipefail`.
assert_doctor_offline_passes() { # $1 router-config-path, $2 expected-mode
  local out
  out=$(HOME="$ARTIFACT_DIR/doctor-home" "$REPO_ROOT/target/debug/codexferry" doctor --offline --config "$1" 2>&1 || true)
  DOCTOR_LAST_OUTPUT=$out
  grep -qF 'INFO: detected mode' <<<"$out" || fail "doctor output missing detected-mode INFO line (expected $2)"$'\n'"$out"
  grep -qF "detected mode — $2 (" <<<"$out" || fail "doctor output detected a different mode than $2"$'\n'"$out"
  ! grep -qF 'FAIL:' <<<"$out" || fail "doctor output has FAIL lines (expected mode $2)"$'\n'"$out"
}

# Same invocation as assert_doctor_offline_passes, but for scenarios that must
# see a FAIL (currently unused; intended for future stale-pin scenarios).
assert_doctor_offline_fails() { # $1 router-config-path, $2 expected-fail-substr
  [ -n "${2:-}" ] || fail "expected-fail-substr is required"
  local out
  out=$(HOME="$ARTIFACT_DIR/doctor-home" "$REPO_ROOT/target/debug/codexferry" doctor --offline --config "$1" 2>&1 || true)
  DOCTOR_LAST_OUTPUT=$out
  grep -qF 'FAIL:' <<<"$out" || fail "doctor output has no FAIL line (expected: $2)"$'\n'"$out"
  grep -qF "$2" <<<"$out" || fail "doctor output missing expected FAIL detail '$2'"$'\n'"$out"
}

cleanup_procs() {
  kill "${ROUTER_PID:-}" "${MOCK_PID:-}" 2>/dev/null || true
  wait "${ROUTER_PID:-}" "${MOCK_PID:-}" 2>/dev/null || true
}
