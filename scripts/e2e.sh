#!/usr/bin/env bash
# E2E: real Codex CLI against codexferry over the scripted e2e-mock upstream.
# Usage: scripts/e2e.sh [basic|models|static|tools|multiturn|cross_format_switch|stale_catalog|doctor_dynamic|doctor_pinned|doctor_fallback|all]   (default: all)
# Requires: codex CLI on PATH, python3, curl. Never touches ~/.codex or the
# resident router's config — the CLI's CODEX_HOME is redirected to the
# artifact dir. Markers that also appear in the user-prompt echo are asserted
# against section-extracted CLI output only, so a mutation cannot false-pass
# via the prompt (spec 2026-08-21).
set -euo pipefail
source "$(dirname "$0")/e2e-lib.sh"

scenario_basic() {
  log "scenario: basic chat turn"
  local rec="$ARTIFACT_DIR/record-basic.jsonl"
  start_mock basic "$rec"
  write_router_config "$ARTIFACT_DIR/config-basic.toml" "$(free_port)" "$MOCK_PORT"
  start_router "$ARTIFACT_DIR/config-basic.toml"
  local expect="${E2E_EXPECT_OVERRIDE:-E2E_BASIC_OK}"

  run_codex -m mocka/chat "Reply with exactly $expect"
  # Drift fix (spec 2026-08-21): the codex log echoes the user prompt verbatim,
  # so grepping the whole log lets a WRONG marker pass via the prompt echo.
  # Assert against the assistant turn only (from the `codex` marker onward).
  local out
  out=$(sed -n '/^codex$/, $p' "$(last_codex_output)")
  grep -qF "$expect" <<<"$out" || fail "CLI output missing marker '$expect'"
  # Drift fix (spec 2026-08-21): the strict `store is False` assertion cannot
  # hold on the chat record — to_chat_request() drops the Responses-only
  # `store` field (no Chat equivalent), so absence is expected here, while the
  # responses path (record e[1]) still carries store: false from Codex.
  # Spec §5: `previous_response_id` is consumed by the router, never
  # forwarded, so it must be absent from the upstream record.
  record_assert "$rec" \
    'len(e) == 1 and e[0]["body"].get("store") is not True and "previous_response_id" not in e[0]["body"] and isinstance(e[0]["body"].get("tools"), list) and len(e[0]["body"]["tools"]) > 0'
  metrics_assert_contains "$ROUTER_PORT" \
    'upstream_requests_total{provider="mocka",route="mocka/chat",model="e2e-model",error_class=""} 1'
  metrics_assert_contains "$ROUTER_PORT" 'upstream_ttft_seconds_count'
  metrics_assert_absent  "$ROUTER_PORT" 'error_class="stream_truncated"'

  # Same scenario over the responses-format route (passthrough CLI compat).
  # Spec matrix (2026-08-21): `store: false` is asserted HERE on the
  # responses-path record — Codex sends it on /v1/responses, while the chat
  # path's request cannot carry the Responses-only field (dropped by
  # to_chat_request), which the chat record assertion above already notes.
  run_codex -m mockr/resp "Reply with exactly E2E_RESP_OK"
  out=$(sed -n '/^codex$/, $p' "$(last_codex_output)")
  grep -qF 'E2E_RESP_OK' <<<"$out" || fail "CLI output missing E2E_RESP_OK (passthrough route)"
  record_assert "$rec" \
    'len(e) == 2 and e[1]["path"] == "/v1/responses" and e[1]["body"]["stream"] is True and e[1]["body"].get("store") is False'
  metrics_assert_contains "$ROUTER_PORT" \
    'upstream_requests_total{provider="mockr",route="mockr/resp",model="e2e-model",error_class=""} 1'
  cleanup_procs
  pass "basic"
}

