//! The poll loop: new messages, their threads, and reaction freshness.

use std::collections::HashSet;

use chrono::{DateTime, TimeDelta, Utc};
use slack_client::HistoryQuery;
use slack_core::models::{Conversation, SyncLoop, SyncState};
use slack_core::store::{self, CursorUpdate};
use sqlx::postgres::PgPool;
use tracing::{error, info};

use crate::api::Slack;
use crate::discovery::discover;
use crate::error::IngestError;
use crate::pages::{IngestContext, fetch_threads, write_page};

/// Slack's `ts` for an instant: whole seconds and six decimals, as the API wants it.
pub fn to_slack_ts(moment: DateTime<Utc>) -> String {
    format!(
        "{}.{:06}",
        moment.timestamp(),
        moment.timestamp_subsec_micros()
    )
}

/// What one pass does, beyond the two things it cannot default.
///
/// Field assignment rather than a builder: the Python's keyword arguments, with the same
/// defaults.
#[derive(Debug, Clone)]
pub struct PollPass<'a> {
    pub team_id: &'a str,
    pub reaction_window_days: i64,
    pub bootstrap_days: Option<i64>,
    pub hydrate_limit: i64,
    pub page_limit: u32,
    pub pass_number: u64,
    pub dormant_days: Option<i64>,
    pub dormant_every: u32,
}

impl<'a> PollPass<'a> {
    pub fn new(team_id: &'a str, reaction_window_days: i64) -> Self {
        Self {
            team_id,
            reaction_window_days,
            bootstrap_days: None,
            hydrate_limit: 50,
            page_limit: 200,
            pass_number: 0,
            dormant_days: None,
            dormant_every: 1,
        }
    }
}

/// Which pass reads a dormant conversation, from a hash of its id.
///
/// `CRC_32_ISO_HDLC` is the algorithm behind `zlib.crc32`, and it agrees with it
/// value-for-value — `slots_match_the_python_crc32` pins four of them. The slots
/// therefore do not shift on cutover, which is worth more than it sounds: a shift would
/// be invisible and unattributable afterwards.
fn slot_of(conversation_id: &str, dormant_every: u32) -> u32 {
    const CRC32: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);
    CRC32.checksum(conversation_id.as_bytes()) % dormant_every
}

/// Whether this pass reads `conversation`'s new messages.
///
/// Most conversations have been silent for months, and reading every one of them every
/// pass is what made a pass take eight minutes at Slack's rate limit. A dormant one is
/// read every `dormant_every`-th pass instead, so a message that wakes it up waits at
/// most that many passes. Each gets its own slot, from a hash of its id rather than its
/// position in a list that changes, so the dormant reads spread evenly across passes
/// instead of landing all on one.
pub fn is_due(
    conversation: &Conversation,
    last: Option<DateTime<Utc>>,
    pass_number: u64,
    dormant_since: Option<DateTime<Utc>>,
    dormant_every: u32,
) -> bool {
    let Some(dormant_since) = dormant_since else {
        return true;
    };
    if dormant_every <= 1 || conversation.poll_cursor.is_none() {
        return true;
    }
    if last.is_some_and(|last| last >= dormant_since) {
        return true;
    }
    pass_number % u64::from(dormant_every) == u64::from(slot_of(&conversation.id, dormant_every))
}

