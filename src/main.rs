//! CLI entry point for `codexferry`.
//!
//! This crate is a local proxy daemon that lets Codex CLI use
//! Chat-Completions-only LLM providers (DeepSeek, Kimi, GLM, SiliconFlow, …)
//! through a single Responses-API endpoint. The binary has three modes of
//! operation, selected by clap from the command line:
//!
//! * **Default (no subcommand)** — run the proxy server: load and validate
//!   the TOML config, spawn the hot-reload watcher, and call [`proxy::run`]
//!   to serve the axum HTTP endpoints (`POST /v1/responses`,
//!   `GET /v1/models`, `GET /healthz`, …).
//! * **`gen-catalog` subcommand** — offline tool: read the config and emit a
//!   Codex `model_catalog_json` file (see [`catalog::run_gen_catalog`]) so
//!   the Codex TUI knows about the proxy's `provider/alias` models.
//! * **`doctor` subcommand** — contract health check (upgrade tripwire):
//!   offline it regenerates the catalog in memory and deep-compares it with
//!   the installed one, reporting drift as FAIL; `--live` additionally
//!   drives the installed Codex CLI through a temporary router (see
//!   [`doctor::run_doctor`]).
//!
//! Logging is initialized lazily inside [`proxy::run`] (see `logging.rs`).
//! The `gen-catalog` path also installs a tracing subscriber — Once-guarded
//! in `catalog::run_gen_catalog`, so its log output IS visible on stderr;
//! the catalog JSON itself is written to the `--out` file.
//!
//! The module tree is split by responsibility: `config` (TOML types +
//! validation + hot reload), `proxy` (axum handlers + streaming),
//! `session` (in-memory conversation store), `upstream` (SSE parsing + API
//! key resolution), `catalog` (model catalog generation), `doctor` (offline
//! + live contract health checks), and the `wire` / `convert` modules
//!   (protocol types and Responses ↔ Chat translation).

mod catalog;
mod config;
mod convert;
mod doctor;
mod doctor_live;
mod heal;
mod logging;
mod metrics;
mod mode;
mod models_cache;
mod normalize;
mod proxy;
mod quirks;
mod session;
mod upstream;
mod version;
mod wire;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Top-level command-line interface for the `codexferry` binary.
///
/// Parsed by clap's derive macro. When no subcommand is given, `command` is
/// `None` and the proxy server is started; otherwise the requested
/// subcommands (`gen-catalog`, `doctor`) is dispatched to (see [`Commands`]).
#[derive(Parser)]
#[command(
    name = "codexferry",
    version,
    about = "Local proxy: Responses API ↔ Chat Completions, multi-provider routing"
)]
struct Cli {
    /// Optional subcommand. `None` (the default) starts the proxy server;
    /// `Some(...)` runs the requested offline tool instead.
    #[command(subcommand)]
    command: Option<Commands>,
}

/// All subcommands for the `codexferry` CLI.
///
/// Currently has two variants: `gen-catalog`, which generates a Codex model
/// catalog JSON file, and `doctor`, which checks router ↔ Codex contract
/// health. New offline tools (e.g. a `check-config` linter) would be added
/// here.
#[derive(Subcommand)]
enum Commands {
    /// Generate Codex model catalog JSON.
    ///
    /// Reads the router config, resolves the configured routes to catalog
    /// entries, and writes a `{"models": [...]}` file that Codex CLI can
    /// consume via `model_catalog_json` in `~/.codex/config.toml`. See
    /// `catalog::run_gen_catalog` for the full algorithm.
    GenCatalog {
        /// Output path for the generated catalog JSON
        /// (e.g. `~/.codex/codexferry-catalog.json`).
        #[arg(long)]
        out: PathBuf,
        /// Path to the router TOML config file (same format the server
        /// loads from `CODEXFERRY_CONFIG`).
        #[arg(long)]
        config: PathBuf,
        /// Optional explicit path to a Codex `models.json` template to
        /// inherit version-sensitive fields from; when omitted, the template
        /// is located automatically (see `catalog::load_template`).
        #[arg(long)]
        codex_models: Option<PathBuf>,
    },

    /// Check router ↔ Codex contract health (upgrade tripwire).
    ///
    /// Offline mode (default) verifies the installed model catalog matches a
    /// fresh regeneration. `--live` additionally drives the installed Codex
    /// CLI through a temporary in-process router + mock upstream and asserts
    /// the normalized wire shape and a full tool round-trip.
    Doctor {
        /// Path to the router TOML config (defaults to CODEXFERRY_CONFIG
        /// or ./cxf.toml, same rule as the server).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Path to the installed catalog JSON
        /// (defaults to ~/.codex/codexferry-catalog.json).
        #[arg(long)]
        catalog: Option<PathBuf>,
        /// Optional explicit path to a Codex `models.json` template to
        /// inherit version-sensitive fields from (same meaning as in
        /// `gen-catalog`); when omitted, the template is located
        /// automatically (see `catalog::load_template`).
        #[arg(long)]
        codex_models: Option<PathBuf>,
        /// Run the live wire-shape + tool round-trip probe.
        #[arg(long)]
        live: bool,
    },
}

/// Program entry point.
///
/// Parses the CLI arguments and dispatches:
/// * `Some(Commands::GenCatalog { .. })` → offline catalog generation
///   (`catalog::run_gen_catalog`), then exit.
/// * `Some(Commands::Doctor { .. })` → offline/live contract health check
///   (`doctor::run_doctor`), then exit (exit code 1 on any FAIL).
/// * `None` → the long-running proxy server (`proxy::run`), which blocks
///   until a shutdown signal (SIGINT/SIGTERM) is received.
///
/// Both paths propagate fatal errors as `anyhow::Result`, which Rust prints
/// with a descriptive message and turns into a non-zero exit code.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Commands::GenCatalog {
            out,
            config,
            codex_models,
        }) => {
            // Offline mode: no network listener, just write the catalog file.
            catalog::run_gen_catalog(&config, &out, codex_models.as_deref())?;
        }
        Some(Commands::Doctor {
            config,
            catalog,
            codex_models,
            live,
        }) => {
            let config_path = config.unwrap_or_else(crate::config::default_config_path);
            doctor::run_doctor(
                &config_path,
                catalog.as_deref(),
                codex_models.as_deref(),
                live,
            )?;
        }
        None => {
            // Server mode: init logging, load config, spawn watcher, serve.
            proxy::run().await?;
        }
    }
    Ok(())
}