scenario_models() {
  log "scenario: /models catalog discovery"
  start_mock basic "$ARTIFACT_DIR/record-models.jsonl"   # /models is router-served; mock unused
  write_router_config "$ARTIFACT_DIR/config-models.toml" "$(free_port)" "$MOCK_PORT"
  start_router "$ARTIFACT_DIR/config-models.toml"

  local body
  body=$(curl -sf "http://127.0.0.1:$ROUTER_PORT/v1/models?client_version=1")
  grep -qF '"models":[' <<<"$body" || fail "catalog shape wrong: $body"
  for route in mocka/chat mockb/chat mockr/resp; do
    grep -qF "$route" <<<"$body" || fail "catalog missing route $route"
  done
  local etag
  # Drift fix (spec 2026-08-21): curl's -D output uses CRLF, and the plan's
  # sed regex leaves the CR inside the capture (GNU sed greediness), so the
  # back-sent If-None-Match contained a control char and the router answered
  # 400 before the handler ran. Strip CRs first, then extract the quoted tag.
  etag=$(curl -sf -D - -o /dev/null "http://127.0.0.1:$ROUTER_PORT/v1/models?client_version=1" | tr -d '\r' | sed -n 's/^[Ee][Tt][Aa][Gg]: \(.*\)$/\1/p')
  [ -n "$etag" ] || fail "no ETag emitted"
  local code
  code=$(curl -s -o /dev/null -w '%{http_code}' -H "If-None-Match: $etag" "http://127.0.0.1:$ROUTER_PORT/v1/models?client_version=1")
  [ "$code" = "304" ] || fail "expected 304 on If-None-Match, got $code"

  # A normal CLI start must succeed against this router AND must have
  # resolved its route through the live catalog. The old "discovery implied"
  # assumption was false: under the previous env_key wiring codex never
  # fetched /models and still started fine on fallback metadata. The
  # command-auth wiring (e2e-lib.sh run_codex) plus the cache assertion below
  # prove codex-side discovery for real.
  run_codex -m mocka/chat "Reply with exactly E2E_BASIC_OK"
  out=$(sed -n '/^codex$/, $p' "$(last_codex_output)")
  grep -qF 'E2E_BASIC_OK' <<<"$out" || fail "CLI start against catalog router failed"
  assert_live_catalog_fetched mocka/chat mockb/chat mockr/resp
  cleanup_procs
  pass "models"
}

# Static-mode counterpart to `models`: codex is wired with env_key and a
# model_catalog_json pin (no auth.command, no live fetch). The mock's
# /v1/models probe endpoint records any hit so we can assert codex
# genuinely never fetched - the routes must work via the pinned catalog
# alone. This scenario is the only place the static wiring
# (scripts/codex-config-static.toml.example) is exercised.
scenario_static() {
  log "scenario: static catalog (env_key + model_catalog_json pin)"
  local rec="$ARTIFACT_DIR/record-static.jsonl"
  start_mock basic "$rec"
  write_router_config "$ARTIFACT_DIR/config-static.toml" "$(free_port)" "$MOCK_PORT"
  start_router "$ARTIFACT_DIR/config-static.toml"

  # Generate the pinned catalog from the SAME temp router config, so the
  # route slugs the pin exposes match the router's routes exactly.
  "$REPO_ROOT/target/debug/codexferry" gen-catalog \
    --config "$ARTIFACT_DIR/config-static.toml" \
    --out    "$ARTIFACT_DIR/catalog.json" >/dev/null \
    || fail "gen-catalog failed"

  run_codex_static -m mocka/chat "Reply with exactly E2E_BASIC_OK"
  local out
  out=$(sed -n '/^codex$/, $p' "$(last_codex_output)")
  grep -qF 'E2E_BASIC_OK' <<<"$out" || fail "CLI start under static-mode wiring failed (chat)"
  # Second turn covers the responses wire - same scenario, different format.
  # Mock returns E2E_RESP_OK on /v1/responses (vs E2E_BASIC_OK on chat), so the
  # assistant-section grep distinguishes a successful responses round-trip
  # from a chat round-trip.
  run_codex_static -m mockr/resp "Reply with exactly E2E_RESP_OK"
  out=$(sed -n '/^codex$/, $p' "$(last_codex_output)")
  grep -qF 'E2E_RESP_OK' <<<"$out" || fail "CLI start under static-mode wiring failed (responses)"
  # Both wires reached the upstream; static-mode defining property (no
  # /v1/models call) holds for the whole session.
  record_assert "$rec" '
    any(e["path"] == "/v1/chat/completions" for e in e)
    and any(e["path"] == "/v1/responses" for e in e)
    and not any(e["path"] == "/v1/models" for e in e)
  '
  metrics_assert_contains "$ROUTER_PORT" \
    'upstream_requests_total{provider="mocka",route="mocka/chat",model="e2e-model",error_class=""} 1'
  metrics_assert_contains "$ROUTER_PORT" \
    'upstream_requests_total{provider="mockr",route="mockr/resp",model="e2e-model",error_class=""} 1'
  cleanup_procs
  pass "static"
}