/// One pass: discover, read new messages, refresh the recent window, fill threads.
///
/// Threads are filled last because the refresh is what notices a parent stored before
/// anyone replied has since gained replies; filling them first left those replies
/// waiting a whole extra pass.
///
/// A conversation that fails for a non-auth reason is logged and skipped; one poisoned
/// conversation must not stop the pass. An auth failure propagates — a dead session makes
/// every remaining call pointless.
pub async fn poll_once<C: Slack>(
    pool: &PgPool,
    client: &C,
    context: &IngestContext,
    pass: &PollPass<'_>,
) -> Result<(), IngestError> {
    discover(
        pool,
        client,
        &context.account_id,
        pass.team_id,
        &context.scope_channels,
    )
    .await?;
    context
        .replace_known_threads(store::list_mention_thread_keys(pool, &context.account_id).await?);

    let mut errors: Vec<String> = Vec::new();
    let mut conversations = store::list_pollable_conversations(pool, &context.account_id).await?;
    let activity = store::last_activity(pool, &context.account_id).await?;
    let dormant_since = pass
        .dormant_days
        .map(|days| Utc::now() - TimeDelta::days(days));

    for conversation in &mut conversations {
        if !is_due(
            conversation,
            activity.get(&conversation.id).copied(),
            pass.pass_number,
            dormant_since,
            pass.dormant_every,
        ) {
            continue;
        }
        match poll_conversation(pool, client, conversation, context, pass).await {
            Ok(_) => {}
            Err(error) if error.is_auth() => return Err(error),
            Err(error) => {
                error!("polling {} failed: {error}", conversation.id);
                errors.push(format!("{}: {error}", conversation.id));
            }
        }
    }

    for conversation in &mut conversations {
        if !conversation.in_scope {
            continue;
        }
        match refresh_window(pool, client, conversation, context, pass).await {
            Ok(_) => {}
            Err(error) if error.is_auth() => return Err(error),
            Err(error) => {
                error!("refresh window for {} failed: {error}", conversation.id);
                errors.push(format!("{} refresh: {error}", conversation.id));
            }
        }
    }

    match hydrate_threads(pool, client, context, pass.hydrate_limit).await {
        Ok(_) => {}
        Err(error) if error.is_auth() => return Err(error),
        Err(error) => {
            error!("thread hydration failed: {error}");
            errors.push(format!("threads: {error}"));
        }
    }

    let now = Utc::now();
    let last_error = (!errors.is_empty()).then(|| errors.join("; "));
    store::set_sync_state(
        pool,
        &SyncState {
            account_id: context.account_id.clone(),
            r#loop: SyncLoop::Poll,
            current_conversation_id: None,
            last_run_at: Some(now),
            last_error_at: last_error.as_ref().map(|_| now),
            last_error,
        },
    )
    .await?;
    Ok(())
}

/// Read everything newer than `poll_cursor`, then advance it.
///
/// The cursor moves only after every page of the pass has committed, so a crash re-reads
/// a page rather than skipping one. Re-reading costs an idempotent upsert; skipping loses
/// a message permanently.
///
/// On the first pass there is no cursor, and without a floor this walks every
/// conversation back to its beginning — the backfill loop's job, done by the loop that is
/// supposed to be surfacing new messages. `bootstrap_days` is that floor. It is only safe
/// because backfill starts from the newest message and pages backwards, so it covers
/// exactly what this skips; the runner passes `None` when backfill is disabled, and then
/// the full crawl here is the only mechanism there is.
pub async fn poll_conversation<C: Slack>(
    pool: &PgPool,
    client: &C,
    conversation: &mut Conversation,
    context: &IngestContext,
    pass: &PollPass<'_>,
) -> Result<usize, IngestError> {
    let oldest: Option<String> = conversation.poll_cursor.clone().or_else(|| {
        pass.bootstrap_days
            .map(|days| to_slack_ts(Utc::now() - TimeDelta::days(days)))
    });
    // Seeding `newest` with the floor means a conversation silent through the whole
    // window still advances its cursor, instead of re-scanning the window every pass.
    let mut newest = oldest.clone();
    let mut cursor: Option<String> = None;
    let mut stored_total = 0;
    let mut threads: HashSet<String> = HashSet::new();

    loop {
        let (messages, next) = client
            .conversations_history(
                &conversation.id,
                &context.account_id,
                HistoryQuery {
                    oldest: oldest.as_deref(),
                    latest: None,
                    cursor: cursor.as_deref(),
                    limit: pass.page_limit,
                    reserve: 0.0,
                },
            )
            .await?;
        if !messages.is_empty() {
            let result = write_page(pool, conversation, &messages, context).await?;
            stored_total += result.stored.len();
            threads.extend(result.new_threads);
            threads.extend(result.touched_threads);
            threads.extend(result.reply_parents);
            // String comparison is the right one here: Slack's ts is fixed-width
            // seconds.microseconds, so lexical and chronological order agree.
            let page_newest = messages.iter().map(|m| m.ts.as_str()).max().unwrap_or("");
            if newest.as_deref().unwrap_or("") < page_newest {
                newest = Some(page_newest.to_owned());
            }
        }
        cursor = next;
        if cursor.is_none() {
            break;
        }
    }

    // Parents that claim replies need their own call: history never carries them.
    for thread_ts in sorted(&threads) {
        fetch_threads(
            pool,
            client,
            conversation,
            &HashSet::from([thread_ts]),
            context,
            0.0,
        )
        .await?;
    }

    let polled_at = Some(Utc::now());
    if newest.is_some() && newest != conversation.poll_cursor {
        store::set_conversation_cursors(
            pool,
            &context.account_id,
            &conversation.id,
            CursorUpdate {
                poll_cursor: newest.as_deref(),
                last_polled_at: polled_at,
                ..CursorUpdate::default()
            },
        )
        .await?;
        conversation.poll_cursor = newest;
    } else {
        store::set_conversation_cursors(
            pool,
            &context.account_id,
            &conversation.id,
            CursorUpdate {
                last_polled_at: polled_at,
                ..CursorUpdate::default()
            },
        )
        .await?;
    }
    Ok(stored_total)
}

