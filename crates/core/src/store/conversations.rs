//! `conversation` and `workspace`: discovery's writes, the ingester's own reads over
//! them, and the read surface's summaries.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::{PgExecutor, Row};

use crate::models::{
    Conversation, ConversationSummary, ConversationType, ScopeReason, Workspace, WorkspaceSummary,
};
use crate::store::{parse_column, parse_optional_column};

/// The columns a whole `Conversation` needs, written once. A macro rather than a `const`
/// so the queries below stay `const` through `concat!` — sqlx 0.9 refuses a query built
/// from a runtime `String` outright, which is how "no caller value reaches the SQL text"
/// became a compile error rather than a convention.
macro_rules! conversation_columns {
    () => {
        "account_id, id, team_id, shared_team_ids, type, name, is_member, in_scope, \
         scope_reason, poll_cursor, backfill_cursor, backfill_done, reactions_fresh_since, \
         last_polled_at, counterpart_user_id"
    };
}

const POLLABLE_CONVERSATIONS: &str = concat!(
    "SELECT ",
    conversation_columns!(),
    " FROM conversation WHERE account_id = $1 AND is_member ORDER BY id"
);

const ONE_CONVERSATION: &str = concat!(
    "SELECT ",
    conversation_columns!(),
    " FROM conversation WHERE account_id = $1 AND id = $2"
);

const NEXT_BACKFILL: &str = concat!(
    "SELECT ",
    conversation_columns!(),
    "
      FROM conversation
     WHERE account_id = $1 AND in_scope AND NOT backfill_done
       AND id <> ALL($2::text[])
     ORDER BY id LIMIT 1"
);

fn conversation_from_row(row: &PgRow) -> Result<Conversation, sqlx::Error> {
    let kind: String = row.try_get("type")?;
    let scope_reason: Option<String> = row.try_get("scope_reason")?;
    Ok(Conversation {
        account_id: row.try_get("account_id")?,
        id: row.try_get("id")?,
        team_id: row.try_get("team_id")?,
        shared_team_ids: row.try_get("shared_team_ids")?,
        kind: parse_column::<ConversationType>(&kind)?,
        name: row.try_get("name")?,
        is_member: row.try_get("is_member")?,
        in_scope: row.try_get("in_scope")?,
        scope_reason: parse_optional_column::<ScopeReason>(scope_reason.as_deref())?,
        poll_cursor: row.try_get("poll_cursor")?,
        backfill_cursor: row.try_get("backfill_cursor")?,
        backfill_done: row.try_get("backfill_done")?,
        reactions_fresh_since: row.try_get("reactions_fresh_since")?,
        last_polled_at: row.try_get("last_polled_at")?,
        counterpart_user_id: row.try_get("counterpart_user_id")?,
        // Not a column: Slack's listing says a channel is externally shared, and only
        // discovery cares, within the pass that read it.
        is_ext_shared: false,
    })
}

/// Insert or refresh a team.
///
/// A row already carrying a name is never overwritten with NULL: external teams are
/// first seen as a bare ID via a shared channel and may later be named, but never the
/// reverse.
pub async fn upsert_workspace(pool: &PgPool, workspace: &Workspace) -> Result<(), sqlx::Error> {
    sqlx::query(
        "
        INSERT INTO workspace (team_id, name, domain, is_external)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (team_id) DO UPDATE SET
          name        = COALESCE(EXCLUDED.name, workspace.name),
          domain      = COALESCE(EXCLUDED.domain, workspace.domain),
          is_external = EXCLUDED.is_external AND workspace.is_external
        ",
    )
    .bind(&workspace.team_id)
    .bind(workspace.name.as_deref())
    .bind(workspace.domain.as_deref())
    .bind(workspace.is_external)
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert or refresh a conversation's discovered metadata.
///
/// Cursors and backfill progress are deliberately absent from the UPDATE clause:
/// discovery runs every poll pass and must never rewind a crawl that is in flight. Use
/// [`set_conversation_cursors`] for those.
///
/// Generic over the executor — the only function here that is — because discovery calls
/// it with the pool and the page writer calls it inside its own transaction.
pub async fn upsert_conversation<'e>(
    db: impl PgExecutor<'e>,
    conversation: &Conversation,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "
        INSERT INTO conversation (
          account_id, id, team_id, shared_team_ids, type, name,
          is_member, in_scope, scope_reason, counterpart_user_id
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        ON CONFLICT (account_id, id) DO UPDATE SET
          team_id         = COALESCE(EXCLUDED.team_id, conversation.team_id),
          shared_team_ids = EXCLUDED.shared_team_ids,
          type            = EXCLUDED.type,
          name            = COALESCE(EXCLUDED.name, conversation.name),
          is_member       = EXCLUDED.is_member,
          in_scope        = EXCLUDED.in_scope OR conversation.in_scope,
          -- A stronger reason replaces a weaker one: a channel first seen through an
          -- @-mention and later named in SLACK_SCOPE_CHANNELS reads as `named_channel`,
          -- not as the label of how it first slipped in. NULL never overwrites, because
          -- discovery reports NULL for a channel that has LEFT the scope list and backfill
          -- needs the stale `named_channel` to notice that it left.
          scope_reason    = CASE
            WHEN EXCLUDED.scope_reason IS NOT NULL
             AND EXCLUDED.scope_reason <> 'mention_thread'
              THEN EXCLUDED.scope_reason
            ELSE COALESCE(conversation.scope_reason, EXCLUDED.scope_reason)
          END,
          counterpart_user_id = COALESCE(
            EXCLUDED.counterpart_user_id, conversation.counterpart_user_id)
        ",
    )
    .bind(&conversation.account_id)
    .bind(&conversation.id)
    .bind(conversation.team_id.as_deref())
    .bind(&conversation.shared_team_ids)
    .bind(conversation.kind.as_str())
    .bind(conversation.name.as_deref())
    .bind(conversation.is_member)
    .bind(conversation.in_scope)
    .bind(conversation.scope_reason.map(|reason| reason.as_str()))
    .bind(conversation.counterpart_user_id.as_deref())
    .execute(db)
    .await?;
    Ok(())
}