# Tool round-trip: turn 1 asks for a shell tool call; the mock emits a
# calibrated exec_command call (args {"cmd":"echo E2E_TOOL_OK"}, see
# src/bin/e2e-mock.rs) whose output must reach the model on turn 2 and whose
# call id `call_e2e_1` must round-trip untouched (spec §7.1 tools conversion).
# E2E_EXPECT_OVERRIDE swaps the tool marker so the tool-section and
# assistant-section greps are mutation-testable.
scenario_tools() {
  log "scenario: tool round-trip"
  local rec="$ARTIFACT_DIR/record-tools.jsonl"
  start_mock tools "$rec"
  write_router_config "$ARTIFACT_DIR/config-tools.toml" "$(free_port)" "$MOCK_PORT"
  start_router "$ARTIFACT_DIR/config-tools.toml"
  local expect="${E2E_EXPECT_OVERRIDE:-E2E_TOOL_OK}"

  # Tools must actually EXECUTE. The E2E harness runs in an externally
  # sandboxed container where bubblewrap cannot create user namespaces, so
  # `-s read-only` makes every command fail before running; the scenario
  # opts into codex's documented bypass flag (see e2e-lib.sh run_codex). The
  # mock only ever emits this one scripted echo.
  E2E_CODEX_SANDBOX=bypass run_codex -m mocka/chat "Use the shell tool to run: echo $expect, then reply with exactly E2E_TOOL_DONE"
  local out
  out="$(last_codex_output)"
  # E2E_TOOL_OK lives in the tool-result section (between the `exec` header
  # and the `codex` assistant header). The prompt echo holds the marker too,
  # so a whole-log grep would let a WRONG marker pass via the prompt —
  # extract the tool section only (spec 2026-08-21).
  local toolsec
  toolsec=$(sed -n '/^exec$/,/^codex$/p' "$out")
  [ -n "$toolsec" ] || fail "no tool-result section in CLI log (tool not executed?)"
  grep -qF "$expect" <<<"$toolsec" || fail "tool output missing (tool not executed?)"
  # E2E_TOOL_DONE is the assistant's convergence reply; grep from the `codex`
  # marker onward so the prompt echo cannot false-pass.
  grep -qF 'E2E_TOOL_DONE' <<<"$(sed -n '/^codex$/, $p' "$out")" || fail "convergence marker missing"

  # Turn 2 must carry the tool result back; the call id must round-trip. The
  # marker must appear in the role=tool message ITSELF (not merely in the
  # replayed user history, which would false-pass even when the tool failed):
  # json.dumps(e[1]) alone is not enough here.
  record_assert "$rec" 'len(e) == 2 and "call_e2e_1" in json.dumps(e[1]) and "E2E_TOOL_OK" in json.dumps(e[1]) and any(m.get("role") == "tool" and "E2E_TOOL_OK" in json.dumps(m) for m in e[1]["body"].get("messages", []))'
  # The executed command's output reaches the model: the second request's
  # tool result contains E2E_TOOL_OK.
  metrics_assert_contains "$ROUTER_PORT" 'upstream_requests_total{provider="mocka",route="mocka/chat",model="e2e-model",error_class=""} 2'
  cleanup_procs
  pass "tools"
}

