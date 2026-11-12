//! The edit-tracking machinery: what a message used to say, and whether it is gone.
//!
//! A bug in here loses data silently rather than loudly — an edit that goes unrecorded
//! overwrites the only copy of the old text, and a false deletion hides a real message
//! from every default view — so the statements are kept together and the invariants are
//! written down beside them.
//!
//! The read side of the same tables (`list_revisions`, `resolve_message`) stays in
//! `messages.rs`, with the other reads the MCP server makes.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::Row;
use sqlx::postgres::PgPool;

/// Whether a re-read is worth a write. Public because the callers need it.
///
/// [`record_revision`] is safe to call unconditionally and returns `false` when nothing
/// changed — but it takes a connection and a row lock to find that out, and in steady
/// state the answer is `false` for every message in every window on every cycle. The
/// loops already hold the stored text, so the cheap comparison happens there and the
/// database is touched only when something actually moved.
pub fn text_differs(stored: Option<&str>, observed: Option<&str>) -> bool {
    !same_text(stored, observed)
}

/// NULL and `''` are the same absence of text, and neither is an edit.
///
/// Messages with only attachments carry no text. Treating NULL as different from `''`
/// would score every one of them as edited on every refresh pass and fill the revision
/// table with rows recording nothing.
fn same_text(a: Option<&str>, b: Option<&str>) -> bool {
    a.unwrap_or_default() == b.unwrap_or_default()
}

/// ts -> text for one conversation's messages since `since`.
///
/// Ranged on `posted_at`, not `ts`: `ts` is an opaque Slack identifier stored as text,
/// and comparing it as a date would be a string comparison wearing a timestamp's
/// clothes.
///
/// Deleted rows are included, so a message that comes back is recognisable as one we
/// already had rather than looking like a message we never held.
///
/// **Thread replies are excluded, and that is load-bearing.**
/// `conversations.history` never returns them — only `conversations.replies` does — so a
/// reply is absent from the refresh window by construction, not by deletion. Including
/// them made the first live pass suspect 138 real messages, every one of them a reply,
/// and a second pass would have marked them deleted and hidden them from every default
/// view.
///
/// The rule this encodes: a message may only be judged missing by a channel that could
/// have returned it. Reply deletion is consequently not detected at all, which is an
/// honest gap rather than a wrong answer.
pub async fn stored_window(
    pool: &PgPool,
    account_id: &str,
    conversation_id: &str,
    since: DateTime<Utc>,
) -> Result<HashMap<String, Option<String>>, sqlx::Error> {
    let rows = sqlx::query(
        "
        SELECT ts, text FROM message
         WHERE account_id = $1 AND conversation_id = $2 AND posted_at >= $3
           AND (thread_ts IS NULL OR thread_ts = ts)
        ",
    )
    .bind(account_id)
    .bind(conversation_id)
    .bind(since)
    .fetch_all(pool)
    .await?;
    texts(&rows)
}

/// The stored text for a page's messages: ts -> text, absent when not held.
///
/// One query per page instead of a connection and a row lock per message. The page
/// writer runs on every page of both loops, and in steady state every message on it is
/// unchanged, so the difference is the whole cost of the feature.
pub async fn stored_texts(
    pool: &PgPool,
    account_id: &str,
    conversation_id: &str,
    timestamps: &[String],
) -> Result<HashMap<String, Option<String>>, sqlx::Error> {
    if timestamps.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query(
        "
        SELECT ts, text FROM message
         WHERE account_id = $1 AND conversation_id = $2 AND ts = ANY($3::text[])
        ",
    )
    .bind(account_id)
    .bind(conversation_id)
    .bind(timestamps)
    .fetch_all(pool)
    .await?;
    texts(&rows)
}

