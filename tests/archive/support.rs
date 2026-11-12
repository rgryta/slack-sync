//! Test fixtures.
//!
//! Postgres tests run against a throwaway testcontainer and report a loud skip when
//! Docker is unreachable. They must never reach
//! the homelab database: **nothing here reads a DSN from the environment**, the
//! container builds its own. That is the structural half of "never point tests at
//! a real database", and it is why `#[sqlx::test]` is not used anywhere — that macro
//! wants `DATABASE_URL` from the environment and creates databases itself, which
//! re-opens the door this closes.

use chrono::{DateTime, TimeZone, Utc};
use slack_core::db::{create_pool, ensure_schema};
use slack_core::models::{Conversation, Message};
use sqlx::postgres::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt, ReuseDirective};
use tokio::sync::{Mutex, OnceCell};

/// Every table `0001_init.sql` creates, minus `schema_migration`: truncating that would
/// make the shared schema look unapplied to the next test. `message_revision` goes with
/// `message` through the CASCADE.
const TRUNCATE_ALL: &str = "TRUNCATE reaction, message, mention_thread, sync_state, \
                            conversation, workspace_user, user_alias_member, user_alias, emoji, \
                            account, workspace CASCADE";

/// One named container serves every run. It is parked in a `static`, which Rust never
/// drops, so it is marked for reuse rather than reaping: the next run attaches to the
/// same container instead of leaving a second one behind. Remove it with
/// `docker rm -f slack-sync-test-pg` when you want the disk back.
const CONTAINER_NAME: &str = "slack-sync-test-pg";

static CONTAINER: OnceCell<Option<(ContainerAsync<Postgres>, String)>> = OnceCell::const_new();
/// One database, many tests, and each one truncates: they have to take turns.
static TURN: Mutex<()> = Mutex::const_new(());

pub struct Fixture {
    pub pool: PgPool,
    _turn: tokio::sync::MutexGuard<'static, ()>,
}

/// A pool over an empty database, or `None` when Docker is unreachable.
///
/// Returning `None` rather than failing is deliberate: Postgres tests skip when Docker is
/// not there. Rust has no "skipped" outcome, so each caller prints the reason and
/// returns — loudly enough to notice in a CI log.
pub async fn fixture() -> Option<Fixture> {
    let turn = TURN.lock().await;
    let dsn = CONTAINER
        .get_or_init(|| async {
            let request = Postgres::default()
                .with_tag("18-alpine")
                .with_container_name(CONTAINER_NAME)
                .with_reuse(ReuseDirective::Always);
            match request.start().await {
                Ok(container) => {
                    let host = container.get_host().await.ok()?;
                    let port = container.get_host_port_ipv4(5432).await.ok()?;
                    let dsn = format!("postgresql://postgres:postgres@{host}:{port}/postgres");
                    let pool = create_pool(&dsn).await.ok()?;
                    ensure_schema(&pool).await.ok()?;
                    pool.close().await;
                    Some((container, dsn))
                }
                Err(error) => {
                    eprintln!("SKIPPING the postgres tests: docker is not reachable: {error}");
                    None
                }
            }
        })
        .await
        .as_ref()
        .map(|(_, dsn)| dsn.clone())?;

    let pool = create_pool(&dsn).await.expect("the container is up");
    sqlx::query(TRUNCATE_ALL)
        .execute(&pool)
        .await
        .expect("truncate");
    Some(Fixture { pool, _turn: turn })
}

/// Skip the body with a printed reason when Docker is unreachable.
macro_rules! db_test {
    ($name:ident, |$fixture:ident| $body:block) => {
        #[tokio::test]
        async fn $name() {
            let Some($fixture) = crate::support::fixture().await else {
                return;
            };
            $body
        }
    };
}
pub(crate) use db_test;

/// A fixed instant, so an assertion about ordering never depends on how fast the test
/// ran.
pub fn base() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 1, 12, 0, 0).unwrap()
}

pub async fn workspace(pool: &PgPool, team_id: &str, name: &str, domain: &str) {
    sqlx::query(
        "INSERT INTO workspace (team_id, name, domain) VALUES ($1, $2, $3) \
         ON CONFLICT (team_id) DO NOTHING",
    )
    .bind(team_id)
    .bind(name)
    .bind(domain)
    .execute(pool)
    .await
    .expect("insert workspace");
}

pub async fn account(pool: &PgPool, id: &str, team_id: &str, my_user_id: &str) {
    sqlx::query(
        "INSERT INTO account (id, team_id, my_user_id) VALUES ($1, $2, $3) \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(id)
    .bind(team_id)
    .bind(my_user_id)
    .execute(pool)
    .await
    .expect("insert account");
}

pub struct Conv<'a> {
    pub account: &'a str,
    pub id: &'a str,
    pub team_id: &'a str,
    pub kind: &'a str,
    pub name: Option<&'a str>,
    pub in_scope: bool,
    pub scope_reason: Option<&'a str>,
    pub counterpart_user_id: Option<&'a str>,
    pub backfill_done: bool,
    pub last_polled_at: Option<DateTime<Utc>>,
    pub reactions_fresh_since: Option<DateTime<Utc>>,
}