/// Fetch replies for stored parents that claim more than we hold.
///
/// Bounded per pass so a corpus that arrives with a large thread backlog drains steadily
/// instead of spending an entire pass on it. Threads whose parents were read before this
/// existed are picked up here rather than needing a re-crawl.
pub async fn hydrate_threads<C: Slack>(
    pool: &PgPool,
    client: &C,
    context: &IngestContext,
    limit: i64,
) -> Result<usize, IngestError> {
    let owed = store::list_unhydrated_threads(pool, &context.account_id, limit).await?;
    if owed.is_empty() {
        return Ok(0);
    }
    info!("hydrating {} thread(s)", owed.len());
    for (conversation_id, thread_ts) in &owed {
        let Some(conversation) =
            store::get_conversation(pool, &context.account_id, conversation_id).await?
        else {
            continue;
        };
        fetch_threads(
            pool,
            client,
            &conversation,
            &HashSet::from([thread_ts.clone()]),
            context,
            0.0,
        )
        .await?;
    }
    Ok(owed.len())
}

/// Re-read the recent window and reconcile it: reactions, edits, deletions.
///
/// All three are the same problem. None of them is an event we are told about, and a
/// message's timestamp does not move when any of them happens, so the only way to learn
/// about them is to look at the message again. Reactions are a snapshot,
/// `reactions_fresh_since` records the boundary, and counts older than it are a floor
/// rather than a truth.
///
/// Edits are handled by the page writer, which sees every message on the page and knows
/// which ones it already held. Deletions are handled here, because they are the one thing
/// that cannot be seen on a page — only in what a complete page is missing.
///
/// This is the expensive step and its cost scales with in-scope volume rather than with
/// new messages, which is why it runs after new messages are already stored.
///
/// Returns how many messages this pass promoted to deleted.
pub async fn refresh_window<C: Slack>(
    pool: &PgPool,
    client: &C,
    conversation: &mut Conversation,
    context: &IngestContext,
    pass: &PollPass<'_>,
) -> Result<u64, IngestError> {
    let since = Utc::now() - TimeDelta::days(pass.reaction_window_days);
    let held = store::stored_window(pool, &context.account_id, &conversation.id, since).await?;
    let mut seen: HashSet<String> = HashSet::new();
    let mut complete = true;
    let mut cursor: Option<String> = None;

    // Nothing held in the window means nothing to reconcile: no reaction to refresh, no
    // edit to notice, no absence to judge. Anything Slack holds there that we do not is a
    // new message, which is the poll's job, not this one's. Skipping the read is what
    // keeps a pass proportional to the conversations that are actually live.
    if !held.is_empty() {
        loop {
            let answer = client
                .conversations_history(
                    &conversation.id,
                    &context.account_id,
                    HistoryQuery {
                        oldest: Some(&to_slack_ts(since)),
                        latest: None,
                        cursor: cursor.as_deref(),
                        limit: pass.page_limit,
                        reserve: 0.0,
                    },
                )
                .await;
            let (messages, next) = match answer {
                Ok(answer) => answer,
                Err(error) if error.is_auth() => return Err(error.into()),
                Err(error) => {
                    // The window was not read to the end, so nothing may be concluded
                    // from what is missing from it. Edits already recorded stand: seeing
                    // a different text is positive evidence and does not depend on
                    // completeness.
                    error!(
                        "refresh window for {} ended early: {error}",
                        conversation.id
                    );
                    complete = false;
                    break;
                }
            };
            if !messages.is_empty() {
                seen.extend(messages.iter().map(|m| m.ts.clone()));
                write_page(pool, conversation, &messages, context).await?;
            }
            cursor = next;
            if cursor.is_none() {
                break;
            }
        }
    }

    let present: Vec<String> = sorted(&seen)
        .into_iter()
        .filter(|ts| held.contains_key(ts))
        .collect();
    store::mark_present(pool, &context.account_id, &conversation.id, &present).await?;
    let mut deleted = 0;
    if complete {
        let mut absent: Vec<String> = held
            .keys()
            .filter(|ts| !seen.contains(*ts))
            .cloned()
            .collect();
        absent.sort_unstable();
        deleted = store::mark_absent(pool, &context.account_id, &conversation.id, &absent).await?;
        store::release_stranded(pool, &context.account_id, &conversation.id, since).await?;
    }
    // Set even when the window held nothing: the guarantee is about the time range, not
    // about whether anything was found in it.
    store::set_conversation_cursors(
        pool,
        &context.account_id,
        &conversation.id,
        CursorUpdate {
            reactions_fresh_since: Some(since),
            ..CursorUpdate::default()
        },
    )
    .await?;
    Ok(deleted)
}