/// Everything the archive holds for one thread: ts -> text, parent included.
///
/// Unlike [`stored_window`] this has no time bound, because the channel that answers it
/// has none either: `conversations.replies` follows its cursor to the end, so a thread
/// fetch observes the whole thread and absence from it means something.
pub async fn stored_thread(
    pool: &PgPool,
    account_id: &str,
    conversation_id: &str,
    thread_ts: &str,
) -> Result<HashMap<String, Option<String>>, sqlx::Error> {
    let rows = sqlx::query(
        "
        SELECT ts, text FROM message
         WHERE account_id = $1 AND conversation_id = $2
           AND (thread_ts = $3 OR ts = $3)
        ",
    )
    .bind(account_id)
    .bind(conversation_id)
    .bind(thread_ts)
    .fetch_all(pool)
    .await?;
    texts(&rows)
}

fn texts(rows: &[sqlx::postgres::PgRow]) -> Result<HashMap<String, Option<String>>, sqlx::Error> {
    rows.iter()
        .map(|row| Ok((row.try_get("ts")?, row.try_get("text")?)))
        .collect()
}

/// Record that a stored message now reads differently, and update it. Idempotent.
///
/// One transaction over a locked message row. The lock matters twice: `seq` is allocated
/// as max+1 and the poll and backfill loops can both reach one message in a pass, and a
/// crash between the revision insert and the text update would leave the log
/// contradicting the message it describes.
///
/// Returns whether anything was written; an unchanged re-read is the common case by
/// orders of magnitude and must cost nothing.
pub async fn record_revision(
    pool: &PgPool,
    account_id: &str,
    conversation_id: &str,
    ts: &str,
    text: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let current: Option<Option<String>> = sqlx::query(
        "
        SELECT text FROM message
         WHERE account_id = $1 AND conversation_id = $2 AND ts = $3 FOR UPDATE
        ",
    )
    .bind(account_id)
    .bind(conversation_id)
    .bind(ts)
    .fetch_optional(&mut *tx)
    .await?
    .map(|row| row.try_get("text"))
    .transpose()?;

    // Not ours to revise. A loop reaching ground it has never covered is storing a
    // message for the first time, and calling that an edit would invent a history that
    // never happened.
    let Some(stored) = current else {
        return Ok(false);
    };
    if same_text(stored.as_deref(), text) {
        return Ok(false);
    }

    let mut seq: i32 = sqlx::query(
        "
        SELECT coalesce(max(seq), 0) AS seq FROM message_revision
         WHERE account_id = $1 AND conversation_id = $2 AND ts = $3
        ",
    )
    .bind(account_id)
    .bind(conversation_id)
    .bind(ts)
    .fetch_one(&mut *tx)
    .await?
    .try_get("seq")?;

    if seq == 0 {
        // The text being replaced has never been recorded, so it becomes seq 1 — with a
        // NULL `first_seen_at`, because nothing knows when it was first stored.
        sqlx::query(
            "
            INSERT INTO message_revision
                   (account_id, conversation_id, ts, seq, text, first_seen_at, superseded_at)
            VALUES ($1, $2, $3, 1, $4::text, NULL, now())
            ",
        )
        .bind(account_id)
        .bind(conversation_id)
        .bind(ts)
        .bind(stored.as_deref())
        .execute(&mut *tx)
        .await?;
        seq = 1;
    } else {
        sqlx::query(
            "
            UPDATE message_revision SET superseded_at = now()
             WHERE account_id = $1 AND conversation_id = $2 AND ts = $3
               AND superseded_at IS NULL
            ",
        )
        .bind(account_id)
        .bind(conversation_id)
        .bind(ts)
        .execute(&mut *tx)
        .await?;
    }

    sqlx::query(
        "
        INSERT INTO message_revision
               (account_id, conversation_id, ts, seq, text, first_seen_at, superseded_at)
        VALUES ($1, $2, $3, $4, $5::text, now(), NULL)
        ",
    )
    .bind(account_id)
    .bind(conversation_id)
    .bind(ts)
    .bind(seq + 1)
    .bind(text)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "
        UPDATE message SET text = $4::text
         WHERE account_id = $1 AND conversation_id = $2 AND ts = $3
        ",
    )
    .bind(account_id)
    .bind(conversation_id)
    .bind(ts)
    .bind(text)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(true)
}