# Multi-turn + mid-session model switch: turn 1 goes to mocka/chat, then the
# SAME session resumes on mockb/chat. Codex replays its transcript inline
# (store:false), so the turn-2 request contains the turn-1 user text and
# assistant reply, and the router's previous_response_id never appears
# upstream (spec §5; cross-provider switch per spec §8.6).
# E2E_EXPECT_OVERRIDE swaps turn 1's marker so both
# assistant-section greps are mutation-testable.
scenario_multiturn() {
  log "scenario: multi-turn + mid-session model switch"
  local rec="$ARTIFACT_DIR/record-multiturn.jsonl"
  start_mock multiturn "$rec"
  write_router_config "$ARTIFACT_DIR/config-multiturn.toml" "$(free_port)" "$MOCK_PORT"
  start_router "$ARTIFACT_DIR/config-multiturn.toml"
  local expect="${E2E_EXPECT_OVERRIDE:-E2E_TURN_1_OK}"

  run_codex -m mocka/chat "Say exactly $expect"
  grep -qF "$expect" <<<"$(sed -n '/^codex$/, $p' "$(last_codex_output)")" || fail "turn 1 marker missing"
  # Resume the SAME session on the OTHER provider's route (`resume` accepts
  # -m, verified against codex 0.147; -s does not exist on resume — see the
  # e2e-lib.sh run_codex_resume comment).
  run_codex_resume -m mockb/chat "Say exactly E2E_TURN_2_OK"
  grep -qF 'E2E_TURN_2_OK' <<<"$(sed -n '/^codex$/, $p' "$(last_codex_output)")" || fail "turn 2 marker missing"

  # Turn 2 (to mockb) replays turn 1 inline: the request contains the turn-1
  # user text AND the turn-1 assistant reply, with no previous_response_id.
  # The assistant clause is role-scoped (mirroring the tools pattern): the
  # plan's plain json.dumps(e[1]["body"]) clause is also satisfied by the
  # turn-1 USER text, so it alone would let a replay that dropped the
  # assistant reply false-pass.
  record_assert "$rec" '
    len(e) == 2
    and e[1]["path"] == "/v1/chat/completions"
    and "Say exactly E2E_TURN_1_OK" in json.dumps(e[1]["body"])
    and "E2E_TURN_1_OK" in json.dumps(e[1]["body"])
    and any(m.get("role") == "assistant" and "E2E_TURN_1_OK" in json.dumps(m) for m in e[1]["body"].get("messages", []))
    and "previous_response_id" not in e[1]["body"]
  '
  metrics_assert_contains "$ROUTER_PORT" 'upstream_requests_total{provider="mocka",route="mocka/chat",model="e2e-model",error_class=""} 1'
  metrics_assert_contains "$ROUTER_PORT" 'upstream_requests_total{provider="mockb",route="mockb/chat",model="e2e-model",error_class=""} 1'
  cleanup_procs
  pass "multiturn"
}