fn sorted(keys: &HashSet<String>) -> Vec<String> {
    let mut keys: Vec<String> = keys.iter().cloned().collect();
    keys.sort_unstable();
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_match_the_python_crc32() {
        // `zlib.crc32` of the same bytes, taken from the Python interpreter. If these
        // ever disagree, every dormant conversation's slot moves at once and the poll
        // cadence shifts for a reason nobody would find afterwards.
        for (id, expected) in [
            ("C0GENERAL", 3_906_694_975_u32),
            ("C0RANDOM", 1_028_785_093),
            ("D0TED", 208_685_456),
            ("C0PARTNER", 400_123_747),
        ] {
            const CRC32: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);
            assert_eq!(CRC32.checksum(id.as_bytes()), expected, "{id}");
        }
    }

    fn dormant_conversation(poll_cursor: Option<&str>) -> Conversation {
        Conversation {
            account_id: "work".into(),
            id: "D0OLD".into(),
            team_id: None,
            shared_team_ids: Vec::new(),
            kind: slack_core::models::ConversationType::Im,
            name: None,
            is_member: true,
            counterpart_user_id: None,
            in_scope: true,
            scope_reason: None,
            poll_cursor: poll_cursor.map(str::to_owned),
            backfill_cursor: None,
            backfill_done: false,
            reactions_fresh_since: None,
            last_polled_at: None,
            is_ext_shared: false,
        }
    }

    #[test]
    fn a_dormant_conversation_is_read_once_every_n_passes() {
        let now = Utc::now();
        let cutoff = now - TimeDelta::days(30);
        let polled = dormant_conversation(Some("1.0"));
        let due: Vec<bool> = (0..10)
            .map(|pass| {
                is_due(
                    &polled,
                    Some(now - TimeDelta::days(90)),
                    pass,
                    Some(cutoff),
                    5,
                )
            })
            .collect();
        assert_eq!(due.iter().filter(|due| **due).count(), 2, "{due:?}");
        let first = due.iter().position(|due| *due).expect("a due pass");
        let last = due.iter().rposition(|due| *due).expect("a due pass");
        assert_eq!(first + 5, last, "the two reads are not a cycle apart");

        // Activity inside the cutoff, a conversation never read, and tiering switched
        // off are all read every pass.
        let fresh = dormant_conversation(None);
        for pass in 0..5 {
            assert!(is_due(&polled, Some(now), pass, Some(cutoff), 5));
            assert!(is_due(&fresh, None, pass, Some(cutoff), 5));
            assert!(is_due(&polled, None, pass, None, 5));
        }
    }

    #[test]
    fn to_slack_ts_has_six_decimals() {
        let moment = DateTime::from_timestamp(1_723_640_400, 1_200_000).unwrap();
        assert_eq!(to_slack_ts(moment), "1723640400.001200");
    }
}
