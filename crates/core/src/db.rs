//! Connection pool and forward-only schema application.
//!
//! The helmsman pattern: every `db-migrations/pg/*.sql` runs in filename order and
//! applied filenames are recorded in `schema_migration`. Chosen over Alembic for the
//! Python original because the schema is small, the migration path is forward-only,
//! and the MCP process must never hold DDL rights.
//!
//! `sqlx::migrate!` cannot be used here. It wants its own `_sqlx_migrations` table
//! keyed by version number and checksum; ours is `schema_migration(filename,
//! applied_at)` and the live database already holds applied rows in that shape.
//! Worse, the macro would `CREATE TABLE` on startup, and the MCP role deliberately
//! has no DDL rights. So the applier is hand-written against the same table, and
//! [`assert_migrations`] reads and never writes.

use std::time::Duration;

use sqlx::postgres::{PgPool, PgPoolOptions};

/// The shipped migrations, embedded rather than read from disk: the binary must not
/// depend on the repository being mounted next to it, and a scratch image has no
/// `db-migrations/` to read. `migrations_match_the_directory` fails if a file is added
/// here and not to this list, or the other way round.
pub const MIGRATIONS: &[(&str, &str)] = &[
    (
        "0001_init.sql",
        include_str!("../../../db-migrations/pg/0001_init.sql"),
    ),
    (
        "0002_repair_raw_and_index_threads.sql",
        include_str!("../../../db-migrations/pg/0002_repair_raw_and_index_threads.sql"),
    ),
    (
        "0003_place_on_tablespaces.sql",
        include_str!("../../../db-migrations/pg/0003_place_on_tablespaces.sql"),
    ),
    (
        "0004_dm_counterpart.sql",
        include_str!("../../../db-migrations/pg/0004_dm_counterpart.sql"),
    ),
    (
        "0005_user_display_name.sql",
        include_str!("../../../db-migrations/pg/0005_user_display_name.sql"),
    ),
    (
        "0006_message_revisions.sql",
        include_str!("../../../db-migrations/pg/0006_message_revisions.sql"),
    ),
];

const SCHEMA_MIGRATION_DDL: &str = "
CREATE TABLE IF NOT EXISTS schema_migration (
  filename   text PRIMARY KEY,
  applied_at timestamptz NOT NULL DEFAULT now()
)
";

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database is missing migrations: {0}")]
    MigrationsMissing(String),
    #[error(transparent)]
    Sql(#[from] sqlx::Error),
}

/// A pool sized like the Python one: `min_size=1, max_size=8`.
pub async fn create_pool(dsn: &str) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .min_connections(1)
        .max_connections(8)
        .acquire_timeout(Duration::from_secs(30))
        .connect(dsn)
        .await
}

/// Apply every not-yet-applied migration, returning the filenames applied now.
///
/// Idempotent: a second call applies nothing and leaves one `schema_migration` row per
/// file. The whole pass runs in one transaction, so a failure part-way leaves neither
/// the DDL nor the rows that would claim it had been applied.
///
/// Called only by the ingester and by the test harness. The MCP process calls
/// [`assert_migrations`].
pub async fn ensure_schema(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    let mut applied_now = Vec::new();
    let mut tx = pool.begin().await?;
    sqlx::query(SCHEMA_MIGRATION_DDL).execute(&mut *tx).await?;
    for (filename, sql) in MIGRATIONS {
        // The primary key is the concurrency guard: a second booter's INSERT blocks on
        // the first, then returns no row, and we skip the DDL rather than running it
        // twice into an "already exists" failure.
        let inserted: Option<String> = sqlx::query_scalar(
            "INSERT INTO schema_migration (filename) VALUES ($1) \
             ON CONFLICT (filename) DO NOTHING RETURNING filename",
        )
        .bind(filename)
        .fetch_optional(&mut *tx)
        .await?;
        if inserted.is_none() {
            continue;
        }
        // `*sql` to hand sqlx a `&'static str`: its `SqlSafeStr` bound is what refuses
        // a runtime-built query, and keeping that refusal is the point.
        sqlx::raw_sql(*sql).execute(&mut *tx).await?;
        applied_now.push((*filename).to_owned());
    }
    tx.commit().await?;
    Ok(applied_now)
}

/// Filenames recorded in `schema_migration`, in filename order.
///
/// Returns an empty list when the table does not exist yet — the MCP process must be
/// able to report "no schema" instead of crashing on a missing relation.
pub async fn applied_migrations(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    let exists: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('public.schema_migration')::text")
            .fetch_one(pool)
            .await?;
    if exists.is_none() {
        return Ok(Vec::new());
    }
    sqlx::query_scalar("SELECT filename FROM schema_migration ORDER BY filename")
        .fetch_all(pool)
        .await
}

/// Read-only check that every shipped migration is present. Never applies DDL.
pub async fn assert_migrations(pool: &PgPool) -> Result<(), DbError> {
    let present = applied_migrations(pool).await?;
    let missing: Vec<&str> = MIGRATIONS
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| !present.iter().any(|p| p == name))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(DbError::MigrationsMissing(missing.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_match_the_directory() {
        // The list is embedded, so nothing at runtime would notice a seventh migration
        // file that was never added to it. This does.
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../db-migrations/pg");
        let mut on_disk: Vec<String> = std::fs::read_dir(dir)
            .expect("db-migrations/pg is missing")
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".sql"))
            .collect();
        on_disk.sort();
        let embedded: Vec<String> = MIGRATIONS.iter().map(|(n, _)| (*n).to_owned()).collect();
        assert_eq!(embedded, on_disk);
    }

    #[test]
    fn migrations_are_embedded_in_filename_order() {
        let mut sorted = MIGRATIONS.to_vec();
        sorted.sort_by_key(|(name, _)| *name);
        assert_eq!(sorted, MIGRATIONS.to_vec());
    }
}
