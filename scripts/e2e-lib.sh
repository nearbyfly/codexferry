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
# distinct providers so a mid-session switch stays cross-provider.
write_router_config() { # $1 path, $2 router port, $3 mock port
  cat > "$1" <<EOF
[server]
host = "127.0.0.1"
port = $2

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
  CODEX_ROUTER_CONFIG="$1" "$REPO_ROOT/target/debug/codexferry" \
    >"$ARTIFACT_DIR/router.log" 2>&1 &
  ROUTER_PID=$!
  wait_healthz "http://127.0.0.1:$ROUTER_PORT"
}

# codex exec with the e2e provider baked in. Never touches ~/.codex or the
# resident router's config: CODEX_HOME is redirected into the artifact dir so
# sessions/state stay out of the real ~/.codex (`-s read-only` only bounds the
# workspace sandbox, not the CLI's own storage).
# `model_provider="e2e"` must be selected explicitly: defining
# model_providers.e2e alone does not change which provider the CLI uses for
# `-m` routes, and the resident ~/.codex/config.toml may set a different
# default model_provider (e.g. "router") that would hijack the request.
# env_key is a dummy: the router does not authenticate clients.
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
  CODEX_HOME="$ARTIFACT_DIR/codex-home" E2E_DUMMY_KEY=dummy codex exec "${sandbox_flags[@]}" --skip-git-repo-check \
    -c 'model_provider="e2e"' \
    -c 'model_providers.e2e.name="e2e"' \
    -c "model_providers.e2e.base_url=\"http://127.0.0.1:${ROUTER_PORT}/v1\"" \
    -c 'model_providers.e2e.wire_api="responses"' \
    -c 'model_providers.e2e.env_key="E2E_DUMMY_KEY"' \
    "$@" >"$ARTIFACT_DIR/codex-$(date +%s%N).log" 2>&1
}

# Resume the most recent session — Codex CLI 0.147's `exec resume` takes its
# OWN -c/-m but NO -s/--sandbox short flag (parse-time exit 2), so unlike
# run_codex it is invoked without -s. Without -s, resume silently inherits
# the CLI default sandbox (`workspace-write`) with approvals never, which
# would let a resumed tool command write unsandboxed; the
# -c 'sandbox_mode="read-only"' override below restores the read-only
# posture (verified parse-clean on 0.147). The LONG-form
# --dangerously-bypass-approvals-and-sandbox DOES exist on resume — only the
# short -s flag does not. The provider overrides (including the
# model_provider selection; see run_codex) are repeated here. CODEX_HOME
# points at the same scratch home as run_codex so `--last` finds the session
# recorded there.
run_codex_resume() { # args…: -m <route> "<prompt>"
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

cleanup_procs() {
  kill "${ROUTER_PID:-}" "${MOCK_PID:-}" 2>/dev/null || true
  wait "${ROUTER_PID:-}" "${MOCK_PID:-}" 2>/dev/null || true
}
