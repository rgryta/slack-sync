//! `message`, its reactions and its mention threads: the page writer's statements and
//! every read over them.
//!
//! The edit-tracking half — `record_revision`, `mark_absent` and the rest — is in
//! `revisions.rs`, because losing an edit is a different kind of bug from losing a
//! message and the two are worth reading apart.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use sqlx::postgres::PgPool;
use sqlx::{FromRow, PgConnection, Row};

use crate::models::{Identity, Message, Reaction, ResolvedMessage, Revision, SearchHit};

/// The one place the message column list is written down. A macro rather than a `const`
/// so the queries below can stay `const` through `concat!`.
macro_rules! message_columns {
    () => {
        "m.account_id, m.conversation_id, m.ts, m.thread_ts, m.user_id, m.text, \
         m.subtype, m.edited_at, m.deleted_at, m.raw, m.posted_at"
    };
}

/// Load reactions for a message list in one query rather than one per message.
async fn attach_reactions(pool: &PgPool, messages: &mut [Message]) -> Result<(), sqlx::Error> {
    if messages.is_empty() {
        return Ok(());
    }
    let accounts: Vec<String> = messages.iter().map(|m| m.account_id.clone()).collect();
    let conversations: Vec<String> = messages.iter().map(|m| m.conversation_id.clone()).collect();
    let timestamps: Vec<String> = messages.iter().map(|m| m.ts.clone()).collect();

    let rows = sqlx::query(
        "
        SELECT account_id, conversation_id, message_ts, name, user_ids, count
          FROM reaction
         WHERE (account_id, conversation_id, message_ts)
               IN (SELECT * FROM unnest($1::text[], $2::text[], $3::text[]))
         ORDER BY name
        ",
    )
    .bind(&accounts)
    .bind(&conversations)
    .bind(&timestamps)
    .fetch_all(pool)
    .await?;

    let mut by_key: HashMap<(String, String, String), Vec<Reaction>> = HashMap::new();
    for row in rows {
        let key = (
            row.try_get("account_id")?,
            row.try_get("conversation_id")?,
            row.try_get("message_ts")?,
        );
        by_key.entry(key).or_default().push(Reaction {
            name: row.try_get("name")?,
            user_ids: row.try_get("user_ids")?,
            count: row.try_get("count")?,
        });
    }
    for message in messages.iter_mut() {
        let key = (
            message.account_id.clone(),
            message.conversation_id.clone(),
            message.ts.clone(),
        );
        message.reactions = by_key.remove(&key).unwrap_or_default();
    }
    Ok(())
}

async fn with_reactions(
    pool: &PgPool,
    mut messages: Vec<Message>,
) -> Result<Vec<Message>, sqlx::Error> {
    attach_reactions(pool, &mut messages).await?;
    Ok(messages)
}

// One page of a conversation, oldest first. Two statements, not one with a computed
// direction, because no caller value may ever reach the SQL text.
//
// With `after` the page is the *oldest* `limit` messages after it: a caller who names a
// start wants to read forward from there, and handing back the end of the period
// instead reads as though everything before it were missing. Without `after` it is the
// newest `limit`, so opening a conversation shows what is going on now rather than the
// day it began.
const HISTORY_FROM_OLDEST: &str = concat!(
    "SELECT * FROM (SELECT ",
    message_columns!(),
    "
        FROM message m
       WHERE m.conversation_id = $1
         AND ($2::text IS NULL OR m.account_id = $2)
         AND ($3::timestamptz IS NULL OR m.posted_at > $3)
         AND ($4::timestamptz IS NULL OR m.posted_at < $4)
       ORDER BY m.posted_at ASC
       LIMIT $5
    ) page ORDER BY posted_at ASC"
);

const HISTORY_NEWEST: &str = concat!(
    "SELECT * FROM (SELECT ",
    message_columns!(),
    "
        FROM message m
       WHERE m.conversation_id = $1
         AND ($2::text IS NULL OR m.account_id = $2)
         AND ($3::timestamptz IS NULL OR m.posted_at > $3)
         AND ($4::timestamptz IS NULL OR m.posted_at < $4)
       ORDER BY m.posted_at DESC
       LIMIT $5
    ) page ORDER BY posted_at ASC"
);