# Cross-wire session switch (chat -> responses): turn 1 on a chat route
# (mocka/chat), turn 2 resumes on a responses route (mockr/resp). This is
# the proxy's headline feature - mid-session model switch across upstreams
# AND wire formats - and it must keep history coherent. We assert the
# router saw both routes (+1 each in /metrics), the mock's record shows
# the wire-format split (e[0] chat, e[1] responses), and turn 2's request
# body carries turn 1's user prompt + assistant reply across the
# format boundary.
scenario_cross_format_switch() {
  log "scenario: chat->responses session switch"
  local rec="$ARTIFACT_DIR/record-cfs.jsonl"
  start_mock basic "$rec"
  write_router_config "$ARTIFACT_DIR/config-cfs.toml" "$(free_port)" "$MOCK_PORT"
  start_router "$ARTIFACT_DIR/config-cfs.toml"

  run_codex -m mocka/chat "Say exactly E2E_CFS_T1"
  local out
  out=$(sed -n '/^codex$/, $p' "$(last_codex_output)")
  grep -qF 'E2E_BASIC_OK' <<<"$out" || fail "turn 1 (chat) marker missing"

  run_codex_resume -m mockr/resp "Say exactly E2E_CFS_T2"
  out=$(sed -n '/^codex$/, $p' "$(last_codex_output)")
  grep -qF 'E2E_RESP_OK' <<<"$out" || fail "turn 2 (responses) marker missing"

  record_assert "$rec" '
    len(e) == 2
    and e[0]["path"] == "/v1/chat/completions"
    and e[1]["path"] == "/v1/responses"
    and "Say exactly E2E_CFS_T1" in json.dumps(e[1]["body"])
    and "E2E_BASIC_OK" in json.dumps(e[1]["body"])
  '
  metrics_assert_contains "$ROUTER_PORT" \
    'upstream_requests_total{provider="mocka",route="mocka/chat",model="e2e-model",error_class=""} 1'
  metrics_assert_contains "$ROUTER_PORT" \
    'upstream_requests_total{provider="mockr",route="mockr/resp",model="e2e-model",error_class=""} 1'
  cleanup_procs
  pass "cross_format_switch"
}

# Stale-pin regression: router exposes 3 routes, but the catalog pin was
# generated from a narrower config that only lists one of them. codex
# is wired with env_key + the stale pin, then asked to use a route that
# is in the router but absent from the pin. This pins down the
# architectural separation between routing (the router's job, driven
# by its own config) and model metadata (the pin's job, codex-side):
# the request must still reach the upstream via the router, and codex
# must NOT fetch /v1/models to "fix" the missing entry (the pin forces
# StaticModelsManager regardless of auth shape). If a future change ever
# adds a silent "pin-miss -> live fetch" fallback, the /v1/models
# assertion catches it.
scenario_stale_catalog() {
  log "scenario: stale catalog (pin missing a route)"
  local rec="$ARTIFACT_DIR/record-stale.jsonl"
  start_mock basic "$rec"
  write_router_config "$ARTIFACT_DIR/config-stale.toml" "$(free_port)" "$MOCK_PORT"
  start_router "$ARTIFACT_DIR/config-stale.toml"

  # Build a deliberately-narrow pin source: keep the full [providers]
  # section from the router config, restrict [routes] to just mocka/chat.
  # The router will serve 3 routes; the pin will list 1.
  awk '/^\[routes\]/,0 {exit} {print}' "$ARTIFACT_DIR/config-stale.toml" > "$ARTIFACT_DIR/config-stale-pin.toml"
  cat >> "$ARTIFACT_DIR/config-stale-pin.toml" <<'EOF'
[routes]
"mocka/chat" = { model = "e2e-model" }
EOF
  "$REPO_ROOT/target/debug/codexferry" gen-catalog \
    --config "$ARTIFACT_DIR/config-stale-pin.toml" \
    --out    "$ARTIFACT_DIR/catalog.json" >/dev/null \
    || fail "gen-catalog failed"

  run_codex_static -m mockr/resp "Reply with exactly E2E_RESP_OK"
  local out
  out=$(sed -n '/^codex$/, $p' "$(last_codex_output)")
  grep -qF 'E2E_RESP_OK' <<<"$out" || fail "stale pin: route mockr/resp did not reach the upstream"
  record_assert "$rec" '
    any(e["path"] == "/v1/responses" for e in e)
    and not any(e["path"] == "/v1/models" for e in e)
  '
  metrics_assert_contains "$ROUTER_PORT" \
    'upstream_requests_total{provider="mockr",route="mockr/resp",model="e2e-model",error_class=""} 1'
  cleanup_procs
  pass "stale_catalog"
}

