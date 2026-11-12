//! The write half of `store`, against a real Postgres.
//!
//! Ported from the write-side assertions in `tests/test_store.py`. Every case here needs
//! the database's own behaviour — ON CONFLICT semantics, foreign keys, `IS DISTINCT
//! FROM` — so none of it is meaningfully testable against a fake.
//!
//! The edit-tracking cases are in `revisions.rs`.

use chrono::{DateTime, TimeDelta, Utc};
use slack_core::models::{
    Account, Conversation, ConversationType, Emoji, Reaction, SyncLoop, SyncState, Workspace,
    WorkspaceUser,
};
use slack_core::store::{self, CursorUpdate};

use crate::support::{self, Conv, Msg, base, db_test};

/// The parent chain, written through the writers rather than around them: a conversation
/// cannot exist without its account, and that is the first thing these statements have
/// to get right.
pub async fn seed(pool: &sqlx::PgPool) {
    store::upsert_workspace(
        pool,
        &Workspace {
            team_id: "T1".into(),
            name: Some("team-T1".into()),
            domain: None,
            is_external: false,
        },
    )
    .await
    .unwrap();
    store::upsert_account(
        pool,
        &Account {
            id: "work".into(),
            team_id: "T1".into(),
            my_user_id: "UME".into(),
            enabled: true,
            last_auth_error: None,
            last_auth_error_at: None,
            updated_at: None,
        },
    )
    .await
    .unwrap();
    store::upsert_conversation(pool, &support::conversation_model(Conv::default()))
        .await
        .unwrap();
}