impl Default for Conv<'_> {
    fn default() -> Self {
        Self {
            account: "work",
            id: "C1",
            team_id: "T1",
            kind: "channel",
            name: Some("general"),
            in_scope: true,
            scope_reason: Some("named_channel"),
            counterpart_user_id: None,
            backfill_done: false,
            last_polled_at: None,
            reactions_fresh_since: None,
        }
    }
}

pub async fn conversation(pool: &PgPool, conv: Conv<'_>) {
    sqlx::query(
        "INSERT INTO conversation (account_id, id, team_id, type, name, is_member, in_scope, \
         scope_reason, counterpart_user_id, backfill_done, last_polled_at, reactions_fresh_since) \
         VALUES ($1, $2, $3, $4, $5, true, $6, $7, $8, $9, $10, $11) \
         ON CONFLICT (account_id, id) DO UPDATE SET \
           name = EXCLUDED.name, in_scope = EXCLUDED.in_scope, \
           scope_reason = EXCLUDED.scope_reason, \
           counterpart_user_id = EXCLUDED.counterpart_user_id, \
           backfill_done = EXCLUDED.backfill_done, \
           last_polled_at = EXCLUDED.last_polled_at, \
           reactions_fresh_since = EXCLUDED.reactions_fresh_since",
    )
    .bind(conv.account)
    .bind(conv.id)
    .bind(conv.team_id)
    .bind(conv.kind)
    .bind(conv.name)
    .bind(conv.in_scope)
    .bind(conv.scope_reason)
    .bind(conv.counterpart_user_id)
    .bind(conv.backfill_done)
    .bind(conv.last_polled_at)
    .bind(conv.reactions_fresh_since)
    .execute(pool)
    .await
    .expect("insert conversation");
}

/// The minimum parent chain: a conversation cannot exist without its account.
pub async fn seed(pool: &PgPool) {
    workspace(pool, "T1", "team-T1", "t1").await;
    account(pool, "work", "T1", "UME").await;
    conversation(pool, Conv::default()).await;
}

pub struct Msg<'a> {
    pub ts: &'a str,
    pub account: &'a str,
    pub conversation: &'a str,
    pub user_id: Option<&'a str>,
    pub text: &'a str,
    pub thread_ts: Option<&'a str>,
    pub subtype: Option<&'a str>,
    pub offset_seconds: i64,
    pub edited_at: Option<DateTime<Utc>>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub raw: serde_json::Value,
}

impl Default for Msg<'_> {
    fn default() -> Self {
        Self {
            ts: "1.0",
            account: "work",
            conversation: "C1",
            user_id: Some("U1"),
            text: "hello",
            thread_ts: None,
            subtype: None,
            offset_seconds: 0,
            edited_at: None,
            deleted_at: None,
            raw: serde_json::json!({}),
        }
    }
}

pub async fn message(pool: &PgPool, msg: Msg<'_>) {
    sqlx::query(
        "INSERT INTO message (account_id, conversation_id, ts, thread_ts, user_id, text, \
         subtype, edited_at, deleted_at, raw, posted_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
         ON CONFLICT (account_id, conversation_id, ts) DO UPDATE SET text = EXCLUDED.text",
    )
    .bind(msg.account)
    .bind(msg.conversation)
    .bind(msg.ts)
    .bind(msg.thread_ts)
    .bind(msg.user_id)
    .bind(msg.text)
    .bind(msg.subtype)
    .bind(msg.edited_at)
    .bind(msg.deleted_at)
    .bind(&msg.raw)
    .bind(base() + chrono::TimeDelta::seconds(msg.offset_seconds))
    .execute(pool)
    .await
    .expect("insert message");
}

pub async fn messages(pool: &PgPool, batch: Vec<Msg<'_>>) {
    for msg in batch {
        message(pool, msg).await;
    }
}

pub async fn reaction(
    pool: &PgPool,
    account: &str,
    conversation: &str,
    ts: &str,
    name: &str,
    user_ids: &[&str],
    count: i32,
) {
    let user_ids: Vec<String> = user_ids.iter().map(|id| (*id).to_owned()).collect();
    sqlx::query(
        "INSERT INTO reaction (account_id, conversation_id, message_ts, name, user_ids, count) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(account)
    .bind(conversation)
    .bind(ts)
    .bind(name)
    .bind(&user_ids)
    .bind(count)
    .execute(pool)
    .await
    .expect("insert reaction");
}

pub async fn user(
    pool: &PgPool,
    account: &str,
    user_id: &str,
    name: Option<&str>,
    real_name: Option<&str>,
    display_name: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO workspace_user (account_id, user_id, name, real_name, display_name) \
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT (account_id, user_id) DO UPDATE SET \
           name = EXCLUDED.name, real_name = EXCLUDED.real_name, \
           display_name = EXCLUDED.display_name",
    )
    .bind(account)
    .bind(user_id)
    .bind(name)
    .bind(real_name)
    .bind(display_name)
    .execute(pool)
    .await
    .expect("insert workspace_user");
}