/// One page of a conversation, oldest first. See [`HISTORY_FROM_OLDEST`] for which
/// page. [`count_history`] says how many messages the same window holds in all.
pub async fn get_history(
    pool: &PgPool,
    conversation_id: &str,
    account: Option<&str>,
    after: Option<DateTime<Utc>>,
    before: Option<DateTime<Utc>>,
    limit: i64,
) -> Result<Vec<Message>, sqlx::Error> {
    let sql = if after.is_some() {
        HISTORY_FROM_OLDEST
    } else {
        HISTORY_NEWEST
    };
    let messages = sqlx::query_as::<_, Message>(sql)
        .bind(conversation_id)
        .bind(account)
        .bind(after)
        .bind(before)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    with_reactions(pool, messages).await
}

/// Every message in the window [`get_history`] pages through.
pub async fn count_history(
    pool: &PgPool,
    conversation_id: &str,
    account: Option<&str>,
    after: Option<DateTime<Utc>>,
    before: Option<DateTime<Utc>>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "
        SELECT count(*) FROM message m
         WHERE m.conversation_id = $1
           AND ($2::text IS NULL OR m.account_id = $2)
           AND ($3::timestamptz IS NULL OR m.posted_at > $3)
           AND ($4::timestamptz IS NULL OR m.posted_at < $4)
        ",
    )
    .bind(conversation_id)
    .bind(account)
    .bind(after)
    .bind(before)
    .fetch_one(pool)
    .await
}

/// Every message in a thread, including the parent, oldest first.
pub async fn get_thread(
    pool: &PgPool,
    conversation_id: &str,
    thread_ts: &str,
    account: Option<&str>,
) -> Result<Vec<Message>, sqlx::Error> {
    let messages = sqlx::query_as::<_, Message>(concat!(
        "SELECT ",
        message_columns!(),
        "
          FROM message m
         WHERE m.conversation_id = $1
           AND (m.thread_ts = $2 OR m.ts = $2)
           AND ($3::text IS NULL OR m.account_id = $3)
         ORDER BY m.posted_at ASC"
    ))
    .bind(conversation_id)
    .bind(thread_ts)
    .bind(account)
    .fetch_all(pool)
    .await?;
    with_reactions(pool, messages).await
}

/// Full-text search over the persisted corpus.
///
/// An empty query returns no hits rather than every message: `websearch_to_tsquery`
/// yields an empty tsquery that matches nothing, and that is the honest answer to a
/// search for nothing.
#[allow(clippy::too_many_arguments)]
pub async fn search_messages(
    pool: &PgPool,
    query: &str,
    account: Option<&str>,
    team_id: Option<&str>,
    conversation_id: Option<&str>,
    authors: Option<&[Identity]>,
    after: Option<DateTime<Utc>>,
    before: Option<DateTime<Utc>>,
    has_reaction: Option<bool>,
    limit: i64,
) -> Result<Vec<SearchHit>, sqlx::Error> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let author_accounts: Option<Vec<String>> =
        authors.map(|ids| ids.iter().map(|i| i.account_id.clone()).collect());
    let author_users: Option<Vec<String>> =
        authors.map(|ids| ids.iter().map(|i| i.user_id.clone()).collect());

    let rows = sqlx::query(concat!(
        "SELECT ",
        message_columns!(),
        ",
               ts_rank_cd(m.search, websearch_to_tsquery('simple', $1)) AS rank,
               ts_headline('simple', m.text, websearch_to_tsquery('simple', $1),
                           'MaxFragments=1,MaxWords=25,MinWords=8') AS snippet
          FROM message m
          JOIN conversation c ON c.account_id = m.account_id AND c.id = m.conversation_id
         WHERE m.search @@ websearch_to_tsquery('simple', $1)
           AND ($2::text IS NULL OR m.account_id = $2)
           AND ($3::text IS NULL OR c.team_id = $3)
           AND ($4::text IS NULL OR m.conversation_id = $4)
           AND ($5::timestamptz IS NULL OR m.posted_at > $5)
           AND ($6::timestamptz IS NULL OR m.posted_at < $6)
           AND ($7::text[] IS NULL OR (m.account_id, m.user_id)
                IN (SELECT * FROM unnest($7::text[], $8::text[])))
           AND ($9::boolean IS NULL OR $9 = EXISTS (
                 SELECT 1 FROM reaction r
                  WHERE r.account_id = m.account_id
                    AND r.conversation_id = m.conversation_id
                    AND r.message_ts = m.ts))
         ORDER BY rank DESC, m.posted_at DESC
         LIMIT $10"
    ))
    .bind(query)
    .bind(account)
    .bind(team_id)
    .bind(conversation_id)
    .bind(after)
    .bind(before)
    .bind(author_accounts.as_deref())
    .bind(author_users.as_deref())
    .bind(has_reaction)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let mut ranked = Vec::with_capacity(rows.len());
    let mut messages = Vec::with_capacity(rows.len());
    for row in &rows {
        ranked.push((
            row.try_get::<f32, _>("rank")?,
            row.try_get::<String, _>("snippet")?,
        ));
        messages.push(Message::from_row(row)?);
    }
    let messages = with_reactions(pool, messages).await?;
    Ok(messages
        .into_iter()
        .zip(ranked)
        .map(|(message, (rank, snippet))| SearchHit {
            message,
            rank,
            snippet,
        })
        .collect())
}