# Doctor offline quick-checks under DYNAMIC codex wiring (spec 2026-08-23
# verification matrix): the scenario drops ~/.codex/config.toml with an
# auth.command and NO model_catalog_json pin, runs `doctor --offline` against
# the same temp router, and asserts the report detects Dynamic, does not warn
# about a pin shadowing the live fetch, and PASSes the dynamic-mode endpoint
# checks (L2.8'/L2.9') plus the mode-independent codex version age INFO (L2.6).
# HOME is scoped to $ARTIFACT_DIR/doctor-home for the doctor invocation only,
# so the user's real ~/.codex is never read or written.
scenario_doctor_dynamic() {
  log "scenario: doctor offline under dynamic wiring"
  local cfg="$ARTIFACT_DIR/config-doctor-dynamic.toml"
  start_mock basic "$ARTIFACT_DIR/record-doctor-dynamic.jsonl"
  write_router_config "$cfg" "$(free_port)" "$MOCK_PORT"
  start_router "$cfg"

  mkdir -p "$ARTIFACT_DIR/doctor-home/.codex"
  cat > "$ARTIFACT_DIR/doctor-home/.codex/config.toml" <<EOF
model_provider = "codexferry"

[model_providers.codexferry]
base_url = "http://127.0.0.1:$ROUTER_PORT/v1"
wire_api = "responses"

[model_providers.codexferry.auth]
command = "echo"
args = ["dummy"]
EOF

  assert_doctor_offline_passes "$cfg" Dynamic
  # No pin -> the L2.7' pin-shadow WARN must NOT fire in dynamic mode.
  ! grep -qF 'pin shadows live fetch' <<<"$DOCTOR_LAST_OUTPUT" \
    || fail "dynamic wiring must not warn about a pin shadowing the live fetch"$'\n'"$DOCTOR_LAST_OUTPUT"
  grep -qF 'PASS: models endpoint reachable' <<<"$DOCTOR_LAST_OUTPUT" \
    || fail "doctor missing PASS: models endpoint reachable"$'\n'"$DOCTOR_LAST_OUTPUT"
  grep -qF 'PASS: models endpoint shape' <<<"$DOCTOR_LAST_OUTPUT" \
    || fail "doctor missing PASS: models endpoint shape"$'\n'"$DOCTOR_LAST_OUTPUT"
  grep -qF 'INFO: codex version age' <<<"$DOCTOR_LAST_OUTPUT" \
    || fail "doctor missing INFO: codex version age"$'\n'"$DOCTOR_LAST_OUTPUT"
  cleanup_procs
  pass "doctor_dynamic"
}

