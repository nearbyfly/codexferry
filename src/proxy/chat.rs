//! Chat-format path: Responses request -> Chat Completions -> Responses response.
//! Extracted from  (module-split spec Phase 2).

use super::capture::{input_items, save_session, store_enabled};
use super::upstream::{
    body_read_failure, record_upstream_failure, send_upstream, upstream_non_2xx,
};
use super::*;

/// Convert a Responses request to Chat Completions, forward it upstream, and
/// convert the response back — streaming or non-streaming to match the
/// client's `stream` flag.
///
/// Streaming: spawns a task that walks the upstream SSE stream with
/// [`parse_sse_stream`], feeds each chunk through a [`StreamConverter`], and
/// forwards the produced Responses SSE events over an `mpsc` channel wrapped
/// in `ReceiverStream`. On success the full conversation (history + input +
/// accumulated output) is saved to the session store; if the upstream ends
/// before a `response.completed` is seen (`saw_completed`), the failure is
/// surfaced via `converter.on_error` and no truncated session is persisted.
/// The per-request log line is emitted from the spawned task once the stream
/// finishes, reporting the real token counts.
///
/// Non-streaming: reads the whole upstream body (bytes first, so it can be
/// traced), converts it in one shot via `chat_response_to_items`, saves the
/// session, and returns a complete Responses JSON object.
///
/// Returns `(Response, input_tokens, output_tokens)`.
pub(super) async fn handle_chat_format(
    state: &AppState,
    req: &ResponsesRequest,
    history: &[Value],
    route: &crate::config::ValidatedRoute,
    api_key: &str,
    upstream_model: &str,
) -> (Response, u32, u32) {
    // Convert Responses request → Chat Completions (spec §7.1); history items
    // are merged in as prior messages (incl. reasoning → reasoning_content).
    // Quirk gates read once per request (hot-reload swaps the whole config,
    // so the next request sees any [quirks] change). `missing_done` is folded
    // into the same read (issue #15 item 3: one acquisition, not two).
    let (glm_thinking, heal, missing_done_quirk) = {
        let config = state.config.read().await;
        (
            config.quirk_enabled("glm_thinking"),
            crate::heal::HealGates {
                dsml: config.quirk_enabled("dsml_heal"),
                think: config.quirk_enabled("think_tags"),
                merge_fragmented: false,
            },
            config.quirk_enabled("missing_done"),
        )
    };
    let (chat_req, ns_map) =
        to_chat_request_with_ns_map(req, history, upstream_model, glm_thinking);
    // Upstream endpoint + per-provider request timeout.
    let url = chat_url(&route.provider.base_url);

    let timeout = Duration::from_millis(route.provider.timeout_ms);
    // Provider escape hatches (chat path only): strip named fields, then
    // merge extras — extra wins over the converted body on collision.
    let mut chat_body =
        serde_json::to_value(&chat_req).expect("ChatRequest serialization cannot fail");
    if let Some(obj) = chat_body.as_object_mut() {
        if let Some(drop_params) = &route.provider.drop_params {
            for key in drop_params {
                obj.remove(key);
            }
        }
        if let Some(extra) = &route.provider.extra_params {
            for (key, value) in extra {
                obj.insert(key.clone(), value.clone());
            }
        }
    }
    let chat_req_json =
        serde_json::to_vec(&chat_body).expect("ChatRequest serialization cannot fail");
    // Log the outbound body when CODEXFERRY_TRACE_BODY=1.
    trace_body("upstream request", &chat_req_json);
    let in_flight = InFlightGuard::new(state.metrics.clone(), &route.provider_name, &req.model);
    let upstream_started = std::time::Instant::now();
    // Send upstream; transport failures already map to a 502 error response.
    // Streaming requests only bound the header phase here (issue #14): their
    // body is governed by the idle timeout in the stream loop below.
    let upstream_resp = match send_upstream(
        &state.client,
        &url,
        chat_req_json,
        api_key,
        route,
        timeout,
        req.stream,
    )
    .await
    {
        Ok(r) => r,
        Err((resp, error_class)) => {
            record_upstream_failure(
                &state.metrics,
                &route.provider_name,
                &req.model,
                &route.model,
                error_class,
            );
            return (resp, 0, 0);
        }
    };

    // Non-2xx upstream: pass the status through with the upstream body as the
    // error message (spec §10). The body read is bounded explicitly: for
    // streaming requests no reqwest total deadline covers it (issue #14).
    if !upstream_resp.status().is_success() {
        return (
            upstream_non_2xx(
                &state.metrics,
                &route.provider_name,
                &req.model,
                &route.model,
                upstream_started,
                timeout,
                upstream_resp,
                "chat path",
            )
            .await,
            0,
            0,
        );
    }

    if req.stream {
        // Streaming path: answer the request immediately with an SSE stream,
        // then convert upstream chunks in the background.
        // Generate a fresh proxy-side response ID (spec §8.4) that is returned
        // to the client and keys this turn's session entry.
        let response_id = state.sessions.new_response_id();
        let route_key = req.model.clone();
        let model_display = req.model.clone();
        let upstream_log = upstream_model.to_string();
        let sessions = state.sessions.clone();
        let store_enabled = store_enabled(req);
        // Skip cloning the turn's context when it will never be stored.
        let history_owned = if store_enabled {
            history.to_vec()
        } else {
            Vec::new()
        };
        let input_items = if store_enabled {
            input_items(req)
        } else {
            Vec::new()
        };

        // mpsc channel (64-event buffer) carries converted SSE events from the
        // spawned task to the ReceiverStream that becomes the HTTP response.
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(64);

        // Everything downstream of the upstream response runs in a detached
        // task: it pushes events into the channel and, at the end, persists the
        // session and emits the per-request log line.
        let metrics = state.metrics.clone();
        let upstream_start = upstream_started;
        let provider_name = route.provider_name.clone();
        // Idle timeout for the upstream body (issue #14): no chunk within
        // `timeout_ms` means the upstream stalled — fail the stream. This is
        // the ONLY body-level deadline for streaming requests (there is no
        // total-duration cap), so healthy streams may run longer.
        let idle_timeout = timeout;

        tokio::spawn(async move {
            let _in_flight = in_flight;
            let started_in_task = std::time::Instant::now();
            // ns_map is moved into the task so tool-call deltas can be
            // decoded back to namespaced names (spec §7).
            // Stateful per-request converter: accumulates output items and
            // maps each upstream chunk to Responses SSE events.
            let mut converter =
                StreamConverter::new(response_id.clone(), model_display.clone(), heal, ns_map);

            // Hand-written SSE parser over the upstream byte stream (spec §7.3).
            let mut sse_stream = Box::pin(parse_sse_stream(upstream_resp.bytes_stream()));
            let mut saw_completed = false;
            // Whether the upstream sent the [DONE] sentinel; a spec-
            // compliant provider always does. The missing_done quirk
            // decides what an ended stream *without* it means.
            let mut saw_done = false;
            // Whether the loop ended because the client hung up (a send
            // failed) rather than because the upstream stream ended. The
            // missing_done anomaly warns must not attribute a client-
            // initiated end to an upstream anomaly, so they are suppressed
            // when this is set.
            let mut client_disconnected = false;
            // Whether the loop ended because the idle timeout fired (a
            // proxy-initiated stop, NOT an upstream completion): the
            // missing_done quirk must not rescue it, metrics classify it as
            // a timeout, and no session is persisted.
            let mut timed_out = false;
            let mut ttft_recorded = false;

            // Open the client-facing stream immediately (spec §7.2): at
            // high/max reasoning effort the upstream's first chunk can lag
            // by tens of seconds, and without an early `response.created`
            // the client sees a silent stream that looks hung.
            if let Some((event_type, data)) = converter.start() {
                let sse_event = Event::default().event(event_type).data(data);
                if tx.send(Ok(sse_event)).await.is_err() {
                    // Client already gone; the chunk loop's first send takes
                    // the normal disconnect path (post-loop logging, cleanup).
                    client_disconnected = true;
                }
            }

            'outer: loop {
                let timed = tokio::time::timeout(idle_timeout, sse_stream.next()).await;
                let event = match timed {
                    Ok(Some(event)) => event,
                    Ok(None) => break,
                    Err(_) => {
                        tracing::warn!(
                            idle_timeout_ms = %idle_timeout.as_millis() as u64,
                            "chat stream idle timeout: no upstream chunk in time"
                        );
                        timed_out = true;
                        break;
                    }
                };
                // Upstream sent the [DONE] sentinel: the stream is complete.
                if is_done(&event.data) {
                    saw_done = true;
                    break;
                }
                // Each payload is a Chat Completions stream chunk; skip chunks
                // that fail to parse (with a warning) rather than aborting.
                let chunk: ChatStreamChunk = match serde_json::from_str(&event.data) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!("failed to parse SSE chunk: {e}");
                        continue;
                    }
                };
                // Convert the chunk into one or more Responses SSE events.
                let events = converter.on_chunk(&chunk);
                for (event_type, data) in events {
                    // Track time-to-first-token (TTFT) on the first content
                    // event - text, reasoning, or tool-call arguments.
                    if !ttft_recorded && is_first_content_event(&event_type) {
                        ttft_recorded = true;
                        metrics.observe_ttft(
                            &provider_name,
                            &route_key,
                            &upstream_log,
                            upstream_start.elapsed().as_secs_f64(),
                        );
                    }
                    // Track whether the upstream produced a completed response,
                    // so a premature end can be distinguished from success.
                    if event_type == "response.completed" {
                        saw_completed = true;
                    }
                    let sse_event = Event::default().event(event_type).data(data);
                    if tx.send(Ok(sse_event)).await.is_err() {
                        // Client disconnected: stop draining the upstream response.
                        client_disconnected = true;
                        break 'outer;
                    }
                }
            }

            // Upstream stream ended: emit the deferred finish sequence (done
            // events + response.completed). It is deferred to stream end so
            // the usage-only trailing chunk (include_usage sends it AFTER the
            // finish_reason chunk) is reflected in response.completed's usage.
            //
            // Quirk `missing_done`: a stream that ended without the [DONE]
            // sentinel is complete only when a chunk carried finish_reason
            // (some providers just drop the sentinel) — the quirk rescues
            // exactly that sub-case. Disabled, or enabled but truncated (no
            // finish_reason seen), the strict rule holds: only [DONE]
            // authorizes completion, and anything else falls through to the
            // on_error path below. The quirk never rescues a proxy-initiated
            // idle timeout (issue #14 / review #2): the stream was cut short
            // by the router, not completed by the upstream.
            let finish_allowed = saw_done
                || (!timed_out && missing_done_quirk && converter.finish_reason().is_some());
            // NOTE (AGENTS.md #11): the warns below are anomaly-only
            // telemetry — they fire only on genuine upstream stream ends
            // without the [DONE] sentinel — NOT per-request log lines, so
            // the single per-request `tracing::info!` at the end of this
            // task is still the only per-request line. Both are suppressed
            // when the client disconnected: that end is client-initiated,
            // not an upstream anomaly, so no warn is warranted. An idle
            // timeout already logged its own warn above.
            if !saw_done && !client_disconnected && !timed_out {
                if finish_allowed {
                    // The missing_done quirk rescued the stream: it ended
                    // without [DONE], but a chunk carried finish_reason, so
                    // it is treated as complete (emitted below). Only this
                    // finish_reason-present sub-case warrants the "fired"
                    // warn; a truncated stream (no finish_reason) falls
                    // through to the on_error path below.
                    if let Some(reason) = converter.finish_reason() {
                        tracing::warn!(
                            "quirk missing_done fired: stream ended without [DONE] (finish_reason={reason}) — treating as complete"
                        );
                    }
                }
                if converter.finish_reason().is_none() && !converter.acc.text.is_empty() {
                    // No [DONE] and no finish_reason, but the model had
                    // produced text: the turn was truncated. Anomalous
                    // regardless of the quirk setting, so this warn fires
                    // even when the quirk is disabled; the on_error path
                    // below still surfaces response.failed and never
                    // persists.
                    tracing::warn!(
                        "stream ended without [DONE] and without finish_reason — turn was truncated, discarding"
                    );
                }
            }
            for (event_type, data) in if finish_allowed {
                converter.finish()
            } else {
                Vec::new()
            } {
                if event_type == "response.completed" {
                    saw_completed = true;
                }
                let sse_event = Event::default().event(event_type).data(data);
                if tx.send(Ok(sse_event)).await.is_err() {
                    client_disconnected = true;
                    break;
                }
            }

            // Record metrics at stream end (spec §3-4).
            if let Some(error_class) =
                stream_metrics_error_class(saw_completed, client_disconnected, timed_out)
            {
                let elapsed = upstream_start.elapsed().as_secs_f64();
                metrics.observe_duration(&provider_name, &route_key, &upstream_log, elapsed);
                metrics.record_request(&provider_name, &route_key, &upstream_log, error_class);

                // Token counters — only available on successful completion
                if saw_completed {
                    if let Some(usage) = &converter.acc.usage {
                        metrics.record_tokens(
                            &provider_name,
                            &route_key,
                            &upstream_log,
                            usage.prompt_tokens,
                            usage.completion_tokens,
                        );
                    }
                }
            }

            // Per-request completion line (spec §11): real token counts are
            // only known once the stream has finished. Extract them BEFORE
            // moving the accumulated items into session storage (issue #15
            // item 3: borrow-then-move, no clone).
            let (in_tok, out_tok) = converter
                .acc
                .usage
                .as_ref()
                .map(|u| (u.prompt_tokens, u.completion_tokens))
                .unwrap_or((0, 0));

            if !saw_completed {
                // Upstream ended before a response.completed event: surface the
                // failure to the client (error + response.failed events) and do
                // not persist a truncated session.
                for (event_type, data) in
                    converter.on_error("upstream stream ended before completion")
                {
                    let sse_event = Event::default().event(event_type).data(data);
                    if tx.send(Ok(sse_event)).await.is_err() {
                        break;
                    }
                }
            } else if store_enabled {
                // Store session: full context = history + input items + output items
                save_session(
                    &sessions,
                    response_id,
                    &history_owned,
                    input_items,
                    converter.acc.items,
                )
                .await;
            }

            let stream_status: u16 = if saw_completed { 200 } else { 500 };
            tracing::info!(
                route = %route_key,
                upstream = %upstream_log,
                model = %model_display,
                status = stream_status,
                input_tokens = in_tok,
                output_tokens = out_tok,
                elapsed_ms = started_in_task.elapsed().as_millis() as u64,
                "stream completed"
            );
        });

        let stream = ReceiverStream::new(rx);
        (Sse::new(stream).into_response(), 0, 0)
    } else {
        // Non-streaming path: read the entire upstream body, then convert.
        // Bytes are read first (not `upstream_resp.json()`) so the raw body
        // can be traced and parsing is deferred.
        let upstream_status = upstream_resp.status();
        let resp_bytes = match upstream_resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                return (
                    body_read_failure(
                        &e,
                        &state.metrics,
                        &route.provider_name,
                        &req.model,
                        &route.model,
                    ),
                    0,
                    0,
                );
            }
        };
        // Log the response body when CODEXFERRY_TRACE_BODY=1, then parse.
        trace_body("upstream response", &resp_bytes);
        let mut chat_resp: ChatResponse = match serde_json::from_slice(&resp_bytes) {
            Ok(r) => r,
            Err(e) => {
                state.metrics.record_request(
                    &route.provider_name,
                    &req.model,
                    &route.model,
                    crate::metrics::ErrorClass::from_status(upstream_status.as_u16()),
                );
                return (
                    error_response(
                        StatusCode::BAD_GATEWAY,
                        "server_error",
                        &format!("failed to parse upstream response: {e}"),
                    ),
                    0,
                    0,
                );
            }
        };

        // Record metrics (spec §3-4): for non-streaming, TTFT and total
        // duration are the same value (the entire response arrived at once).
        let elapsed = upstream_started.elapsed().as_secs_f64();
        state
            .metrics
            .observe_ttft(&route.provider_name, &req.model, &route.model, elapsed);
        state
            .metrics
            .observe_duration(&route.provider_name, &req.model, &route.model, elapsed);
        state.metrics.record_request(
            &route.provider_name,
            &req.model,
            &route.model,
            crate::metrics::ErrorClass::Empty,
        );
        if let Some(usage) = &chat_resp.usage {
            state.metrics.record_tokens(
                &route.provider_name,
                &req.model,
                &route.model,
                usage.prompt_tokens,
                usage.completion_tokens,
            );
        }

        // Response healing (quirks dsml_heal + think_tags): repair the
        // assistant message in place before conversion - same order as the
        // streaming pipeline (DSML first, then think).
        if let Some(choice) = chat_resp.choices.first_mut() {
            if heal.dsml {
                crate::heal::heal_dsml_chat_message(&mut choice.message);
            }
            if heal.think {
                crate::heal::heal_think_chat_message(&mut choice.message);
            }
        }

        // Convert the complete Chat response into Responses-format items
        // (spec §7.2: reasoning → reasoning item, tool_calls → function_call).
        let items = chat_response_to_items(&chat_resp, &ns_map);
        let response_id = state.sessions.new_response_id();

        // Assemble the final Responses JSON object and extract token counts
        // from upstream usage for the per-request log line.
        let response =
            build_completed_response(&response_id, &req.model, &items, chat_resp.usage.as_ref());

        // Store session: full context = history + new input items + output
        // items. Runs after `build_completed_response` so `items` is borrowed
        // first and then MOVED (issue #15 item 3: no clone).
        if store_enabled(req) {
            save_session(
                &state.sessions,
                response_id.clone(),
                history,
                input_items(req),
                items,
            )
            .await;
        }
        let (input, output) = chat_resp
            .usage
            .as_ref()
            .map(|u| (u.prompt_tokens, u.completion_tokens))
            .unwrap_or((0, 0));
        (Json(response).into_response(), input, output)
    }
}