/// Messages that mention me, newest first, wherever they are.
///
/// Reads the corpus for the literal `<@Uxxxx>` token rather than the `mention_thread`
/// table. That table only ever holds pings in conversations the scope filter did NOT
/// already capture — DMs, group DMs and named channels are stored wholesale and return
/// from earlier rules, so a mention there never registers. With this scope that made
/// the table permanently empty while the corpus held hundreds of real mentions.
///
/// A self-mention is excluded: quoting your own handle is not being pinged.
pub async fn list_mentions(
    pool: &PgPool,
    my_user_id: &str,
    account: Option<&str>,
    team_id: Option<&str>,
    after: Option<DateTime<Utc>>,
    limit: i64,
) -> Result<Vec<Message>, sqlx::Error> {
    let messages = sqlx::query_as::<_, Message>(concat!(
        "SELECT ",
        message_columns!(),
        "
          FROM message m
          JOIN conversation c ON c.account_id = m.account_id AND c.id = m.conversation_id
         WHERE m.text LIKE '%<@' || $1::text || '>%'
           AND m.user_id IS DISTINCT FROM $1::text
           AND ($2::text IS NULL OR m.account_id = $2)
           AND ($3::text IS NULL OR c.team_id = $3)
           AND ($4::timestamptz IS NULL OR m.posted_at >= $4)
         ORDER BY m.posted_at DESC, m.ts DESC
         LIMIT $5"
    ))
    .bind(my_user_id)
    .bind(account)
    .bind(team_id)
    .bind(after)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    with_reactions(pool, messages).await
}

