//! The backfill loop: history older than the day we started, one page at a time.
//!
//! Shares the poll loop's rate limiter and marks every call as background work, so it
//! yields to polling instead of splitting the budget with it. The bucket alone is FIFO,
//! and FIFO would let a continuously-running crawl of 2019 double how long a poll pass
//! takes to surface a message from a minute ago.

use std::collections::HashSet;

use chrono::Utc;
use slack_client::HistoryQuery;
use slack_core::models::{ScopeReason, SyncLoop, SyncState};
use slack_core::store::{self, CursorUpdate};
use sqlx::postgres::PgPool;
use tracing::{error, info};

use crate::api::Slack;
use crate::discovery::scope_for;
use crate::error::IngestError;
use crate::pages::{IngestContext, fetch_threads, write_page};

/// Tokens the backfill leaves for the poll loop. Expressed as a floor rather than a
/// share so it costs nothing while polling is idle — between passes the crawl still gets
/// the whole rate.
pub const POLL_RESERVE: f64 = 2.0;

/// Crawl one page of the oldest conversation still owing history.
///
/// Returns `false` when there is nothing left to backfill. Doing one page per call rather
/// than one conversation keeps the loop interruptible and keeps a huge channel from
/// monopolising the rate limiter.
///
/// `skip` is the caller's memory of conversations that failed this run; a failure adds to
/// it rather than marking the conversation done.
pub async fn backfill_step<C: Slack>(
    pool: &PgPool,
    client: &C,
    context: &IngestContext,
    page_limit: u32,
    skip: &mut HashSet<String>,
) -> Result<bool, IngestError> {
    let excluded: Vec<String> = skip.iter().cloned().collect();
    let Some(mut conversation) =
        store::next_backfill_conversation(pool, &context.account_id, &excluded).await?
    else {
        return Ok(false);
    };

    store::set_sync_progress(
        pool,
        &context.account_id,
        SyncLoop::Backfill,
        Some(&conversation.id),
        Some(Utc::now()),
    )
    .await?;

    // A channel dropped from SLACK_SCOPE_CHANNELS stops collecting new rows and is marked
    // done. Already-stored messages stay: deleting history because a config line changed
    // is a destructive act the ingester should never take on its own. `in_scope` in the
    // database is sticky by design, so "still in scope" is decided against the live
    // config: a channel that was named and no longer is has left.
    let (in_scope, _reason) = scope_for(&conversation, &context.scope_channels);
    if conversation.scope_reason == Some(ScopeReason::NamedChannel) && !in_scope {
        info!("{} left scope mid-backfill; marking done", conversation.id);
        finish(pool, &context.account_id, &conversation.id).await?;
        return Ok(true);
    }

    let answer = client
        .conversations_history(
            &conversation.id,
            &context.account_id,
            HistoryQuery {
                oldest: None,
                latest: conversation.backfill_cursor.as_deref(),
                cursor: None,
                limit: page_limit,
                reserve: POLL_RESERVE,
            },
        )
        .await;
    let (messages, more) = match answer {
        Ok(answer) => answer,
        Err(error) if error.is_auth() => return Err(error.into()),
        Err(error) => {
            error!("backfilling {} failed: {error}", conversation.id);
            let now = Utc::now();
            store::set_sync_state(
                pool,
                &SyncState {
                    account_id: context.account_id.clone(),
                    r#loop: SyncLoop::Backfill,
                    current_conversation_id: Some(conversation.id.clone()),
                    last_run_at: None,
                    last_error: Some(error.to_string()),
                    last_error_at: Some(now),
                },
            )
            .await?;
            // Skipped for this run rather than marked done: the crawl moves on, but the
            // conversation still owes its history and a restart picks it up again.
            // Marking it done would record "history complete" for a page we never read.
            skip.insert(conversation.id.clone());
            return Ok(true);
        }
    };

    if messages.is_empty() {
        // An empty conversation would otherwise loop on a null cursor forever.
        finish(pool, &context.account_id, &conversation.id).await?;
        return Ok(true);
    }

    let result = write_page(pool, &mut conversation, &messages, context).await?;
    let mut threads: HashSet<String> = result.new_threads;
    threads.extend(result.touched_threads);
    threads.extend(result.reply_parents);
    if !threads.is_empty() {
        fetch_threads(pool, client, &conversation, &threads, context, POLL_RESERVE).await?;
    }

    let oldest = messages
        .iter()
        .map(|m| m.ts.as_str())
        .min()
        .expect("a non-empty page");
    // Advancing only after the write has committed means a crash re-reads this page.
    // `latest` is inclusive-exclusive at the boundary, so the oldest ts is the right next
    // `latest`; the page it re-reads costs an idempotent upsert.
    store::set_conversation_cursors(
        pool,
        &context.account_id,
        &conversation.id,
        CursorUpdate {
            backfill_cursor: Some(oldest),
            ..CursorUpdate::default()
        },
    )
    .await?;
    if more.is_none() {
        // Slack signals the end of history by returning no next cursor, which is the same
        // thing as `has_more: false`. A short page is not the signal — a page can be short
        // and still have more behind it.
        finish(pool, &context.account_id, &conversation.id).await?;
    }
    Ok(true)
}

async fn finish(pool: &PgPool, account_id: &str, conversation_id: &str) -> Result<(), IngestError> {
    store::set_conversation_cursors(
        pool,
        account_id,
        conversation_id,
        CursorUpdate {
            backfill_done: Some(true),
            ..CursorUpdate::default()
        },
    )
    .await?;
    Ok(())
}
