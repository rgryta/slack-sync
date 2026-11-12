//! The ingester process: one Slack account in, rows out.
//!
//! Its own binary in its own package. The MCP binary lives in the root package and does
//! not depend on `slack-client`, which is what makes it unable to reach Slack; a second
//! `[[bin]]` there would have given the whole package that dependency.
//!
//! No argument parser: there are no arguments. Everything comes from `SLACK_*`.

use std::process::ExitCode;

use slack_core::config::IngestSettings;

const USAGE: &str = "\
usage: slack-ingest

Runs the poll and backfill loops for one Slack account until stopped.

Configuration comes from SLACK_ACCOUNT, SLACK_XOXC, SLACK_XOXD, SLACK_DSN,
SLACK_SCOPE_CHANNELS, SLACK_POLL_INTERVAL, SLACK_POLL_DORMANT_DAYS,
SLACK_POLL_DORMANT_EVERY, SLACK_POLL_BOOTSTRAP_DAYS, SLACK_REACTION_WINDOW,
SLACK_RATE_LIMIT_RPS, SLACK_RATE_LIMIT_BURST, SLACK_BACKFILL_ENABLED and
SLACK_LOG_LEVEL.
";

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => {}
        [flag] if flag == "--help" || flag == "-h" => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    }

    let settings = match IngestSettings::from_env() {
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

    match slack_ingest::run(&settings).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "the ingester stopped");
            ExitCode::FAILURE
        }
    }
}
