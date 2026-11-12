//! The alias tables — the MCP role's only write authority.

use sqlx::Row;
use sqlx::postgres::PgPool;

use crate::models::{Alias, Identity};

/// Resolve an alias or a raw user ID to the identity set it names.
///
/// An alias spanning two accounts returns both identities, which is what makes
/// `author: "ted"` mean "Ted anywhere" rather than "Ted in one workspace". An
/// unrecognised string is matched against the directory by name, because the name on
/// screen is what a caller has to go on — being told "Sandy Ganoti wrote this" and
/// then having `author: "Sandy Ganoti"` return nothing is a dead end with no next
/// move. That match is whole-string and case-insensitive rather than a substring, so
/// it cannot quietly widen to everyone sharing a first name.
pub async fn resolve_author(pool: &PgPool, author: &str) -> Result<Vec<Identity>, sqlx::Error> {
    let by_alias = sqlx::query_as::<_, Identity>(
        "SELECT account_id, user_id FROM user_alias_member WHERE alias = $1",
    )
    .bind(author)
    .fetch_all(pool)
    .await?;
    if !by_alias.is_empty() {
        return Ok(by_alias);
    }
    let by_id = sqlx::query_as::<_, Identity>(
        "SELECT account_id, user_id FROM workspace_user WHERE user_id = $1",
    )
    .bind(author)
    .fetch_all(pool)
    .await?;
    if !by_id.is_empty() {
        return Ok(by_id);
    }
    sqlx::query_as::<_, Identity>(
        "
        SELECT account_id, user_id FROM workspace_user
         WHERE lower($1) IN (lower(display_name), lower(real_name), lower(name))
         ORDER BY account_id, user_id
        ",
    )
    .bind(author)
    .fetch_all(pool)
    .await
}

pub async fn list_aliases(pool: &PgPool, alias: Option<&str>) -> Result<Vec<Alias>, sqlx::Error> {
    let rows = sqlx::query(
        "
        SELECT a.alias, a.note, m.account_id, m.user_id
          FROM user_alias a
          LEFT JOIN user_alias_member m ON m.alias = a.alias
         WHERE ($1::text IS NULL OR a.alias = $1)
         ORDER BY a.alias, m.account_id, m.user_id
        ",
    )
    .bind(alias)
    .fetch_all(pool)
    .await?;

    // Ordered by alias, so the rows for one alias arrive together and the last entry
    // is always the one to extend.
    let mut aliases: Vec<Alias> = Vec::new();
    for row in rows {
        let name: String = row.try_get("alias")?;
        if aliases.last().is_none_or(|last| last.alias != name) {
            aliases.push(Alias {
                alias: name,
                note: row.try_get("note")?,
                members: Vec::new(),
            });
        }
        let account_id: Option<String> = row.try_get("account_id")?;
        if let Some(account_id) = account_id {
            aliases
                .last_mut()
                .expect("just pushed")
                .members
                .push(Identity {
                    account_id,
                    user_id: row.try_get("user_id")?,
                });
        }
    }
    Ok(aliases)
}

/// Create or replace an alias's member set. Returns identities moved off another
/// alias.
///
/// Each Slack identity belongs to at most one alias, so attaching one that is already
/// attached elsewhere is a move. The moved identities are returned rather than
/// silently reassigned, so the caller can say so out loud.
pub async fn set_alias(
    pool: &PgPool,
    alias: &str,
    members: &[Identity],
    note: Option<&str>,
) -> Result<Vec<Identity>, sqlx::Error> {
    let accounts: Vec<String> = members.iter().map(|m| m.account_id.clone()).collect();
    let users: Vec<String> = members.iter().map(|m| m.user_id.clone()).collect();

    let mut tx = pool.begin().await?;
    sqlx::query(
        "
        INSERT INTO user_alias (alias, note) VALUES ($1, $2)
        ON CONFLICT (alias) DO UPDATE SET
          note = COALESCE(EXCLUDED.note, user_alias.note),
          updated_at = now()
        ",
    )
    .bind(alias)
    .bind(note)
    .execute(&mut *tx)
    .await?;

    let moved = if members.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as::<_, Identity>(
            "
            SELECT account_id, user_id FROM user_alias_member
             WHERE alias <> $1
               AND (account_id, user_id)
                   IN (SELECT * FROM unnest($2::text[], $3::text[]))
            ",
        )
        .bind(alias)
        .bind(&accounts)
        .bind(&users)
        .fetch_all(&mut *tx)
        .await?
    };

    sqlx::query("DELETE FROM user_alias_member WHERE alias = $1")
        .bind(alias)
        .execute(&mut *tx)
        .await?;

    if members.is_empty() {
        // An alias with no members is an alias that resolves to nothing; deleting it
        // is more honest than leaving a name that matches no one.
        sqlx::query("DELETE FROM user_alias WHERE alias = $1")
            .bind(alias)
            .execute(&mut *tx)
            .await?;
    } else {
        for member in members {
            sqlx::query(
                "
                INSERT INTO user_alias_member (account_id, user_id, alias)
                VALUES ($1, $2, $3)
                ON CONFLICT (account_id, user_id) DO UPDATE SET alias = EXCLUDED.alias
                ",
            )
            .bind(&member.account_id)
            .bind(&member.user_id)
            .bind(alias)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    Ok(moved)
}
