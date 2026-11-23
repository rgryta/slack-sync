//! Boot and the two long-running loops. One account per process.

use std::collections::HashSet;
use std::time::Duration;

use chrono::Utc;
use slack_client::SlackClient;
use slack_core::config::IngestSettings;
use slack_core::db::{create_pool, ensure_schema};
use slack_core::models::{Account, SyncLoop, SyncState};
use slack_core::store;
use sqlx::postgres::PgPool;
use tracing::{error, info, warn};

use crate::api::Slack;
use crate::backfill::backfill_step;
use crate::error::IngestError;
use crate::pages::IngestContext;
use crate::poll::{PollPass, poll_once};

const BACKFILL_IDLE: Duration = Duration::from_secs(300);

/// The session credential was already dead at boot.
#[derive(Debug, thiserror::Error)]
pub enum BootError {
    #[error("session credential rejected: {0}")]
    Auth(String),
    #[error(transparent)]
    Ingest(#[from] IngestError),
}

impl From<slack_client::SlackError> for BootError {
    fn from(error: slack_client::SlackError) -> Self {
        Self::Ingest(error.into())
    }
}

impl From<sqlx::Error> for BootError {
    fn from(error: sqlx::Error) -> Self {
        Self::Ingest(error.into())
    }
}

/// Identify the account, register it, and load the reference data.
///
/// An auth failure here is fatal rather than something to retry: a token that is dead on
/// arrival will not revive, and idling on it would look like a working ingester that
/// simply never finds anything.
pub async fn boot<C: Slack>(
    pool: &PgPool,
    client: &C,
    settings: &IngestSettings,
) -> Result<(IngestContext, String), BootError> {
    let (team_id, my_user_id) = match client.auth_test().await {
        Ok(identity) => identity,
        Err(error) if error.is_auth() => {
            let code = error.code().unwrap_or("invalid_auth").to_owned();
            record_boot_failure(pool, &settings.account, &code).await;
            return Err(BootError::Auth(code));
        }
        Err(error) => return Err(error.into()),
    };

    let workspace = client.team_info(None).await?;
    store::upsert_workspace(pool, &workspace).await?;
    store::upsert_account(
        pool,
        &Account {
            id: settings.account.clone(),
            team_id: team_id.clone(),
            my_user_id: my_user_id.clone(),
            enabled: true,
            last_auth_error: None,
            last_auth_error_at: None,
            updated_at: None,
        },
    )
    .await?;
    store::set_account_auth_error(pool, &settings.account, None).await?;

    store::upsert_emoji(pool, &client.emoji_list(&team_id).await?).await?;
    store::upsert_users(pool, &client.users_list(&settings.account).await?).await?;

    let context = IngestContext::new(
        settings.account.clone(),
        my_user_id,
        settings.scope_channels.iter().cloned().collect(),
    );
    Ok((context, team_id))
}

/// Best-effort: the account row may not exist yet on a first-ever boot.
async fn record_boot_failure(pool: &PgPool, account_id: &str, error: &str) {
    if let Err(error) = store::set_account_auth_error(pool, account_id, Some(error)).await {
        error!("could not record the auth failure for {account_id}: {error}");
    }
}

/// Poll forever, or `passes` times in tests.
///
/// An auth failure does not end the process: it is recorded, Slack calls stop for that
/// pass, and the next pass tries again. Data goes stale while the MCP tools keep serving
/// what is already stored, which is a better failure than a dead container.
pub async fn poll_loop<C: Slack>(
    pool: &PgPool,
    client: &C,
    context: &IngestContext,
    settings: &IngestSettings,
    team_id: &str,
    passes: Option<u64>,
) {
    let mut completed = 0;
    while passes.is_none_or(|passes| completed < passes) {
        let mut pass = PollPass::new(team_id, settings.reaction_window);
        // With backfill running there is no reason for the poll loop to crawl history
        // too; without it, there is no one else to do it.
        pass.bootstrap_days = settings
            .backfill_enabled
            .then_some(settings.poll_bootstrap_days);
        pass.pass_number = completed;
        pass.dormant_days = Some(settings.poll_dormant_days);
        pass.dormant_every = settings.poll_dormant_every;

        match poll_once(pool, client, context, &pass).await {
            Err(error) if error.is_auth() => {
                let code = error.code().unwrap_or("invalid_auth");
                error!("poll pass aborted: {code}");
                if let Err(error) =
                    store::set_account_auth_error(pool, &context.account_id, Some(code)).await
                {
                    error!("could not record the auth failure: {error}");
                }
            }
            Err(error) => {
                error!("poll pass failed: {error}");
                let now = Utc::now();
                let state = SyncState {
                    account_id: context.account_id.clone(),
                    r#loop: SyncLoop::Poll,
                    current_conversation_id: None,
                    last_run_at: None,
                    last_error: Some(error.to_string()),
                    last_error_at: Some(now),
                };
                if let Err(error) = store::set_sync_state(pool, &state).await {
                    error!("could not record the failed pass: {error}");
                }
            }
            Ok(()) => {
                // A pass that got through without an auth failure is the evidence that
                // the session works. Without this the flag was only ever cleared at boot,
                // so one transient `invalid_auth` left `status` reporting "data is frozen
                // until the token is replaced" for as long as the container lived — while
                // the very same loop kept ingesting.
                if let Err(error) =
                    store::set_account_auth_error(pool, &context.account_id, None).await
                {
                    error!("could not clear the auth flag: {error}");
                }
            }
        }
        completed += 1;
        if passes.is_none_or(|passes| completed < passes) {
            tokio::time::sleep(Duration::from_secs(settings.poll_interval)).await;
        }
    }
}

/// Crawl history until there is none left, then check back occasionally.
///
/// New conversations enter scope over time, so "done" is a resting state rather than an
/// exit.
pub async fn backfill_loop<C: Slack>(
    pool: &PgPool,
    client: &C,
    context: &IngestContext,
    steps: Option<u64>,
    idle: Duration,
) {
    let mut completed = 0;
    // Conversations that failed this run. Held here rather than in the database so a
    // restart retries them: a transient Slack error is not a permanent verdict.
    let mut skip: HashSet<String> = HashSet::new();
    while steps.is_none_or(|steps| completed < steps) {
        let worked = match backfill_step(pool, client, context, 200, &mut skip).await {
            Ok(worked) => worked,
            Err(error) if error.is_auth() => {
                warn!(
                    "backfill paused: {}",
                    error.code().unwrap_or("invalid_auth")
                );
                false
            }
            Err(error) => {
                // A database failure is not one conversation's fault, so the crawl rests
                // rather than spinning on it. `backfill_step` already handles a Slack
                // failure per conversation.
                error!("backfill step failed: {error}");
                false
            }
        };
        completed += 1;
        if !worked && steps.is_none_or(|steps| completed < steps) {
            skip.clear();
            tokio::time::sleep(idle).await;
        }
    }
}

/// The `slack-ingest` entrypoint: boot, then both loops until the process ends.
pub async fn run(settings: &IngestSettings) -> Result<(), BootError> {
    let pool = create_pool(settings.dsn.expose()).await?;
    let result = run_with_pool(&pool, settings).await;
    pool.close().await;
    result
}

async fn run_with_pool(pool: &PgPool, settings: &IngestSettings) -> Result<(), BootError> {
    ensure_schema(pool).await?;
    let client = SlackClient::new(
        settings.xoxc.expose(),
        settings.xoxd.expose(),
        settings.rate_limit_rps,
        settings.rate_limit_burst,
    )?;
    let (context, team_id) = boot(pool, &client, settings).await?;
    // The scope list is printed as parsed, not as configured. A `SLACK_SCOPE_CHANNELS`
    // this process read differently from the last one is a silent failure — the named
    // channels simply stop collecting — and one log line at boot is the cheapest place to
    // see it. No credential is in reach of this line.
    let mut scope: Vec<&str> = context.scope_channels.iter().map(String::as_str).collect();
    scope.sort_unstable();
    info!(
        "ingesting account {} of team {team_id}, scope: {scope:?}",
        settings.account
    );

    // One task, two futures. The Python used `asyncio.gather` over two tasks; `join!`
    // gives the same interleaving without either loop needing to be `Send`, and the poll
    // loop's sleep is where the backfill gets the whole rate limiter.
    if settings.backfill_enabled {
        tokio::join!(
            poll_loop(pool, &client, &context, settings, &team_id, None),
            backfill_loop(pool, &client, &context, None, BACKFILL_IDLE),
        );
    } else {
        poll_loop(pool, &client, &context, settings, &team_id, None).await;
    }
    Ok(())
}
