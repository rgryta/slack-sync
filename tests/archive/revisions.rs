//! Edit history and deletion markers, against a real Postgres.
//!
//! Ported from `tests/test_revisions.py`. Slack is the one archive that already had
//! `edited_at` and `deleted_at` before this existed. What it never had was the text an
//! edit replaced — the upsert overwrote it — or any detection of deletion at all.
//!
//! A bug in here loses data silently: an unrecorded edit destroys the only copy of the
//! old text, and a false deletion hides a real message from every default view.

use chrono::{DateTime, TimeDelta, Utc};
use slack_core::store;

use crate::support::{self, Msg, base, db_test};

/// The parent chain and one stored message.
async fn one(pool: &sqlx::PgPool, text: &str) {
    crate::writes::seed(pool).await;
    support::store_messages(
        pool,
        &[support::message_model(Msg {
            text,
            ..Msg::default()
        })],
    )
    .await;
}

async fn revision_rows(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM message_revision")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn deleted_at(pool: &sqlx::PgPool, ts: &str) -> Option<DateTime<Utc>> {
    sqlx::query_scalar("SELECT deleted_at FROM message WHERE ts = $1")
        .bind(ts)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn missing_since(pool: &sqlx::PgPool, ts: &str) -> Option<DateTime<Utc>> {
    sqlx::query_scalar("SELECT missing_since FROM message WHERE ts = $1")
        .bind(ts)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn absent(pool: &sqlx::PgPool, ts: &str) -> u64 {
    store::mark_absent(pool, "work", "C1", &[ts.to_owned()])
        .await
        .unwrap()
}

// ------------------------------------------------------------------------- revisions

db_test!(an_unchanged_reread_writes_nothing, |f| {
    // The common case by orders of magnitude, and it must cost nothing.
    one(&f.pool, "hello").await;
    assert!(
        !store::record_revision(&f.pool, "work", "C1", "1.0", Some("hello"))
            .await
            .unwrap()
    );
    assert_eq!(revision_rows(&f.pool).await, 0);
});

db_test!(stored_null_and_empty_text_are_the_same_absence, |f| {
    // Attachment-only messages carry no text, and NULL vs '' is not an edit. The pure
    // half of this is `text_differs`, tested in the crate; this is the half that has to
    // agree with what the column holds.
    one(&f.pool, "").await;
    assert!(
        !store::record_revision(&f.pool, "work", "C1", "1.0", None)
            .await
            .unwrap()
    );
    assert_eq!(revision_rows(&f.pool).await, 0);
});

db_test!(first_edit_writes_both_the_old_and_the_new_text, |f| {
    one(&f.pool, "hello").await;
    assert!(
        store::record_revision(&f.pool, "work", "C1", "1.0", Some("hello there"))
            .await
            .unwrap()
    );
    let rows = store::list_revisions(&f.pool, "work", "C1", "1.0")
        .await
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| (r.seq, r.text.as_deref()))
            .collect::<Vec<_>>(),
        [(1, Some("hello")), (2, Some("hello there"))]
    );
    // The replaced text was never written down, so its start is unknown rather than
    // borrowed from the message's own timestamp.
    assert_eq!(rows[0].first_seen_at, None);
    assert_eq!(rows[1].superseded_at, None);
});