pub async fn emoji(pool: &PgPool, team_id: &str, name: &str) {
    sqlx::query("INSERT INTO emoji (team_id, name) VALUES ($1, $2)")
        .bind(team_id)
        .bind(name)
        .execute(pool)
        .await
        .expect("insert emoji");
}

#[derive(Clone, Copy)]
pub struct Rev<'a> {
    pub account: &'a str,
    pub conversation: &'a str,
    pub ts: &'a str,
    pub seq: i32,
    pub text: &'a str,
    pub first_seen_at: Option<DateTime<Utc>>,
    pub superseded_at: Option<DateTime<Utc>>,
}

impl Default for Rev<'_> {
    fn default() -> Self {
        Self {
            account: "work",
            conversation: "C1",
            ts: "1.0",
            seq: 1,
            text: "hello",
            first_seen_at: None,
            superseded_at: None,
        }
    }
}

pub async fn revision(pool: &PgPool, rev: Rev<'_>) {
    sqlx::query(
        "INSERT INTO message_revision (account_id, conversation_id, ts, seq, text,          first_seen_at, superseded_at) VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(rev.account)
    .bind(rev.conversation)
    .bind(rev.ts)
    .bind(rev.seq)
    .bind(rev.text)
    .bind(rev.first_seen_at)
    .bind(rev.superseded_at)
    .execute(pool)
    .await
    .expect("insert message_revision");
}

pub async fn auth_error(pool: &PgPool, account: &str, error: &str) {
    sqlx::query(
        "UPDATE account SET last_auth_error = $2, last_auth_error_at = now() WHERE id = $1",
    )
    .bind(account)
    .bind(error)
    .execute(pool)
    .await
    .expect("set auth error");
}

/// The DSN of the shared container, for the one test that needs a second database.
pub async fn container_dsn() -> Option<String> {
    CONTAINER.get()?.as_ref().map(|(_, dsn)| dsn.clone())
}

// --------------------------------------------------------- what the writers are given

/// Store messages the way the page writer does: one transaction, with the writers that
/// take a connection inside it. That is the production call shape, so the tests exercise
/// the same deref from `Transaction` that `pages.rs` will.
pub async fn store_messages(pool: &PgPool, messages: &[Message]) {
    let mut tx = pool.begin().await.expect("begin");
    slack_core::store::upsert_messages(&mut tx, messages)
        .await
        .expect("upsert messages");
    tx.commit().await.expect("commit");
}

/// The `Message` the ingester hands the store, from the same shape the row helpers use.
///
/// The shape the row helpers use, so one message reads the same either way.
pub fn message_model(msg: Msg<'_>) -> Message {
    Message {
        account_id: msg.account.to_owned(),
        conversation_id: msg.conversation.to_owned(),
        ts: msg.ts.to_owned(),
        thread_ts: msg.thread_ts.map(str::to_owned),
        user_id: msg.user_id.map(str::to_owned),
        text: msg.text.to_owned(),
        subtype: msg.subtype.map(str::to_owned),
        edited_at: msg.edited_at,
        deleted_at: msg.deleted_at,
        raw: msg.raw,
        posted_at: base() + chrono::TimeDelta::seconds(msg.offset_seconds),
        reactions: Vec::new(),
    }
}

/// The `Conversation` discovery hands the store.
pub fn conversation_model(conv: Conv<'_>) -> Conversation {
    Conversation {
        account_id: conv.account.to_owned(),
        id: conv.id.to_owned(),
        team_id: Some(conv.team_id.to_owned()),
        shared_team_ids: Vec::new(),
        kind: conv.kind.parse().expect("a conversation type"),
        name: conv.name.map(str::to_owned),
        is_member: true,
        counterpart_user_id: conv.counterpart_user_id.map(str::to_owned),
        in_scope: conv.in_scope,
        scope_reason: conv
            .scope_reason
            .map(|reason| reason.parse().expect("a scope reason")),
        poll_cursor: None,
        backfill_cursor: None,
        backfill_done: conv.backfill_done,
        reactions_fresh_since: conv.reactions_fresh_since,
        last_polled_at: conv.last_polled_at,
        is_ext_shared: false,
    }
}

/// `scope_reason` as the column holds it, for the gardening tests.
pub async fn stored_scope_reason(pool: &PgPool, id: &str) -> Option<String> {
    sqlx::query_scalar(
        "SELECT scope_reason FROM conversation WHERE account_id = 'work' AND id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("the conversation")
}
