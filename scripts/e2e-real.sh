#!/usr/bin/env bash
# E2E (real layer): codex exec against real upstreams through a DEDICATED
# router instance. Costs tokens: prompts are tiny, <= 3 turns per route.
# Requires a debug build first: cargo build --quiet --bin codexferry.
# Usage: E2E_REAL_ROUTES="glm/glm-4.7 deepseek/deepseek-v4-flash" \
#        [E2E_REAL_CONFIG=path/to/cxf.toml] \
#        [E2E_REAL_MODE=dynamic|static] scripts/e2e-real.sh
#
# E2E_REAL_MODE picks the codex-side wiring:
#   dynamic (default) - auth.command + live /v1/models fetch
#                      (asserts every tested route arrived through /models).
#   static           - env_key + a model_catalog_json pin generated from
#                      E2E_REAL_CONFIG via codexferry gen-catalog; no live
#                      fetch, but the pinned file IS the model metadata
#                      source. This is the only place the static-mode
#                      example wiring is exercised end-to-end against real
#                      upstreams.
# Run the script once per mode to cover both wirings (each run costs its
# own set of tokens).
set -euo pipefail
source "$(dirname "$0")/e2e-lib.sh"

command -v codex >/dev/null || fail "codex CLI not on PATH"
# Two or more routes are REQUIRED: with a single route the resume-switch
# proof silently never runs, so the guard enforces the >=2 invariant the
# usage text advertises.
[ "$(wc -w <<<"${E2E_REAL_ROUTES:-}")" -ge 2 ] || fail "set E2E_REAL_ROUTES=\"route1 route2\" (>=2 required for the switch proof)"
# Never run REAL providers with the sandbox bypassed: E2E_CODEX_SANDBOX=bypass
# is the tools-scenario toggle (which turns on
# --dangerously-bypass-approvals-and-sandbox in e2e-lib.sh's run_codex) and must
# not leak into real-upstream turns.
[ "${E2E_CODEX_SANDBOX:-}" != "bypass" ] || fail "refusing real-provider run with E2E_CODEX_SANDBOX=bypass"
# Success-count helper for the resume-switch proof: scrape /metrics, find the
# error_class="" line for a route, and print its trailing counter as a number
# (0 is a sentinel for absent/unscrapable and is caught by the baseline guard
# before the resume, never by the after > before comparison).
metrics_success_count() { # $1 router port, $2 route
  local body line
  body=$(curl -sf "http://127.0.0.1:$1/metrics") || { echo 0; return; }
  # A plain `line=$(...)` assignment would abort under set -euo pipefail when
  # no metric line matches (the pipeline reports the first failing grep's
  # nonzero status), so the no-match path is protected by the `if` condition
  # instead; the empty capture becomes the 0 sentinel, same as the
  # curl-failure arm above (that arm is the one whose `||` guard makes the
  # empty-capture claim hold).
  if line=$(grep -F "route=\"$2\"" <<<"$body" | grep -F 'error_class=""' | grep -oE '[0-9]+$' | tail -1); then
    printf '%s\n' "$line"
  else
    echo 0
  fi
}

REAL_CONFIG="${E2E_REAL_CONFIG:-$HOME/.config/codexferry/cxf.toml}"
[ -f "$REAL_CONFIG" ] || fail "router config not found: $REAL_CONFIG"
[ -x "$REPO_ROOT/target/debug/codexferry" ] || fail "build codexferry first: cargo build --quiet --bin codexferry"

# Wiring mode (default dynamic for back-compat with the original script).
REAL_MODE="${E2E_REAL_MODE:-dynamic}"
case "$REAL_MODE" in
  dynamic) ;;
  static) ;;
  *) fail "unknown E2E_REAL_MODE: $REAL_MODE (dynamic|static)" ;;
esac

# The router has no --port override; derive a temp config with an ephemeral
# port so the resident instance is never touched.
PORT=$(free_port)
sed -E "s/^port[[:space:]]*=[[:space:]]*[0-9]+/port = $PORT/" "$REAL_CONFIG" > "$ARTIFACT_DIR/config-real.toml"

