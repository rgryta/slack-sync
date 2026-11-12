//! The MCP process: `slack-sync mcp`, as its own binary.
//!
//! This binary links `slack-core` and `slack-mcp` and nothing else. `slack-client` is
//! not in its dependency graph, so the code that could call Slack is not in the process
//! at all, and `tests/archive/isolation.rs` keeps the manifests honest about it.
//!
//! No argument parser: there are two modes and one of them is a flag. A crate for that
//! is a crate to keep patched for nothing.

use std::process::ExitCode;

use slack_core::config::McpSettings;
use slack_mcp::server;

const USAGE: &str = "\
usage: slack-mcp [--health-check]

  (no arguments)   serve the read-only MCP tools over HTTP
  --health-check   probe this container's own /healthz and exit 0 when it answers

Configuration comes from SLACK_MCP_DSN, SLACK_MCP_HOST, SLACK_MCP_PORT and
SLACK_MCP_LOG_LEVEL.
";

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => {}
        [flag] if flag == "--health-check" => return run_health_check().await,
        [flag] if flag == "--help" || flag == "-h" => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    }

    let settings = match McpSettings::from_env() {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("configuration error: {error}");
            return ExitCode::FAILURE;
        }
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| settings.log_level.to_lowercase().into()),
        )
        .init();

    match server::run(&settings).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "the server stopped");
            ExitCode::FAILURE
        }
    }
}

/// The healthcheck path. Deliberately quiet and dependency-free: it prints one line on
/// failure and nothing at all on success, because that is all a `HEALTHCHECK CMD` reads.
async fn run_health_check() -> ExitCode {
    // `SLACK_MCP_DSN` is not needed to probe a port, and demanding it would make the
    // healthcheck fail for a reason that has nothing to do with health.
    let (host, port) = match slack_core::config::address_from_env() {
        Ok(address) => address,
        Err(error) => {
            eprintln!("configuration error: {error}");
            return ExitCode::FAILURE;
        }
    };
    match server::health_check(&host, port).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
