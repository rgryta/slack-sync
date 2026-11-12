//! `account` and `sync_state`: the ingester's own bookkeeping, and the status read over
//! it.

use chrono::{DateTime, Utc};
use sqlx::Row;
use sqlx::postgres::PgPool;

use crate::models::{Account, SyncLoop, SyncState, SyncStatus};

use super::parse_column;

/// Register the account the ingester is running as, or refresh what it reports.
///
/// `enabled` is written on the insert and never on the update, unlike the Python, which
/// assigned `EXCLUDED.enabled` — and every boot constructs an `Account` with the default
/// `enabled: true`, so a human who disabled an account by hand had it silently switched
/// back on by the next restart. Nothing in the code ever writes `false`, so the column is
/// a person's switch and the ingester has no business overwriting it.
pub async fn upsert_account(pool: &PgPool, account: &Account) -> Result<(), sqlx::Error> {
    sqlx::query(
        "
        INSERT INTO account (id, team_id, my_user_id, enabled, updated_at)
        VALUES ($1, $2, $3, $4, now())
        ON CONFLICT (id) DO UPDATE SET
          team_id    = EXCLUDED.team_id,
          my_user_id = EXCLUDED.my_user_id,
          updated_at = now()
        ",
    )
    .bind(&account.id)
    .bind(&account.team_id)
    .bind(&account.my_user_id)
    .bind(account.enabled)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record or clear a session failure. Clearing sets both columns NULL together.
///
/// A write that would not change the flag is skipped. Every successful poll pass calls
/// this to clear, so without the guard the account row churned once a cycle; and
/// re-recording an error that is already standing would push `last_auth_error_at`
/// forward on every pass, which turns the "failing since" that `status` prints into
/// "still failing as of", the one thing it must not mean.
pub async fn set_account_auth_error(
    pool: &PgPool,
    account_id: &str,
    error: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "
        UPDATE account
           SET last_auth_error    = $2,
               last_auth_error_at = CASE WHEN $2::text IS NULL THEN NULL ELSE now() END,
               updated_at         = now()
         WHERE id = $1
           AND last_auth_error IS DISTINCT FROM $2::text
        ",
    )
    .bind(account_id)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record where a loop is, leaving any recorded error alone.
///
/// Separate from [`set_sync_state`] because progress and failure are written at
/// different moments: a later successful step must not erase the record of an earlier
/// gap by writing a NULL error over it.
pub async fn set_sync_progress(
    pool: &PgPool,
    account_id: &str,
    r#loop: SyncLoop,
    current_conversation_id: Option<&str>,
    last_run_at: Option<DateTime<Utc>>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "
        INSERT INTO sync_state (account_id, loop, current_conversation_id, last_run_at)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (account_id, loop) DO UPDATE SET
          current_conversation_id = EXCLUDED.current_conversation_id,
          last_run_at             = COALESCE(EXCLUDED.last_run_at, sync_state.last_run_at)
        ",
    )
    .bind(account_id)
    .bind(r#loop.as_str())
    .bind(current_conversation_id)
    .bind(last_run_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record a loop's whole state, error included.
///
/// `last_run_at` is COALESCEd: a pass that fails before running must not erase the last
/// time the loop actually ran.
pub async fn set_sync_state(pool: &PgPool, state: &SyncState) -> Result<(), sqlx::Error> {
    sqlx::query(
        "
        INSERT INTO sync_state (
          account_id, loop, current_conversation_id, last_run_at, last_error, last_error_at
        )
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (account_id, loop) DO UPDATE SET
          current_conversation_id = EXCLUDED.current_conversation_id,
          last_run_at             = COALESCE(EXCLUDED.last_run_at, sync_state.last_run_at),
          last_error              = EXCLUDED.last_error,
          last_error_at           = EXCLUDED.last_error_at
        ",
    )
    .bind(&state.account_id)
    .bind(state.r#loop.as_str())
    .bind(state.current_conversation_id.as_deref())
    .bind(state.last_run_at)
    .bind(state.last_error.as_deref())
    .bind(state.last_error_at)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_sync_state(
    pool: &PgPool,
    account_id: &str,
) -> Result<Vec<SyncState>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT account_id, loop, current_conversation_id, last_run_at, last_error, \
         last_error_at FROM sync_state WHERE account_id = $1 ORDER BY loop",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            let r#loop: String = row.try_get("loop")?;
            Ok(SyncState {
                account_id: row.try_get("account_id")?,
                r#loop: parse_column::<SyncLoop>(&r#loop)?,
                current_conversation_id: row.try_get("current_conversation_id")?,
                last_run_at: row.try_get("last_run_at")?,
                last_error: row.try_get("last_error")?,
                last_error_at: row.try_get("last_error_at")?,
            })
        })
        .collect()
}

/// Every ingesting account. `mentions` needs each one's own user id: the token to
/// match is `<@their-id>`, which differs per account.
pub async fn list_accounts(pool: &PgPool) -> Result<Vec<Account>, sqlx::Error> {
    sqlx::query_as::<_, Account>(
        "SELECT id, team_id, my_user_id, enabled, last_auth_error, last_auth_error_at, \
         updated_at FROM account ORDER BY id",
    )
    .fetch_all(pool)
    .await
}

/// Per-account freshness: backfill progress, poll times and the last auth error.
pub async fn sync_status(
    pool: &PgPool,
    account: Option<&str>,
) -> Result<Vec<SyncStatus>, sqlx::Error> {
    let rows = sqlx::query(
        "
        SELECT a.id AS account_id, a.team_id, a.enabled,
               a.last_auth_error, a.last_auth_error_at,
               count(c.id) FILTER (WHERE c.in_scope)                        AS in_scope_count,
               count(c.id) FILTER (WHERE c.in_scope AND c.backfill_done)    AS backfilled_count,
               max(c.last_polled_at)                                        AS last_polled_at,
               min(c.reactions_fresh_since) FILTER (WHERE c.in_scope)       AS reactions_fresh_since
          FROM account a
          LEFT JOIN conversation c ON c.account_id = a.id
         WHERE ($1::text IS NULL OR a.id = $1)
         GROUP BY a.id, a.team_id, a.enabled, a.last_auth_error, a.last_auth_error_at
         ORDER BY a.id
        ",
    )
    .bind(account)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(SyncStatus {
                account_id: row.try_get("account_id")?,
                team_id: row.try_get("team_id")?,
                enabled: row.try_get("enabled")?,
                last_auth_error: row.try_get("last_auth_error")?,
                last_auth_error_at: row.try_get("last_auth_error_at")?,
                in_scope_count: row.try_get("in_scope_count")?,
                backfilled_count: row.try_get("backfilled_count")?,
                last_polled_at: row.try_get("last_polled_at")?,
                reactions_fresh_since: row.try_get("reactions_fresh_since")?,
            })
        })
        .collect()
}