async fn count(pool: &sqlx::PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

// ------------------------------------------------------------------------- workspace

db_test!(upsert_workspace_never_replaces_a_name_with_null, |f| {
    // An external team is often first seen as a bare ID and named later, never back.
    let named = Workspace {
        team_id: "T9".into(),
        name: Some("Acme".into()),
        domain: Some("acme".into()),
        is_external: false,
    };
    store::upsert_workspace(&f.pool, &named).await.unwrap();
    store::upsert_workspace(
        &f.pool,
        &Workspace {
            team_id: "T9".into(),
            name: None,
            domain: None,
            is_external: true,
        },
    )
    .await
    .unwrap();

    let row: (Option<String>, Option<String>, bool) =
        sqlx::query_as("SELECT name, domain, is_external FROM workspace WHERE team_id = 'T9'")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(row.0.as_deref(), Some("Acme"));
    assert_eq!(row.1.as_deref(), Some("acme"));
    // is_external is an AND: a team seen as our own is never demoted to external.
    assert!(!row.2);
});

// ---------------------------------------------------------------------- conversation

db_test!(
    upsert_conversation_does_not_rewind_an_in_flight_crawl,
    |f| {
        seed(&f.pool).await;
        store::set_conversation_cursors(
            &f.pool,
            "work",
            "C1",
            CursorUpdate {
                poll_cursor: Some("1.0"),
                backfill_cursor: Some("page-2"),
                ..CursorUpdate::default()
            },
        )
        .await
        .unwrap();

        store::upsert_conversation(&f.pool, &support::conversation_model(Conv::default()))
            .await
            .unwrap();

        let row: (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT poll_cursor, backfill_cursor FROM conversation \
         WHERE account_id = 'work' AND id = 'C1'",
        )
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert_eq!(row.0.as_deref(), Some("1.0"));
        assert_eq!(row.1.as_deref(), Some("page-2"));
    }
);

db_test!(set_conversation_cursors_leaves_omitted_columns_alone, |f| {
    seed(&f.pool).await;
    store::set_conversation_cursors(
        &f.pool,
        "work",
        "C1",
        CursorUpdate {
            poll_cursor: Some("1.0"),
            backfill_done: Some(true),
            ..CursorUpdate::default()
        },
    )
    .await
    .unwrap();
    store::set_conversation_cursors(
        &f.pool,
        "work",
        "C1",
        CursorUpdate {
            last_polled_at: Some(base()),
            ..CursorUpdate::default()
        },
    )
    .await
    .unwrap();

    let row: (Option<String>, bool, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT poll_cursor, backfill_done, last_polled_at FROM conversation \
         WHERE account_id = 'work' AND id = 'C1'",
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(row.0.as_deref(), Some("1.0"));
    assert!(row.1);
    assert_eq!(row.2, Some(base()));
});

db_test!(membership_change_keeps_stored_messages, |f| {
    seed(&f.pool).await;
    support::store_messages(&f.pool, &[support::message_model(Msg::default())]).await;
    store::set_conversation_membership(&f.pool, "work", "C1", false)
        .await
        .unwrap();
    assert_eq!(count(&f.pool, "SELECT count(*) FROM message").await, 1);
    // ...and the conversation stops being polled, which is the point of the flag.
    assert!(
        store::list_member_conversation_ids(&f.pool, "work")
            .await
            .unwrap()
            .is_empty()
    );
});

db_test!(the_ingesters_own_conversation_reads_round_trip, |f| {
    seed(&f.pool).await;
    store::upsert_conversation(
        &f.pool,
        &support::conversation_model(Conv {
            id: "C2",
            name: Some("second"),
            in_scope: false,
            scope_reason: None,
            ..Conv::default()
        }),
    )
    .await
    .unwrap();
    support::store_messages(&f.pool, &[support::message_model(Msg::default())]).await;

    // `list_pollable_conversations` is membership, not scope: an out-of-scope member
    // channel is still where a mention is discovered.
    let pollable = store::list_pollable_conversations(&f.pool, "work")
        .await
        .unwrap();
    assert_eq!(
        pollable.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        ["C1", "C2"]
    );
    assert_eq!(pollable[0].kind, ConversationType::Channel);

    let one = store::get_conversation(&f.pool, "work", "C1")
        .await
        .unwrap()
        .expect("C1");
    assert_eq!(one.name.as_deref(), Some("general"));
    assert!(
        store::get_conversation(&f.pool, "work", "C404")
            .await
            .unwrap()
            .is_none()
    );

    // Only the in-scope one owes a backfill, and `exclude` skips what failed this run.
    let next = store::next_backfill_conversation(&f.pool, "work", &[])
        .await
        .unwrap();
    assert_eq!(next.map(|c| c.id), Some("C1".to_owned()));
    assert!(
        store::next_backfill_conversation(&f.pool, "work", &["C1".to_owned()])
            .await
            .unwrap()
            .is_none()
    );

    let activity = store::last_activity(&f.pool, "work").await.unwrap();
    assert_eq!(activity.get("C1"), Some(&base()));
    // A conversation with nothing stored is absent rather than present and null.
    assert!(!activity.contains_key("C2"));
});

// -------------------------------------------------------------------------- messages

db_test!(
    upsert_messages_is_idempotent_and_leaves_text_to_record_revision,
    |f| {
        // `text` deliberately does not move through the upsert. Both loops re-read pages
        // they have already stored, so carrying an edit through here would change the
        // archive with no record that anything changed — the exact gap `message_revision`
        // exists to close, hidden inside the write that closes it. `edited_at` still
        // refreshes: that is Slack's claim about the message, not ours.
        seed(&f.pool).await;
        let first = support::message_model(Msg {
            text: "first draft",
            ..Msg::default()
        });
        support::store_messages(&f.pool, std::slice::from_ref(&first)).await;
        support::store_messages(&f.pool, std::slice::from_ref(&first)).await;
        assert_eq!(count(&f.pool, "SELECT count(*) FROM message").await, 1);

        let edited_at = base() + TimeDelta::minutes(5);
        support::store_messages(
            &f.pool,
            &[support::message_model(Msg {
                text: "edited",
                edited_at: Some(edited_at),
                ..Msg::default()
            })],
        )
        .await;
        let row: (String, Option<DateTime<Utc>>) =
            sqlx::query_as("SELECT text, edited_at FROM message WHERE ts = '1.0'")
                .fetch_one(&f.pool)
                .await
                .unwrap();
        assert_eq!(row.0, "first draft");
        assert_eq!(row.1, Some(edited_at));

        assert!(
            store::record_revision(&f.pool, "work", "C1", "1.0", Some("edited"))
                .await
                .unwrap()
        );
        let text: String = sqlx::query_scalar("SELECT text FROM message WHERE ts = '1.0'")
            .fetch_one(&f.pool)
            .await
            .unwrap();
        assert_eq!(text, "edited");
        assert_eq!(count(&f.pool, "SELECT count(*) FROM message").await, 1);
    }
);

db_test!(upsert_messages_accepts_an_empty_batch, |f| {
    seed(&f.pool).await;
    support::store_messages(&f.pool, &[]).await;
    assert_eq!(count(&f.pool, "SELECT count(*) FROM message").await, 0);
});

db_test!(message_raw_round_trips_as_a_dict, |f| {
    seed(&f.pool).await;
    support::store_messages(
        &f.pool,
        &[support::message_model(Msg {
            raw: serde_json::json!({ "client_msg_id": "abc" }),
            ..Msg::default()
        })],
    )
    .await;
    let page = store::get_history(&f.pool, "C1", None, None, None, 10)
        .await
        .unwrap();
    assert_eq!(page[0].raw, serde_json::json!({ "client_msg_id": "abc" }));
});

db_test!(raw_is_stored_as_a_json_object_not_a_string, |f| {
    // Asserted in SQL on purpose. A round trip through the driver cannot see this bug:
    // the decode path is symmetric, so a double-encode looks correct from the
    // application while every jsonb query on the column quietly stops matching.
    seed(&f.pool).await;
    support::store_messages(
        &f.pool,
        &[support::message_model(Msg {
            raw: serde_json::json!({ "reply_count": 3 }),
            ..Msg::default()
        })],
    )
    .await;
    let kind: String = sqlx::query_scalar("SELECT jsonb_typeof(raw) FROM message")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(kind, "object");
    let replies: i32 = sqlx::query_scalar("SELECT (raw->>'reply_count')::int FROM message")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(replies, 3);
});

db_test!(deletion_tombstones_rather_than_removes, |f| {
    // Two strikes to claim it, and the claim does not drift once made. This replaces
    // `mark_message_deleted`, which tombstoned on a single absence and which nothing
    // ever called.
    seed(&f.pool).await;
    support::store_messages(&f.pool, &[support::message_model(Msg::default())]).await;
    let absent = async || {
        store::mark_absent(&f.pool, "work", "C1", &["1.0".to_owned()])
            .await
            .unwrap()
    };
    assert_eq!(absent().await, 0);
    assert_eq!(absent().await, 1);

    let first: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT deleted_at FROM message WHERE ts = '1.0'")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert!(first.is_some());

    absent().await;
    let again: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT deleted_at FROM message WHERE ts = '1.0'")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(again, first, "the claim moved after it was made");
    assert_eq!(
        count(&f.pool, "SELECT count(*) FROM message WHERE ts = '1.0'").await,
        1
    );
});

db_test!(
    unhydrated_threads_are_the_parents_still_owing_replies,
    |f| {
        seed(&f.pool).await;
        support::store_messages(
            &f.pool,
            &[
                support::message_model(Msg {
                    raw: serde_json::json!({ "reply_count": 2 }),
                    ..Msg::default()
                }),
                support::message_model(Msg {
                    ts: "2.0",
                    thread_ts: Some("1.0"),
                    offset_seconds: 1,
                    ..Msg::default()
                }),
                support::message_model(Msg {
                    ts: "3.0",
                    raw: serde_json::json!({ "reply_count": 1 }),
                    offset_seconds: 2,
                    ..Msg::default()
                }),
                support::message_model(Msg {
                    ts: "4.0",
                    thread_ts: Some("3.0"),
                    offset_seconds: 3,
                    ..Msg::default()
                }),
                support::message_model(Msg {
                    ts: "5.0",
                    offset_seconds: 4,
                    ..Msg::default()
                }),
            ],
        )
        .await;
        // 1.0 claims two and holds one; 3.0 claims one and holds it; 5.0 has no thread.
        assert_eq!(
            store::list_unhydrated_threads(&f.pool, "work", 50)
                .await
                .unwrap(),
            [("C1".to_owned(), "1.0".to_owned())]
        );
    }
);

// ------------------------------------------------------------------------- reactions

db_test!(replace_reactions_reconciles_the_whole_set, |f| {
    // Slack sends reactions as a snapshot, so an absent name means removed. Add, then
    // change a count, then drop a name, then clear everything — each pass must leave the
    // stored set exactly equal to what was observed.
    seed(&f.pool).await;
    support::store_messages(&f.pool, &[support::message_model(Msg::default())]).await;

    let observe = async |reactions: Vec<Reaction>| -> Vec<(String, i32)> {
        let mut tx = f.pool.begin().await.unwrap();
        store::replace_reactions(&mut tx, "work", "C1", "1.0", &reactions)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        sqlx::query_as("SELECT name, count FROM reaction WHERE message_ts = '1.0' ORDER BY name")
            .fetch_all(&f.pool)
            .await
            .unwrap()
    };
    let reaction = |name: &str, users: &[&str], count: i32| Reaction {
        name: name.to_owned(),
        user_ids: users.iter().map(|u| (*u).to_owned()).collect(),
        count,
    };

    assert_eq!(
        observe(vec![reaction("tada", &["U1"], 1), reaction("eyes", &[], 2)]).await,
        [("eyes".to_owned(), 2), ("tada".to_owned(), 1)]
    );
    // a count changing on an existing name
    assert_eq!(
        observe(vec![
            reaction("tada", &["U1", "U2"], 2),
            reaction("eyes", &[], 2)
        ])
        .await,
        [("eyes".to_owned(), 2), ("tada".to_owned(), 2)]
    );
    // a name disappearing from the snapshot is a removal
    assert_eq!(
        observe(vec![reaction("tada", &["U1", "U2"], 2)]).await,
        [("tada".to_owned(), 2)]
    );
    // an empty snapshot clears the message rather than meaning "no information"
    assert!(observe(Vec::new()).await.is_empty());
});

db_test!(replace_reactions_is_scoped_to_one_message, |f| {
    seed(&f.pool).await;
    support::store_messages(
        &f.pool,
        &[
            support::message_model(Msg::default()),
            support::message_model(Msg {
                ts: "2.0",
                offset_seconds: 1,
                ..Msg::default()
            }),
        ],
    )
    .await;
    let one = |name: &str| {
        vec![Reaction {
            name: name.to_owned(),
            user_ids: Vec::new(),
            count: 1,
        }]
    };
    let mut tx = f.pool.begin().await.unwrap();
    store::replace_reactions(&mut tx, "work", "C1", "1.0", &one("tada"))
        .await
        .unwrap();
    store::replace_reactions(&mut tx, "work", "C1", "2.0", &one("wave"))
        .await
        .unwrap();
    store::replace_reactions(&mut tx, "work", "C1", "1.0", &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let rows: Vec<(String, String)> = sqlx::query_as("SELECT message_ts, name FROM reaction")
        .fetch_all(&f.pool)
        .await
        .unwrap();
    assert_eq!(rows, [("2.0".to_owned(), "wave".to_owned())]);
});

// ------------------------------------------------------------------- mention threads

db_test!(register_mention_thread_keeps_the_first_mention, |f| {
    seed(&f.pool).await;
    let mut tx = f.pool.begin().await.unwrap();
    assert!(
        store::register_mention_thread(&mut tx, "work", "C1", "1.0", "1.0")
            .await
            .unwrap()
    );
    assert!(
        !store::register_mention_thread(&mut tx, "work", "C1", "1.0", "9.0")
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();

    let mention_ts: String = sqlx::query_scalar("SELECT mention_ts FROM mention_thread")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(mention_ts, "1.0");
    assert_eq!(
        store::list_mention_thread_keys(&f.pool, "work")
            .await
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>(),
        [("C1".to_owned(), "1.0".to_owned())]
    );
});

// --------------------------------------------------------- the directory and the emoji

db_test!(a_payload_without_a_display_name_does_not_erase_one, |f| {
    seed(&f.pool).await;
    let user = WorkspaceUser {
        account_id: "work".into(),
        user_id: "U1".into(),
        team_id: None,
        name: Some("sg".into()),
        real_name: None,
        display_name: Some("Sandy Ganoti".into()),
        is_bot: false,
    };
    store::upsert_users(&f.pool, std::slice::from_ref(&user))
        .await
        .unwrap();
    store::upsert_users(
        &f.pool,
        &[WorkspaceUser {
            display_name: None,
            ..user
        }],
    )
    .await
    .unwrap();
    let display_name: Option<String> =
        sqlx::query_scalar("SELECT display_name FROM workspace_user WHERE user_id = 'U1'")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(display_name.as_deref(), Some("Sandy Ganoti"));
});

db_test!(upsert_emoji_is_idempotent, |f| {
    seed(&f.pool).await;
    let emoji = |url: &str| {
        vec![Emoji {
            team_id: "T1".into(),
            name: "party".into(),
            url: Some(url.to_owned()),
            alias_for: None,
        }]
    };
    store::upsert_emoji(&f.pool, &emoji("https://x/1.png"))
        .await
        .unwrap();
    store::upsert_emoji(&f.pool, &emoji("https://x/2.png"))
        .await
        .unwrap();
    let url: Option<String> = sqlx::query_scalar("SELECT url FROM emoji WHERE name = 'party'")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(url.as_deref(), Some("https://x/2.png"));
});

// ------------------------------------------------------------------ status and state

db_test!(sync_status_and_state_round_trip, |f| {
    seed(&f.pool).await;
    let state = |last_run_at: Option<DateTime<Utc>>, current: Option<&str>| SyncState {
        account_id: "work".into(),
        r#loop: SyncLoop::Poll,
        current_conversation_id: current.map(str::to_owned),
        last_run_at,
        last_error: None,
        last_error_at: None,
    };
    store::set_sync_state(&f.pool, &state(Some(base()), Some("C1")))
        .await
        .unwrap();
    store::set_sync_state(&f.pool, &state(None, None))
        .await
        .unwrap();

    let states = store::get_sync_state(&f.pool, "work").await.unwrap();
    assert_eq!(states.len(), 1);
    // last_run_at is COALESCEd: a pass that fails before running must not erase the last
    // time the loop actually ran.
    assert_eq!(states[0].last_run_at, Some(base()));
    assert_eq!(states[0].r#loop, SyncLoop::Poll);

    store::set_account_auth_error(&f.pool, "work", Some("invalid_auth"))
        .await
        .unwrap();
    let status = store::sync_status(&f.pool, Some("work")).await.unwrap();
    assert_eq!(status[0].last_auth_error.as_deref(), Some("invalid_auth"));
    assert_eq!(status[0].in_scope_count, 1);

    store::set_account_auth_error(&f.pool, "work", None)
        .await
        .unwrap();
    let status = store::sync_status(&f.pool, None).await.unwrap();
    assert_eq!(status[0].last_auth_error, None);
    assert_eq!(status[0].last_auth_error_at, None);
});

db_test!(a_boot_does_not_re_enable_an_account_a_human_disabled, |f| {
    // The Python assigned `EXCLUDED.enabled`, and every boot builds an `Account` with
    // the default `true`, so a hand-disabled account came back on at the next restart.
    // Nothing in the code writes `false`, so the column is a person's switch.
    seed(&f.pool).await;
    sqlx::query("UPDATE account SET enabled = false WHERE id = 'work'")
        .execute(&f.pool)
        .await
        .unwrap();

    seed(&f.pool).await;

    let enabled: bool = sqlx::query_scalar("SELECT enabled FROM account WHERE id = 'work'")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert!(!enabled, "the boot switched a disabled account back on");
    // ...and a new account still starts enabled, which is what the insert is for.
    assert!(
        store::list_accounts(&f.pool)
            .await
            .unwrap()
            .iter()
            .all(|a| a.id == "work")
    );
});

db_test!(
    re_recording_the_same_auth_error_does_not_move_its_timestamp,
    |f| {
        // `status` prints this as "failing since". Pushing it forward on every pass would
        // turn that into "still failing as of", the one thing it must not mean.
        seed(&f.pool).await;
        store::set_account_auth_error(&f.pool, "work", Some("invalid_auth"))
            .await
            .unwrap();
        let first: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT last_auth_error_at FROM account WHERE id = 'work'")
                .fetch_one(&f.pool)
                .await
                .unwrap();
        store::set_account_auth_error(&f.pool, "work", Some("invalid_auth"))
            .await
            .unwrap();
        let again: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT last_auth_error_at FROM account WHERE id = 'work'")
                .fetch_one(&f.pool)
                .await
                .unwrap();
        assert_eq!(first, again);
        assert!(first.is_some());
    }
);

db_test!(
    progress_and_failure_are_written_without_erasing_each_other,
    |f| {
        // A later successful step must not erase the record of an earlier gap by writing a
        // NULL error over it, which is why `set_sync_progress` exists beside
        // `set_sync_state`.
        seed(&f.pool).await;
        store::set_sync_state(
            &f.pool,
            &SyncState {
                account_id: "work".into(),
                r#loop: SyncLoop::Backfill,
                current_conversation_id: None,
                last_run_at: Some(base()),
                last_error: "boom".to_owned().into(),
                last_error_at: Some(base()),
            },
        )
        .await
        .unwrap();
        store::set_sync_progress(
            &f.pool,
            "work",
            SyncLoop::Backfill,
            Some("C1"),
            Some(base() + TimeDelta::minutes(1)),
        )
        .await
        .unwrap();

        let states = store::get_sync_state(&f.pool, "work").await.unwrap();
        assert_eq!(states[0].current_conversation_id.as_deref(), Some("C1"));
        assert_eq!(states[0].last_run_at, Some(base() + TimeDelta::minutes(1)));
        assert_eq!(states[0].last_error.as_deref(), Some("boom"));
    }
);

// -------------------------------------------------------- the scope_reason gardening

/// All three branches of `upsert_conversation`'s CASE. A simplified
/// `COALESCE(EXCLUDED, stored)` still passes the first and fails the third; the reverse
/// fails the first. Both simplifications are silent in production.
async fn gardened(pool: &sqlx::PgPool, conv: Conversation) -> Option<String> {
    store::upsert_conversation(pool, &conv).await.unwrap();
    support::stored_scope_reason(pool, "C9").await
}

fn c9(in_scope: bool, scope_reason: Option<&'static str>) -> Conversation {
    support::conversation_model(Conv {
        id: "C9",
        name: Some("logistics"),
        in_scope,
        scope_reason,
        ..Conv::default()
    })
}

db_test!(
    naming_a_channel_replaces_the_reason_it_first_slipped_in_by,
    |f| {
        // logistics read `mention_thread` after it was named in the scope list,
        // because the reason was written once and then kept for ever.
        seed(&f.pool).await;
        assert_eq!(
            gardened(&f.pool, c9(true, Some("mention_thread"))).await,
            Some("mention_thread".to_owned())
        );
        assert_eq!(
            gardened(&f.pool, c9(true, Some("named_channel"))).await,
            Some("named_channel".to_owned())
        );
    }
);

db_test!(
    a_channel_that_leaves_the_scope_list_keeps_its_old_reason,
    |f| {
        // Discovery reports NULL once a channel is no longer named, and backfill reads the
        // stale `named_channel` to notice it left and stop crawling. Clearing it would hide
        // that.
        seed(&f.pool).await;
        gardened(&f.pool, c9(true, Some("named_channel"))).await;
        assert_eq!(
            gardened(&f.pool, c9(false, None)).await,
            Some("named_channel".to_owned())
        );
    }
);

db_test!(a_mention_does_not_overwrite_a_named_channel, |f| {
    seed(&f.pool).await;
    gardened(&f.pool, c9(true, Some("named_channel"))).await;
    assert_eq!(
        gardened(&f.pool, c9(true, Some("mention_thread"))).await,
        Some("named_channel".to_owned())
    );
});