/// A message the source still returns is neither missing nor deleted.
///
/// Clearing `deleted_at` as well is deliberate: if a message we called deleted comes
/// back, we were wrong, and withdrawing the claim beats keeping a tombstone the evidence
/// no longer supports.
pub async fn mark_present(
    pool: &PgPool,
    account_id: &str,
    conversation_id: &str,
    timestamps: &[String],
) -> Result<u64, sqlx::Error> {
    if timestamps.is_empty() {
        return Ok(0);
    }
    let result = sqlx::query(
        "
        UPDATE message SET missing_since = NULL, deleted_at = NULL
         WHERE account_id = $1 AND conversation_id = $2 AND ts = ANY($3::text[])
           AND (missing_since IS NOT NULL OR deleted_at IS NOT NULL)
        ",
    )
    .bind(account_id)
    .bind(conversation_id)
    .bind(timestamps)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Two strikes. The first clean window that misses a message stamps `missing_since`; the
/// second promotes it to `deleted_at`. Rows are never removed — a deletion is itself
/// history.
///
/// An absence is not evidence on its own. A short page, a dropped connection and a real
/// deletion are indistinguishable from here, and `deleted_at` hides the message from
/// every default view, so a false positive quietly removes real history.
///
/// Returns how many were promoted on this call, counted from RETURNING rather than the
/// row count, which would also count first strikes.
pub async fn mark_absent(
    pool: &PgPool,
    account_id: &str,
    conversation_id: &str,
    timestamps: &[String],
) -> Result<u64, sqlx::Error> {
    if timestamps.is_empty() {
        return Ok(0);
    }
    let rows = sqlx::query(
        "
        UPDATE message
           SET deleted_at = CASE WHEN missing_since IS NOT NULL THEN now() ELSE deleted_at END,
               missing_since = coalesce(missing_since, now())
         WHERE account_id = $1 AND conversation_id = $2 AND ts = ANY($3::text[])
           AND deleted_at IS NULL
        RETURNING deleted_at
        ",
    )
    .bind(account_id)
    .bind(conversation_id)
    .bind(timestamps)
    .fetch_all(pool)
    .await?;

    let mut promoted = 0;
    for row in &rows {
        let deleted_at: Option<DateTime<Utc>> = row.try_get("deleted_at")?;
        if deleted_at.is_some() {
            promoted += 1;
        }
    }
    Ok(promoted)
}

/// Drop a one-strike absence once the message has aged out of the refresh window.
///
/// Nothing will look at it again, so corroboration is permanently impossible. Promoting
/// instead would turn "we stopped being able to check" into "it was deleted" — a claim
/// nothing downstream could ever discover was false.
pub async fn release_stranded(
    pool: &PgPool,
    account_id: &str,
    conversation_id: &str,
    before: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "
        UPDATE message SET missing_since = NULL
         WHERE account_id = $1 AND conversation_id = $2 AND posted_at < $3
           AND missing_since IS NOT NULL AND deleted_at IS NULL
        ",
    )
    .bind(account_id)
    .bind(conversation_id)
    .bind(before)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_and_empty_text_are_the_same_absence() {
        // A message with only attachments carries no text. Scoring that as an edit on
        // every pass would fill the revision table with rows recording nothing.
        assert!(!text_differs(None, Some("")));
        assert!(!text_differs(Some(""), None));
        assert!(!text_differs(None, None));
        assert!(!text_differs(Some("same"), Some("same")));
        assert!(text_differs(Some("before"), Some("after")));
        assert!(text_differs(None, Some("appeared")));
        assert!(text_differs(Some("vanished"), None));
    }
}