db_test!(exactly_one_current_revision_and_it_matches_the_text, |f| {
    // Asserted in SQL: this is the invariant every reader depends on, and the partial
    // unique index is what enforces it.
    one(&f.pool, "hello").await;
    for text in ["one", "two"] {
        store::record_revision(&f.pool, "work", "C1", "1.0", Some(text))
            .await
            .unwrap();
    }
    let row: (i64, Option<String>, String) = sqlx::query_as(
        "
        SELECT (SELECT count(*) FROM message_revision v
                 WHERE v.account_id = m.account_id AND v.conversation_id = m.conversation_id
                   AND v.ts = m.ts AND v.superseded_at IS NULL) AS current_rows,
               (SELECT v.text FROM message_revision v
                 WHERE v.account_id = m.account_id AND v.conversation_id = m.conversation_id
                   AND v.ts = m.ts AND v.superseded_at IS NULL) AS current_text,
               m.text
          FROM message m WHERE m.ts = '1.0'
        ",
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(row.0, 1);
    assert_eq!(row.1.as_deref(), Some("two"));
    assert_eq!(row.2, "two");
});

db_test!(a_message_the_archive_does_not_hold_is_never_an_edit, |f| {
    // The backfill walking into new ground is storing, not revising.
    crate::writes::seed(&f.pool).await;
    assert!(
        !store::record_revision(&f.pool, "work", "C1", "9.0", Some("text"))
            .await
            .unwrap()
    );
    assert_eq!(revision_rows(&f.pool).await, 0);
});

db_test!(concurrent_writers_produce_a_dense_seq, |f| {
    // The poll and the backfill can both reach one message in a single pass. `seq` is
    // allocated as max+1 under the message row's own lock, which is the only reason two
    // writers cannot both decide they are seq 2.
    one(&f.pool, "hello").await;
    let (a, b) = tokio::join!(
        store::record_revision(&f.pool, "work", "C1", "1.0", Some("a")),
        store::record_revision(&f.pool, "work", "C1", "1.0", Some("b")),
    );
    a.unwrap();
    b.unwrap();
    let seqs: Vec<i32> = store::list_revisions(&f.pool, "work", "C1", "1.0")
        .await
        .unwrap()
        .iter()
        .map(|r| r.seq)
        .collect();
    assert_eq!(seqs, [1, 2, 3]);
});

// -------------------------------------------------------------------------- deletion

db_test!(one_absence_marks_missing_but_never_deleted, |f| {
    one(&f.pool, "hello").await;
    assert_eq!(absent(&f.pool, "1.0").await, 0);
    assert!(missing_since(&f.pool, "1.0").await.is_some());
    assert!(deleted_at(&f.pool, "1.0").await.is_none());
});

db_test!(two_absences_mark_deleted, |f| {
    one(&f.pool, "hello").await;
    absent(&f.pool, "1.0").await;
    assert_eq!(absent(&f.pool, "1.0").await, 1);
    assert!(deleted_at(&f.pool, "1.0").await.is_some());
});

db_test!(reappearing_withdraws_the_claim, |f| {
    // We were wrong. The repair is to stop saying it, not to keep the tombstone.
    one(&f.pool, "hello").await;
    absent(&f.pool, "1.0").await;
    absent(&f.pool, "1.0").await;
    assert_eq!(
        store::mark_present(&f.pool, "work", "C1", &["1.0".to_owned()])
            .await
            .unwrap(),
        1
    );
    assert!(deleted_at(&f.pool, "1.0").await.is_none());
    assert!(missing_since(&f.pool, "1.0").await.is_none());
});

db_test!(a_stranded_suspicion_is_released_not_promoted, |f| {
    // Aging out of the window ends the ability to corroborate. Promoting here would turn
    // "we stopped being able to check" into "it was deleted", which nothing downstream
    // could ever discover was false.
    one(&f.pool, "hello").await;
    absent(&f.pool, "1.0").await;
    assert_eq!(
        store::release_stranded(&f.pool, "work", "C1", base() + TimeDelta::days(1))
            .await
            .unwrap(),
        1
    );
    assert!(missing_since(&f.pool, "1.0").await.is_none());
    assert!(deleted_at(&f.pool, "1.0").await.is_none());
});

db_test!(release_leaves_confirmed_deletions_alone, |f| {
    one(&f.pool, "hello").await;
    absent(&f.pool, "1.0").await;
    absent(&f.pool, "1.0").await;
    store::release_stranded(&f.pool, "work", "C1", base() + TimeDelta::days(1))
        .await
        .unwrap();
    assert!(deleted_at(&f.pool, "1.0").await.is_some());
});

// ------------------------------------------------------------------ what can be judged

db_test!(
    stored_window_ranges_on_posted_at_and_keeps_deleted_rows,
    |f| {
        // `ts` is an opaque identifier stored as text; ranging on it would be a string
        // comparison wearing a timestamp's clothes.
        one(&f.pool, "hello").await;
        absent(&f.pool, "1.0").await;
        absent(&f.pool, "1.0").await;
        let held = store::stored_window(&f.pool, "work", "C1", base() - TimeDelta::days(1))
            .await
            .unwrap();
        assert!(held.contains_key("1.0"));
        assert!(
            store::stored_window(&f.pool, "work", "C1", base() + TimeDelta::days(1))
                .await
                .unwrap()
                .is_empty()
        );
    }
);

db_test!(thread_replies_are_never_deletion_candidates, |f| {
    // `conversations.history` does not return replies, so their absence proves nothing.
    // The first live pass suspected 138 real messages, every one a thread reply, and a
    // second pass would have marked them deleted and hidden them from every view. A
    // message may only be judged missing by a channel that could have returned it.
    crate::writes::seed(&f.pool).await;
    support::store_messages(
        &f.pool,
        &[
            support::message_model(Msg::default()),
            support::message_model(Msg {
                ts: "2.0",
                thread_ts: Some("1.0"),
                ..Msg::default()
            }),
            // A parent carries thread_ts == ts in the archive only if Slack said so;
            // either way it is a message the window could have returned.
            support::message_model(Msg {
                ts: "3.0",
                thread_ts: Some("3.0"),
                ..Msg::default()
            }),
        ],
    )
    .await;
    let held = store::stored_window(&f.pool, "work", "C1", base() - TimeDelta::days(1))
        .await
        .unwrap();
    let mut keys: Vec<&str> = held.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["1.0", "3.0"]);
});

db_test!(a_thread_fetch_can_see_a_deleted_reply, |f| {
    // `conversations.replies` follows its cursor to the end, so it observes the whole
    // thread and absence from it is evidence — unlike the refresh window, which cannot
    // see replies at all.
    crate::writes::seed(&f.pool).await;
    support::store_messages(
        &f.pool,
        &[
            support::message_model(Msg::default()),
            support::message_model(Msg {
                ts: "2.0",
                thread_ts: Some("1.0"),
                ..Msg::default()
            }),
            support::message_model(Msg {
                ts: "3.0",
                thread_ts: Some("1.0"),
                ..Msg::default()
            }),
        ],
    )
    .await;
    let held = store::stored_thread(&f.pool, "work", "C1", "1.0")
        .await
        .unwrap();
    let mut keys: Vec<&str> = held.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["1.0", "2.0", "3.0"]);

    // The fetch came back without 3.0: two strikes and it is gone.
    absent(&f.pool, "3.0").await;
    assert_eq!(absent(&f.pool, "3.0").await, 1);
    assert!(deleted_at(&f.pool, "3.0").await.is_some());
});

db_test!(stored_texts_answers_for_a_page_and_nothing_else, |f| {
    // One query per page is the whole cost of edit tracking in steady state: the loops
    // compare in memory and touch the database only when something moved.
    one(&f.pool, "hello").await;
    let held = store::stored_texts(&f.pool, "work", "C1", &["1.0".to_owned(), "9.0".to_owned()])
        .await
        .unwrap();
    assert_eq!(held.get("1.0"), Some(&Some("hello".to_owned())));
    assert!(!held.contains_key("9.0"));
    assert!(
        store::stored_texts(&f.pool, "work", "C1", &[])
            .await
            .unwrap()
            .is_empty()
    );
});