/// Everything in scope after a timestamp, oldest first.
pub async fn messages_since(
    pool: &PgPool,
    since: DateTime<Utc>,
    account: Option<&str>,
    team_id: Option<&str>,
    limit: i64,
) -> Result<Vec<Message>, sqlx::Error> {
    let messages = sqlx::query_as::<_, Message>(concat!(
        "SELECT ",
        message_columns!(),
        "
          FROM message m
          JOIN conversation c ON c.account_id = m.account_id AND c.id = m.conversation_id
         WHERE c.in_scope
           AND m.posted_at > $1
           AND ($2::text IS NULL OR m.account_id = $2)
           AND ($3::text IS NULL OR c.team_id = $3)
         ORDER BY m.posted_at ASC
         LIMIT $4"
    ))
    .bind(since)
    .bind(account)
    .bind(team_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    with_reactions(pool, messages).await
}

pub async fn list_revisions(
    pool: &PgPool,
    account_id: &str,
    conversation_id: &str,
    ts: &str,
) -> Result<Vec<Revision>, sqlx::Error> {
    sqlx::query_as::<_, Revision>(
        "
        SELECT seq, text, first_seen_at, superseded_at FROM message_revision
         WHERE account_id = $1 AND conversation_id = $2 AND ts = $3 ORDER BY seq
        ",
    )
    .bind(account_id)
    .bind(conversation_id)
    .bind(ts)
    .fetch_all(pool)
    .await
}

/// Find one message by the conversation name or id a caller actually typed.
pub async fn resolve_message(
    pool: &PgPool,
    conversation: &str,
    ts: &str,
    account: Option<&str>,
) -> Result<Option<ResolvedMessage>, sqlx::Error> {
    let row = sqlx::query(
        "
        SELECT m.account_id, m.conversation_id, m.deleted_at, m.edited_at
          FROM message m JOIN conversation c
            ON c.account_id = m.account_id AND c.id = m.conversation_id
         WHERE m.ts = $1
           AND ($2::text IS NULL OR m.account_id = $2)
           AND (c.id = $3 OR lower(c.name) = lower($3))
         LIMIT 1
        ",
    )
    .bind(ts)
    .bind(account)
    .bind(conversation)
    .fetch_optional(pool)
    .await?;
    row.map(|row| {
        Ok(ResolvedMessage {
            account_id: row.try_get("account_id")?,
            conversation_id: row.try_get("conversation_id")?,
            deleted_at: row.try_get("deleted_at")?,
            edited_at: row.try_get("edited_at")?,
        })
    })
    .transpose()
}

// ------------------------------------------------------------------------- the writes

/// Insert or refresh a batch of messages.
///
/// Idempotent by primary key, so a re-read page costs nothing. `raw` arrives already
/// stripped of `files` and `blocks` by the parsing layer; this function does not inspect
/// it.
///
/// `text` is deliberately absent from the UPDATE. Both loops re-read pages they have
/// already stored, and carrying an edit through here would change the archive without
/// recording that anything changed — the exact hole `message_revision` exists to close,
/// hidden inside the write that closes it. [`super::record_revision`] is the only writer
/// of `text` after the first insert, so it cannot move without the row that explains it.
/// `edited_at` still refreshes: that is Slack's own claim about the message, not ours,
/// and the two are allowed to disagree.
///
/// Takes a connection rather than a pool because the caller owns the transaction: a page
/// is written with its reactions and its mention threads or not at all.
pub async fn upsert_messages(
    conn: &mut PgConnection,
    messages: &[Message],
) -> Result<(), sqlx::Error> {
    for message in messages {
        sqlx::query(
            "
            INSERT INTO message (
              account_id, conversation_id, ts, thread_ts, user_id,
              text, subtype, edited_at, raw, posted_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            ON CONFLICT (account_id, conversation_id, ts) DO UPDATE SET
              thread_ts = EXCLUDED.thread_ts,
              user_id   = EXCLUDED.user_id,
              subtype   = EXCLUDED.subtype,
              edited_at = EXCLUDED.edited_at,
              raw       = EXCLUDED.raw,
              posted_at = EXCLUDED.posted_at
            ",
        )
        .bind(&message.account_id)
        .bind(&message.conversation_id)
        .bind(&message.ts)
        .bind(message.thread_ts.as_deref())
        .bind(message.user_id.as_deref())
        .bind(&message.text)
        .bind(message.subtype.as_deref())
        .bind(message.edited_at)
        // Bound as a `Value`, which sqlx encodes as jsonb itself. Encoding it to a
        // string here as well would store every payload as a JSON *string*: the decode
        // path is symmetric, so the application never notices and every jsonb query on
        // the column quietly stops matching.
        .bind(&message.raw)
        .bind(message.posted_at)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Make the stored reaction set for one message EQUAL the observed set.
///
/// Slack returns reactions as a snapshot on the message, not as events, so an absent
/// name means removed — not "no information". An empty `reactions` therefore clears
/// every row for the message rather than being treated as a no-op. Insert and delete run
/// in the caller's transaction so a reader never sees the message with neither set.
pub async fn replace_reactions(
    conn: &mut PgConnection,
    account_id: &str,
    conversation_id: &str,
    message_ts: &str,
    reactions: &[Reaction],
) -> Result<(), sqlx::Error> {
    for reaction in reactions {
        sqlx::query(
            "
            INSERT INTO reaction (
              account_id, conversation_id, message_ts, name, user_ids, count, observed_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, now())
            ON CONFLICT (account_id, conversation_id, message_ts, name) DO UPDATE SET
              user_ids    = EXCLUDED.user_ids,
              count       = EXCLUDED.count,
              observed_at = now()
            ",
        )
        .bind(account_id)
        .bind(conversation_id)
        .bind(message_ts)
        .bind(&reaction.name)
        .bind(&reaction.user_ids)
        .bind(reaction.count)
        .execute(&mut *conn)
        .await?;
    }
    let names: Vec<String> = reactions.iter().map(|r| r.name.clone()).collect();
    sqlx::query(
        "
        DELETE FROM reaction
         WHERE account_id = $1 AND conversation_id = $2 AND message_ts = $3
           AND name <> ALL($4::text[])
        ",
    )
    .bind(account_id)
    .bind(conversation_id)
    .bind(message_ts)
    .bind(&names)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Record a thread the user was mentioned in. `true` if newly registered.
///
/// The first mention wins: `mention_ts` is not overwritten, so the record keeps pointing
/// at the message that originally brought the thread into scope.
pub async fn register_mention_thread(
    conn: &mut PgConnection,
    account_id: &str,
    conversation_id: &str,
    thread_ts: &str,
    mention_ts: &str,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query(
        "
        INSERT INTO mention_thread (account_id, conversation_id, thread_ts, mention_ts)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (account_id, conversation_id, thread_ts) DO NOTHING
        RETURNING thread_ts
        ",
    )
    .bind(account_id)
    .bind(conversation_id)
    .bind(thread_ts)
    .bind(mention_ts)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.is_some())
}

/// Every `(conversation_id, thread_ts)` in scope for this account.
///
/// The ingester holds this in memory for the duration of a pass; it is the fourth rule
/// of the scope filter and must not require a query per message.
pub async fn list_mention_thread_keys(
    pool: &PgPool,
    account_id: &str,
) -> Result<HashSet<(String, String)>, sqlx::Error> {
    let rows =
        sqlx::query("SELECT conversation_id, thread_ts FROM mention_thread WHERE account_id = $1")
            .bind(account_id)
            .fetch_all(pool)
            .await?;
    rows.iter()
        .map(|row| Ok((row.try_get("conversation_id")?, row.try_get("thread_ts")?)))
        .collect()
}

/// `(conversation_id, thread_ts)` for parents holding fewer replies than they claim.
///
/// `conversations.history` never returns thread replies, so a parent arrives saying "12
/// replies" and nothing else does. This finds the ones still owing, so the poll loop can
/// drain them a bounded number at a time rather than in one burst.
///
/// Compares against `reply_count` rather than a "hydrated" flag: a thread that grows
/// after we fetched it reappears here by itself.
pub async fn list_unhydrated_threads(
    pool: &PgPool,
    account_id: &str,
    limit: i64,
) -> Result<Vec<(String, String)>, sqlx::Error> {
    let rows = sqlx::query(
        "
        SELECT m.conversation_id, m.ts
          FROM message m
          JOIN conversation c ON c.account_id = m.account_id AND c.id = m.conversation_id
         WHERE m.account_id = $1
           AND c.in_scope
           AND (m.raw->>'reply_count')::int > 0
           AND (m.raw->>'reply_count')::int > (
                 SELECT count(*) FROM message r
                  WHERE r.account_id = m.account_id
                    AND r.conversation_id = m.conversation_id
                    AND r.thread_ts = m.ts)
         ORDER BY m.posted_at DESC
         LIMIT $2
        ",
    )
    .bind(account_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| Ok((row.try_get("conversation_id")?, row.try_get("ts")?)))
        .collect()
}