# Select the codex-side helpers for the chosen mode. Shell function-name
# indirection ($run_first) keeps the loop below mode-agnostic.
if [ "$REAL_MODE" = static ]; then
  # Generate the pinned catalog from the SAME temp router config so the
  # route slugs in the pin match the routes the router actually serves.
  "$REPO_ROOT/target/debug/codexferry" gen-catalog \
    --config "$ARTIFACT_DIR/config-real.toml" \
    --out    "$ARTIFACT_DIR/catalog.json" >/dev/null \
    || fail "gen-catalog failed"
  run_first=run_codex_static
  run_resume=run_codex_resume_static
  log "real layer mode: static (env_key + model_catalog_json pin)"
else
  run_first=run_codex
  run_resume=run_codex_resume
  log "real layer mode: dynamic (auth.command + live /v1/models)"
fi

# The lib's EXIT trap (installed at `source` time) already runs cleanup_procs
# and prints the artifact dir; do not install a second trap here.
CODEXFERRY_CONFIG="$ARTIFACT_DIR/config-real.toml" "$REPO_ROOT/target/debug/codexferry" \
  >"$ARTIFACT_DIR/router.log" 2>&1 &
ROUTER_PID=$!
wait_healthz "http://127.0.0.1:$PORT"

i=0
for route in $E2E_REAL_ROUTES; do
  i=$((i+1))
  log "real route $i: $route"
  ROUTER_PORT=$PORT
  "$run_first" -m "$route" "Reply with exactly OK"
  # The codex log echoes the prompt ("Reply with exactly OK"), so a whole-log
  # grep would false-pass on an empty assistant reply; assert the assistant
  # section only (from the `codex` marker onward, same drift fix as e2e.sh).
  grep -qiF 'OK' <<<"$(sed -n '/^codex$/, $p' "$(last_codex_output)")" || fail "empty/unexpected reply on $route"
  metrics_assert_contains "$PORT" "route=\"$route\""
  metrics_assert_contains "$PORT" 'error_class=""'
  metrics_assert_absent  "$PORT" 'error_class="stream_truncated"'
done

# Dynamic mode: command-auth wiring makes codex fetch the live catalog from
# THIS dedicated router on the first turn; assert every tested route
# actually arrived through /models (the cache file only exists when codex
# itself fetched it - fallback metadata never writes it). Static mode
# deliberately skips this: env_key providers never fetch, and the
# model_catalog_json pin forces StaticModelsManager regardless.
if [ "$REAL_MODE" = dynamic ]; then
  assert_live_catalog_fetched $E2E_REAL_ROUTES
fi

if [ "$(wc -w <<<"$E2E_REAL_ROUTES")" -ge 2 ]; then
  # The loop just ran the LAST route, so `exec resume --last` resumes the
  # final route; target the FIRST route to exercise a real cross-provider
  # switch and prove it via the metrics counter.
  first=$(awk '{print $1}' <<<"$E2E_REAL_ROUTES")
  before=$(metrics_success_count "$PORT" "$first")
  # A zero baseline means the scrape failed or no success line exists; had the
  # after > before comparison been the only check, a failed baseline could
  # false-pass the resume proof.
  [ "$before" -gt 0 ] || fail "baseline success counter missing for $first"
  log "real layer: resume turn on the first route (cross-provider switch)"
  ROUTER_PORT=$PORT
  "$run_resume" -m "$first" "Reply with exactly OK"
  # Assistant-section extraction again; the resume log also echoes the prompt.
  grep -qiF 'OK' <<<"$(sed -n '/^codex$/, $p' "$(last_codex_output)")" || fail "resume turn failed on $first"
  after=$(metrics_success_count "$PORT" "$first")
  [ "$after" -gt "$before" ] || fail "resume did not reach $first"
fi

grep -q 'response.failed' "$ARTIFACT_DIR/router.log" && fail "router logged response.failed" || true
pass "real layer ($E2E_REAL_ROUTES)"
