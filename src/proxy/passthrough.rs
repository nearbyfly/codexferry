//! Responses-format passthrough path: relay upstream Responses SSE verbatim,
//! healing event-granular when quirks fire; session keyed by upstream id.
//! Extracted from  (module-split spec Phase 2).

use super::capture::{completed_capture, last_completed_payload, trim_completed_prefix};
use super::upstream::{
    body_read_failure, record_upstream_failure, send_upstream, upstream_non_2xx,
};
use super::*;

/// Forward a request to a native Responses upstream verbatim (no protocol
/// conversion), replacing the model name, merging session history into
/// `input`, and stripping `previous_response_id` (consumed, never forwarded).
///
/// Streaming: the upstream byte stream is relayed as the HTTP response body
/// via `Body::from_stream`. With both healing quirks off this is a verbatim
/// byte passthrough preserving the original SSE event types; with `dsml_heal`
/// or `think_tags` on, the stream is split into structure-preserving events
/// and rewritten event-granular ([`ResponsesStreamHealer`]). Session capture
/// is best-effort: the forwarded bytes are accumulated and the final
/// `response.completed` event is decoded once at stream end
/// ([`completed_capture`]).
///
/// Session keying (AGENTS.md #4): because the stream is passed through, the
/// client only ever sees the upstream's response id (healing rewrites events
/// in place, never re-keys), so the session is keyed by that id — a
/// proxy-generated fallback key would never be hit by the next
/// `previous_response_id`.
///
/// Non-streaming: the full JSON body is forwarded (healed in place when a
/// healing quirk is on), with id/output/usage extracted for session capture
/// and token logging.
pub(super) async fn handle_responses_format(
    state: &AppState,
    req: &ResponsesRequest,
    history: &[Value],
    route: &crate::config::ValidatedRoute,
    api_key: &str,
    upstream_model: &str,
) -> (Response, u32, u32) {
    // Healing gates read once per request (same pattern as the chat path;
    // [quirks] is hot-reloaded, so the next request sees any change).
    let heal = {
        let config = state.config.read().await;
        crate::heal::HealGates {
            dsml: config.quirk_enabled("dsml_heal"),
            think: config.quirk_enabled("think_tags"),
        }
    };
    let url = responses_url(&route.provider.base_url);
    let timeout = Duration::from_millis(route.provider.timeout_ms);

    // Build the upstream request: replace model name, merge history, drop previous_response_id.
    let mut req_body =
        serde_json::to_value(req).expect("ResponsesRequest serialization cannot fail");
    if let Some(obj) = req_body.as_object_mut() {
        // Drop null-valued optional fields for a cleaner upstream body.
        obj.retain(|_, v| !v.is_null());
        obj.insert("model".into(), Value::String(upstream_model.into()));
        if !history.is_empty() {
            let mut full_input = history.to_vec();
            full_input.extend(input_items(req));
            obj.insert("input".into(), Value::Array(full_input));
        }
        // Boundary normalization (always on): hoist Codex's dialect tool
        // deliveries into the public shape and surface unknown input item
        // types. Runs AFTER the history merge so replayed items are covered.
        crate::normalize::normalize_responses_request(obj);
        obj.remove("previous_response_id");
    }

    let in_flight = InFlightGuard::new(state.metrics.clone(), &route.provider_name, &req.model);
    let upstream_started = std::time::Instant::now();
    let req_json = serde_json::to_vec(&req_body).expect("serialize upstream body");
    // Send upstream; transport failures map to a 502 error response.
    // Streaming requests only bound the header phase here (issue #14).
    let upstream_resp = match send_upstream(
        &state.client,
        &url,
        req_json,
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

    // Non-2xx upstream: pass the status and body through as the error. The
    // body read is bounded explicitly: for streaming requests no reqwest
    // total deadline covers it (issue #14).
    let status = upstream_resp.status();
    if !status.is_success() {
        return (
            upstream_non_2xx(
                &state.metrics,
                &route.provider_name,
                &req.model,
                &route.model,
                upstream_started,
                timeout,
                upstream_resp,
                "passthrough path",
            )
            .await,
            0,
            0,
        );
    }

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

    if req.stream {
        // Byte passthrough (verbatim when both healing quirks are off;
        // event-granular healing when either is on) + best-effort session
        // capture:
        // a spawned task relays chunks through an mpsc channel into the
        // response body while accumulating the forwarded bytes for one-shot
        // parsing of the final response.completed event.
        let route_key = req.model.clone();
        let model_display = req.model.clone();
        let upstream_log = upstream_model.to_string();
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, Infallible>>(64);
        let metrics = state.metrics.clone();
        let upstream_start = upstream_started;
        let provider_name = route.provider_name.clone();
        // Idle timeout for the upstream body (issue #14) — same semantics as
        // the chat path's loop.
        let idle_timeout = timeout;
        tokio::spawn(async move {
            let _in_flight = in_flight;
            let started_in_task = std::time::Instant::now();
            let mut stream = upstream_resp.bytes_stream();
            // Accumulates the bytes actually forwarded; decoded once at
            // stream end so a UTF-8 character split across chunks is never
            // corrupted. The buffer is trimmed to the first completed-event
            // marker as it streams (issue #15 item 2), so it grows O(tail)
            // rather than O(whole stream).
            let mut raw: Vec<u8> = Vec::new();
            let mut raw_trimmed = false;
            let mut ttft_recorded = false;
            let mut client_disconnected = false;
            // Whether a relay loop ended because the idle timeout fired (a
            // proxy-initiated stop, not an upstream end): metrics classify it
            // as a timeout and a terminal failure event is appended.
            let mut timed_out = false;
            let mut content_carry: Vec<u8> = Vec::new();
            if !heal.dsml && !heal.think {
                // Fast path: both healing gates off — verbatim byte relay
                // (today's behavior, zero added parsing).
                loop {
                    let timed = tokio::time::timeout(idle_timeout, stream.next()).await;
                    let chunk_result = match timed {
                        Ok(Some(r)) => r,
                        Ok(None) => break,
                        Err(_) => {
                            tracing::warn!(
                                idle_timeout_ms = %idle_timeout.as_millis() as u64,
                                "passthrough stream idle timeout: no upstream chunk in time"
                            );
                            timed_out = true;
                            break;
                        }
                    };
                    match chunk_result {
                        Ok(bytes) => {
                            // TTFT: first content marker (text, reasoning, or
                            // tool-call args) in the raw relayed bytes, with a
                            // carry so markers split across chunks still match.
                            if !ttft_recorded
                                && first_content_event_bytes(&bytes, &mut content_carry)
                            {
                                ttft_recorded = true;
                                metrics.observe_ttft(
                                    &provider_name,
                                    &route_key,
                                    &upstream_log,
                                    upstream_start.elapsed().as_secs_f64(),
                                );
                            }
                            if tx.send(Ok(bytes.clone())).await.is_err() {
                                client_disconnected = true;
                                break; // client disconnected
                            }
                            let prev_len = raw.len();
                            raw.extend_from_slice(&bytes);
                            if !raw_trimmed && trim_completed_prefix(&mut raw, prev_len) {
                                raw_trimmed = true;
                            }
                        }
                        Err(e) => {
                            tracing::warn!("passthrough stream error: {e}");
                            break;
                        }
                    }
                }
            } else {
                // Healed relay: event-granular rewrite. `raw` accumulates the
                // bytes actually forwarded, so session capture reflects the
                // healed stream (replay consistency with the chat path). One
                // asymmetry vs the fast path: `split_sse_events` drops a
                // trailing run of pure whitespace with no event, and a text
                // delta ending in a marker-prefix tail is re-encoded with its
                // withheld tail released as a separate delta (the healer
                // no-ops, so the relay is semantically verbatim but not
                // byte-identical).
                let mut healer = crate::heal::ResponsesStreamHealer::new(heal);
                let mut events = Box::pin(crate::upstream::split_sse_events(stream));
                'relay: loop {
                    let timed = tokio::time::timeout(idle_timeout, events.next()).await;
                    let evt = match timed {
                        Ok(Some(e)) => e,
                        Ok(None) => break,
                        Err(_) => {
                            tracing::warn!(
                                idle_timeout_ms = %idle_timeout.as_millis() as u64,
                                "passthrough stream idle timeout: no upstream event in time"
                            );
                            timed_out = true;
                            break;
                        }
                    };
                    if !ttft_recorded {
                        if let Some(ref event_type) = evt.event {
                            if is_first_content_event(event_type) {
                                ttft_recorded = true;
                                metrics.observe_ttft(
                                    &provider_name,
                                    &route_key,
                                    &upstream_log,
                                    upstream_start.elapsed().as_secs_f64(),
                                );
                            }
                        }
                    }
                    for chunk in healer.push_event(&evt.raw, evt.event.as_deref(), &evt.data) {
                        if tx.send(Ok(chunk.clone())).await.is_err() {
                            client_disconnected = true;
                            break 'relay; // client disconnected
                        }
                        let prev_len = raw.len();
                        raw.extend_from_slice(&chunk);
                        if !raw_trimmed && trim_completed_prefix(&mut raw, prev_len) {
                            raw_trimmed = true;
                        }
                    }
                }
                for chunk in healer.finish() {
                    if tx.send(Ok(chunk.clone())).await.is_err() {
                        client_disconnected = true;
                        break;
                    }
                    let prev_len = raw.len();
                    raw.extend_from_slice(&chunk);
                    if !raw_trimmed && trim_completed_prefix(&mut raw, prev_len) {
                        raw_trimmed = true;
                    }
                }
            }
            // Best-effort capture: parse the final response.completed event
            // ONCE (decoded once, no per-chunk UTF-8 corruption) and derive
            // both the session fields and the token counts from that single
            // payload - the whole-buffer scan + JSON parse is too costly to
            // run twice per request. Key the session by the upstream id so
            // the client's previous_response_id hits the store next turn.
            let (upstream_id, all_output, usage) = last_completed_payload(&raw)
                .map(completed_capture)
                .unwrap_or((None, Vec::new(), None));

            // Record metrics at stream end (spec §3-4). Success is judged by
            // the captured upstream id — the SAME predicate `stream_status`
            // below and the non-streaming path use — not by the presence of
            // a usage object: a completed event without token counts is
            // still a completed response (issue #15 item 1). A trailing
            // idle-timeout after an id-less completed event now also
            // surfaces the terminal failure below, consistent with the
            // non-streaming classification of that shape.
            let saw_completed = upstream_id.is_some();
            if let Some(error_class) =
                stream_metrics_error_class(saw_completed, client_disconnected, timed_out)
            {
                let elapsed = upstream_start.elapsed().as_secs_f64();
                metrics.observe_duration(&provider_name, &route_key, &upstream_log, elapsed);
                metrics.record_request(&provider_name, &route_key, &upstream_log, error_class);

                // Record token counts — available from the usage parsed above.
                if let Some((in_tok, out_tok)) = usage {
                    metrics.record_tokens(
                        &provider_name,
                        &route_key,
                        &upstream_log,
                        in_tok,
                        out_tok,
                    );
                }
            }

            // Idle timeout before any completed event: append a terminal
            // failure so the client can distinguish the proxy giving up from
            // the upstream silently dropping the connection (review #3). A
            // send failure here means the client already hung up.
            if timed_out && !saw_completed {
                const IDLE_TIMEOUT_FAILED_EVENT: &str = concat!(
                    "event: response.failed\n",
                    "data: {\"type\":\"response.failed\",\"response\":{\"id\":\"\",",
                    "\"status\":\"failed\",\"error\":{\"code\":\"timeout\",",
                    "\"message\":\"upstream stream idle timeout\"}}}\n\n"
                );
                let _ = tx
                    .send(Ok(Bytes::from_static(IDLE_TIMEOUT_FAILED_EVENT.as_bytes())))
                    .await;
            }

            let stream_status: u16 = if saw_completed {
                200
            } else if timed_out {
                // The proxy gave up waiting; partial output without a
                // completed event is still a failed turn.
                500
            } else if !all_output.is_empty() {
                200
            } else {
                500
            };
            // Only persist when the upstream id is available: passthrough
            // (verbatim when both quirks are off; healed in place when they
            // fire) never re-keys the response, so the client only ever sees
            // upstream ids (a router-generated fallback key would be
            // unreachable on the next turn).
            if let Some(key) = upstream_id {
                if store_enabled {
                    save_session(&sessions, key, &history_owned, input_items, all_output).await;
                }
            } else {
                tracing::debug!(
                    "responses passthrough ended without a completed response id; not caching session"
                );
            }

            // Per-request completion line (spec §11): real token counts are
            // only known once the stream has finished.
            let (in_tok, out_tok) = usage.unwrap_or((0, 0));
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

        // Forward the relayed byte stream as the response body — verbatim
        // when both healing gates are off, otherwise the healed stream.
        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        let body = axum::body::Body::from_stream(stream);
        let mut resp = Response::new(body);
        *resp.status_mut() = status;
        // SSE responses must be declared as text/event-stream.
        resp.headers_mut()
            .insert("content-type", "text/event-stream".parse().unwrap());
        (resp, 0, 0)
    } else {
        // Non-streaming passthrough: read the full upstream JSON body and
        // forward it (healed in place when a healing quirk is on).
        let body = match upstream_resp.bytes().await {
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

        // Response-side healing (quirks dsml_heal + think_tags): rewrite
        // leaked DSML/think markup in place; the healed body is both
        // forwarded and what session capture parses.
        let forward_body: Bytes = if heal.dsml || heal.think {
            crate::heal::heal_responses_body(&body, heal).into()
        } else {
            body.clone()
        };

        // Best-effort session capture + token counts: parse the body once and
        // derive everything through the shared [`completed_capture`] (handles
        // both the `response`-nested OpenAI shape and flat providers).
        // Persist only when the upstream id is present (see streaming note).
        let parsed = serde_json::from_slice::<Value>(&forward_body).ok();
        let (upstream_id, output, usage) =
            parsed
                .map(completed_capture)
                .unwrap_or((None, Vec::new(), None));
        let (input_tokens, output_tokens) = usage.unwrap_or((0, 0));
        let saw_upstream_id = upstream_id.is_some();

        if let Some(key) = upstream_id {
            if store_enabled {
                save_session(&sessions, key, &history_owned, input_items, output).await;
            }
        }

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
            if saw_upstream_id {
                crate::metrics::ErrorClass::Empty
            } else {
                crate::metrics::ErrorClass::StreamTruncated
            },
        );
        if input_tokens > 0 || output_tokens > 0 {
            state.metrics.record_tokens(
                &route.provider_name,
                &req.model,
                &route.model,
                input_tokens,
                output_tokens,
            );
        }

        let mut resp = Response::new(forward_body.into());
        *resp.status_mut() = status;
        // Non-streaming passthrough is a plain JSON body.
        resp.headers_mut()
            .insert("content-type", "application/json".parse().unwrap());
        (resp, input_tokens, output_tokens)
    }
}