# Doctor offline quick-checks under PINNED codex wiring: env_key + a
# model_catalog_json pin generated from the SAME temp router config (same
# gen-catalog pattern as scenario_static). The report must detect Pinned,
# confirm the static catalog pin, and PASS the pin reconciliation + entry
# shape checks (L2.7-L2.10).
scenario_doctor_pinned() {
  log "scenario: doctor offline under pinned catalog"
  local cfg="$ARTIFACT_DIR/config-pinned.toml"
  start_mock basic "$ARTIFACT_DIR/record-doctor-pinned.jsonl"
  write_router_config "$cfg" "$(free_port)" "$MOCK_PORT"
  start_router "$cfg"

  "$REPO_ROOT/target/debug/codexferry" gen-catalog \
    --config "$cfg" \
    --out    "$ARTIFACT_DIR/catalog.json" >/dev/null \
    || fail "gen-catalog failed"

  mkdir -p "$ARTIFACT_DIR/doctor-home/.codex"
  cat > "$ARTIFACT_DIR/doctor-home/.codex/config.toml" <<EOF
model_provider = "codexferry"
model_catalog_json = "$ARTIFACT_DIR/catalog.json"

[model_providers.codexferry]
base_url = "http://127.0.0.1:$ROUTER_PORT/v1"
wire_api = "responses"
env_key = "E2E_DUMMY_KEY"
EOF

  assert_doctor_offline_passes "$cfg" Pinned
  grep -qF 'INFO: static catalog pin' <<<"$DOCTOR_LAST_OUTPUT" \
    || fail "doctor missing INFO: static catalog pin"$'\n'"$DOCTOR_LAST_OUTPUT"
  for name in 'pin exists and parses' 'pin covers router' 'pin matches router' 'pin entry shape'; do
    grep -qF "PASS: $name" <<<"$DOCTOR_LAST_OUTPUT" \
      || fail "doctor missing PASS: $name"$'\n'"$DOCTOR_LAST_OUTPUT"
  done
  cleanup_procs
  pass "doctor_pinned"
}

# Doctor offline quick-checks under FALLBACK codex wiring: env_key only, no
# model_catalog_json pin and no auth.command. The report must detect
# Fallback and WARN about the degraded wiring (L2.7''), without any FAIL.
scenario_doctor_fallback() {
  log "scenario: doctor offline under fallback wiring"
  local cfg="$ARTIFACT_DIR/config-doctor-fallback.toml"
  start_mock basic "$ARTIFACT_DIR/record-doctor-fallback.jsonl"
  write_router_config "$cfg" "$(free_port)" "$MOCK_PORT"
  start_router "$cfg"

  mkdir -p "$ARTIFACT_DIR/doctor-home/.codex"
  cat > "$ARTIFACT_DIR/doctor-home/.codex/config.toml" <<EOF
model_provider = "codexferry"

[model_providers.codexferry]
base_url = "http://127.0.0.1:$ROUTER_PORT/v1"
wire_api = "responses"
env_key = "E2E_DUMMY_KEY"
EOF

  assert_doctor_offline_passes "$cfg" Fallback
  grep -qF 'WARN: fallback wiring' <<<"$DOCTOR_LAST_OUTPUT" \
    || fail "doctor missing WARN: fallback wiring"$'\n'"$DOCTOR_LAST_OUTPUT"
  cleanup_procs
  pass "doctor_fallback"
}

cargo build --quiet --bin codexferry --bin e2e-mock || fail "build failed"
command -v codex >/dev/null || fail "codex CLI not on PATH"
command -v python3 >/dev/null || fail "python3 required"
command -v curl >/dev/null || fail "curl required"

want="${1:-all}"
case "$want" in
  basic|models|static|tools|multiturn|cross_format_switch|stale_catalog|doctor_dynamic|doctor_pinned|doctor_fallback|all) ;;
  *) fail "unknown scenario: $want (basic|models|static|tools|multiturn|cross_format_switch|stale_catalog|doctor_dynamic|doctor_pinned|doctor_fallback|all)" ;;
esac
case "$want" in
  basic) scenario_basic ;;
  models) scenario_models ;;
  static) scenario_static ;;
  tools) scenario_tools ;;
  multiturn) scenario_multiturn ;;
  cross_format_switch) scenario_cross_format_switch ;;
  stale_catalog) scenario_stale_catalog ;;
  doctor_dynamic) scenario_doctor_dynamic ;;
  doctor_pinned) scenario_doctor_pinned ;;
  doctor_fallback) scenario_doctor_fallback ;;
  all) scenario_basic; scenario_models; scenario_static; scenario_tools; scenario_multiturn; scenario_cross_format_switch; scenario_stale_catalog; scenario_doctor_dynamic; scenario_doctor_pinned; scenario_doctor_fallback ;;
esac
log "all requested scenarios passed"