/// Which cursors to advance. Every field is optional and `None` means "don't touch",
/// which is why each column is written through a COALESCE against the parameter rather
/// than assigned directly.
#[derive(Debug, Clone, Default)]
pub struct CursorUpdate<'a> {
    pub poll_cursor: Option<&'a str>,
    pub backfill_cursor: Option<&'a str>,
    pub backfill_done: Option<bool>,
    pub reactions_fresh_since: Option<DateTime<Utc>>,
    pub last_polled_at: Option<DateTime<Utc>>,
}

/// Advance whichever cursors were supplied; leave the rest untouched.
pub async fn set_conversation_cursors(
    pool: &PgPool,
    account_id: &str,
    conversation_id: &str,
    update: CursorUpdate<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "
        UPDATE conversation
           SET poll_cursor           = COALESCE($3, poll_cursor),
               backfill_cursor       = COALESCE($4, backfill_cursor),
               backfill_done         = COALESCE($5, backfill_done),
               reactions_fresh_since = COALESCE($6, reactions_fresh_since),
               last_polled_at        = COALESCE($7, last_polled_at)
         WHERE account_id = $1 AND id = $2
        ",
    )
    .bind(account_id)
    .bind(conversation_id)
    .bind(update.poll_cursor)
    .bind(update.backfill_cursor)
    .bind(update.backfill_done)
    .bind(update.reactions_fresh_since)
    .bind(update.last_polled_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark a conversation joined or left. Stored messages are never removed.
pub async fn set_conversation_membership(
    pool: &PgPool,
    account_id: &str,
    conversation_id: &str,
    is_member: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE conversation SET is_member = $3 WHERE account_id = $1 AND id = $2")
        .bind(account_id)
        .bind(conversation_id)
        .bind(is_member)
        .execute(pool)
        .await?;
    Ok(())
}

/// Conversations this account is currently recorded as a member of.
pub async fn list_member_conversation_ids(
    pool: &PgPool,
    account_id: &str,
) -> Result<HashSet<String>, sqlx::Error> {
    let rows = sqlx::query("SELECT id FROM conversation WHERE account_id = $1 AND is_member")
        .bind(account_id)
        .fetch_all(pool)
        .await?;
    rows.iter().map(|row| row.try_get("id")).collect()
}

/// Every conversation worth calling `conversations.history` on this pass.
///
/// Non-member conversations are excluded: their history is already stored and Slack
/// would refuse the call anyway. Out-of-scope member channels are still polled — that is
/// where mentions are discovered.
pub async fn list_pollable_conversations(
    pool: &PgPool,
    account_id: &str,
) -> Result<Vec<Conversation>, sqlx::Error> {
    let rows = sqlx::query(POLLABLE_CONVERSATIONS)
        .bind(account_id)
        .fetch_all(pool)
        .await?;
    rows.iter().map(conversation_from_row).collect()
}

/// conversation id -> newest stored `posted_at`, thread replies included.
///
/// The poll loop's measure of whether a conversation is worth reading every pass. A
/// conversation with nothing stored is absent from the result.
pub async fn last_activity(
    pool: &PgPool,
    account_id: &str,
) -> Result<HashMap<String, DateTime<Utc>>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT conversation_id, max(posted_at) AS last FROM message \
         WHERE account_id = $1 GROUP BY conversation_id",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| Ok((row.try_get("conversation_id")?, row.try_get("last")?)))
        .collect()
}

