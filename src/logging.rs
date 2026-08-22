//! Tracing/logging initialization.
//!
//! The whole crate logs through the `tracing` facade; this module installs
//! the subscriber once at process start (from `proxy::run`). Output is
//! human-readable formatted lines on stdout via `tracing_subscriber::fmt`.
//!
//! ## Log level: `RUST_LOG`
//!
//! The level is controlled by the `RUST_LOG` environment variable, parsed
//! with `tracing_subscriber::EnvFilter`. When `RUST_LOG` is unset (or
//! invalid), the filter falls back to `codex_router=info` — i.e. info-level
//! logs from this crate only, hiding the noisy debug output of dependencies
//! (hyper, reqwest, tower, …).
//!
//! Example invocations:
//!
//! * `RUST_LOG=debug codex-router` — debug level for every crate, including
//!   dependencies; the most useful for diagnosing request flow.
//! * `RUST_LOG=codex_router=trace codex-router` — trace level only for this
//!   crate's own modules (session hits/misses, streaming conversion, …).
//! * `RUST_LOG=error codex-router` — only errors, regardless of crate.
//!
//! ## Body tracing: `CODEX_ROUTER_TRACE_BODY`
//!
//! `CODEX_ROUTER_TRACE_BODY=1` is a separate opt-in switch checked in
//! `proxy.rs` (spec §11): with it set (and the level at `debug`), the proxy
//! logs raw request/response bodies via `proxy::trace_body`. It is
//! independent of `RUST_LOG` because bodies are only dumped when the
//! variable is explicitly set — intended for local debugging only.

pub fn init() {
    // Use the environment's RUST_LOG if present, else our crate-scoped default.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("codex_router=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