pub async fn get_conversation(
    pool: &PgPool,
    account_id: &str,
    conversation_id: &str,
) -> Result<Option<Conversation>, sqlx::Error> {
    let row = sqlx::query(ONE_CONVERSATION)
        .bind(account_id)
        .bind(conversation_id)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(conversation_from_row).transpose()
}

/// The oldest in-scope conversation still owing a backfill, if any.
///
/// `exclude` lets the caller skip conversations that failed this run without marking
/// them done — a transient failure must not be recorded as "history complete".
pub async fn next_backfill_conversation(
    pool: &PgPool,
    account_id: &str,
    exclude: &[String],
) -> Result<Option<Conversation>, sqlx::Error> {
    let row = sqlx::query(NEXT_BACKFILL)
        .bind(account_id)
        .bind(exclude)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(conversation_from_row).transpose()
}

pub async fn list_conversations(
    pool: &PgPool,
    account: Option<&str>,
    team_id: Option<&str>,
    kind: Option<&str>,
) -> Result<Vec<ConversationSummary>, sqlx::Error> {
    let rows = sqlx::query(
        "
        SELECT c.account_id, c.id, c.team_id, c.type, c.name, c.scope_reason,
               c.last_polled_at, c.reactions_fresh_since,
               count(m.ts)      AS message_count,
               max(m.posted_at) AS last_activity
          FROM conversation c
          LEFT JOIN message m ON m.account_id = c.account_id AND m.conversation_id = c.id
         WHERE c.in_scope
           AND ($1::text IS NULL OR c.account_id = $1)
           AND ($2::text IS NULL OR c.team_id = $2)
           AND ($3::text IS NULL OR c.type = $3)
         GROUP BY c.account_id, c.id, c.team_id, c.type, c.name, c.scope_reason,
                  c.last_polled_at, c.reactions_fresh_since
         ORDER BY last_activity DESC NULLS LAST, c.name NULLS LAST
        ",
    )
    .bind(account)
    .bind(team_id)
    .bind(kind)
    .fetch_all(pool)
    .await?;

    let mut summaries = Vec::with_capacity(rows.len());
    for row in rows {
        let kind: String = row.try_get("type")?;
        let scope_reason: Option<String> = row.try_get("scope_reason")?;
        summaries.push(ConversationSummary {
            account_id: row.try_get("account_id")?,
            id: row.try_get("id")?,
            team_id: row.try_get("team_id")?,
            kind: parse_column::<ConversationType>(&kind)?,
            name: row.try_get("name")?,
            scope_reason: parse_optional_column::<ScopeReason>(scope_reason.as_deref())?,
            message_count: row.try_get("message_count")?,
            last_activity: row.try_get("last_activity")?,
            last_polled_at: row.try_get("last_polled_at")?,
            reactions_fresh_since: row.try_get("reactions_fresh_since")?,
        });
    }
    Ok(summaries)
}

pub async fn list_workspaces(pool: &PgPool) -> Result<Vec<WorkspaceSummary>, sqlx::Error> {
    let rows = sqlx::query(
        "
        SELECT w.team_id, w.name, w.domain, w.is_external,
               count(c.id) FILTER (WHERE c.in_scope) AS conversation_count
          FROM workspace w
          LEFT JOIN conversation c ON c.team_id = w.team_id
         GROUP BY w.team_id, w.name, w.domain, w.is_external
         ORDER BY w.is_external, w.name NULLS LAST, w.team_id
        ",
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(WorkspaceSummary {
                team_id: row.try_get("team_id")?,
                name: row.try_get("name")?,
                domain: row.try_get("domain")?,
                is_external: row.try_get("is_external")?,
                conversation_count: row.try_get("conversation_count")?,
            })
        })
        .collect()
}

/// A team ID, name or domain to a team ID. Unknown input resolves to nothing.
///
/// Returning `None` rather than falling back to "no filter" matters: a typo'd
/// workspace must produce an empty result, not silently widen the search to every
/// team.
pub async fn resolve_workspace(
    pool: &PgPool,
    workspace: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "
        SELECT team_id FROM workspace
         WHERE team_id = $1 OR domain = $1 OR lower(name) = lower($1)
         ORDER BY (team_id = $1) DESC
         LIMIT 1
        ",
    )
    .bind(workspace)
    .fetch_optional(pool)
    .await
}
