//! The read half of `store`, against a real Postgres.
//!
//! Ported from the read-side assertions in `tests/test_store.py`. The write-side ones
//! (`upsert_*`, `record_revision`, `mark_absent`, `release_stranded`, scope-reason
//! gardening) are not here: their functions belong to the ingester and land with it in
//! phase 2. See the PR description for the list.

use chrono::TimeDelta;
use slack_core::models::Identity;
use slack_core::store;

use crate::support::{self, Conv, Msg, Rev, base, db_test};

db_test!(get_history_returns_the_newest_page_in_reading_order, |f| {
    support::seed(&f.pool).await;
    support::messages(
        &f.pool,
        (1..=5)
            .map(|i| Msg {
                ts: match i {
                    1 => "1.0",
                    2 => "2.0",
                    3 => "3.0",
                    4 => "4.0",
                    _ => "5.0",
                },
                text: match i {
                    1 => "m1",
                    2 => "m2",
                    3 => "m3",
                    4 => "m4",
                    _ => "m5",
                },
                offset_seconds: i,
                ..Msg::default()
            })
            .collect(),
    )
    .await;
    let page = store::get_history(&f.pool, "C1", None, None, None, 3)
        .await
        .unwrap();
    assert_eq!(texts(&page), ["m3", "m4", "m5"]);
});

db_test!(get_history_range_filters, |f| {
    support::seed(&f.pool).await;
    support::messages(
        &f.pool,
        vec![
            Msg {
                ts: "1.0",
                text: "m1",
                offset_seconds: 60,
                ..Msg::default()
            },
            Msg {
                ts: "2.0",
                text: "m2",
                offset_seconds: 120,
                ..Msg::default()
            },
            Msg {
                ts: "3.0",
                text: "m3",
                offset_seconds: 180,
                ..Msg::default()
            },
        ],
    )
    .await;
    let page = store::get_history(
        &f.pool,
        "C1",
        None,
        Some(base() + TimeDelta::seconds(60)),
        Some(base() + TimeDelta::seconds(180)),
        50,
    )
    .await
    .unwrap();
    assert_eq!(texts(&page), ["m2"]);
});

db_test!(
    get_history_with_an_after_reads_forward_from_it_and_counts_the_rest,
    |f| {
        // Asking for a period from its start used to return the END of it, with nothing
        // to say the rest existed.
        support::seed(&f.pool).await;
        support::messages(
            &f.pool,
            vec![
                Msg {
                    ts: "1.0",
                    text: "m1",
                    offset_seconds: 1,
                    ..Msg::default()
                },
                Msg {
                    ts: "2.0",
                    text: "m2",
                    offset_seconds: 2,
                    ..Msg::default()
                },
                Msg {
                    ts: "3.0",
                    text: "m3",
                    offset_seconds: 3,
                    ..Msg::default()
                },
                Msg {
                    ts: "4.0",
                    text: "m4",
                    offset_seconds: 4,
                    ..Msg::default()
                },
                Msg {
                    ts: "5.0",
                    text: "m5",
                    offset_seconds: 5,
                    ..Msg::default()
                },
            ],
        )
        .await;
        let page = store::get_history(&f.pool, "C1", None, Some(base()), None, 2)
            .await
            .unwrap();
        assert_eq!(texts(&page), ["m1", "m2"]);
        assert_eq!(
            store::count_history(&f.pool, "C1", None, Some(base()), None)
                .await
                .unwrap(),
            5
        );
    }
);

db_test!(get_thread_includes_the_parent, |f| {
    support::seed(&f.pool).await;
    support::messages(
        &f.pool,
        vec![
            Msg {
                ts: "1.0",
                text: "parent",
                ..Msg::default()
            },
            Msg {
                ts: "2.0",
                text: "reply",
                thread_ts: Some("1.0"),
                offset_seconds: 10,
                ..Msg::default()
            },
            Msg {
                ts: "3.0",
                text: "unrelated",
                offset_seconds: 20,
                ..Msg::default()
            },
        ],
    )
    .await;
    let thread = store::get_thread(&f.pool, "C1", "1.0", None).await.unwrap();
    assert_eq!(texts(&thread), ["parent", "reply"]);
});

db_test!(messages_since_only_covers_in_scope_conversations, |f| {
    support::seed(&f.pool).await;
    support::conversation(
        &f.pool,
        Conv {
            id: "C2",
            name: Some("noise"),
            in_scope: false,
            scope_reason: None,
            ..Conv::default()
        },
    )
    .await;
    support::messages(
        &f.pool,
        vec![
            Msg {
                ts: "1.0",
                text: "kept",
                offset_seconds: 10,
                ..Msg::default()
            },
            Msg {
                ts: "2.0",
                conversation: "C2",
                text: "dropped",
                offset_seconds: 20,
                ..Msg::default()
            },
        ],
    )
    .await;
    let recent = store::messages_since(&f.pool, base(), None, None, 200)
        .await
        .unwrap();
    assert_eq!(texts(&recent), ["kept"]);
});

db_test!(reads_attach_reactions, |f| {
    support::seed(&f.pool).await;
    support::message(&f.pool, Msg::default()).await;
    support::reaction(&f.pool, "work", "C1", "1.0", "tada", &["U2"], 1).await;
    let history = store::get_history(&f.pool, "C1", None, None, None, 50)
        .await
        .unwrap();
    let reactions = &history[0].reactions;
    assert_eq!(reactions.len(), 1);
    assert_eq!(reactions[0].name, "tada");
    assert_eq!(reactions[0].count, 1);
    assert_eq!(reactions[0].user_ids, vec!["U2".to_string()]);
});

db_test!(search_matches_a_non_english_token, |f| {
    // The tsvector is `simple`, so a Polish word is matched as written. Under `english`
    // stemming these tokens are mangled and the hit disappears silently, which for a
    // mixed-language corpus is a wrong answer that looks like an empty one.
    support::seed(&f.pool).await;
    support::messages(
        &f.pool,
        vec![
            Msg {
                ts: "1.0",
                text: "zażółć gęślą jaźń",
                offset_seconds: 1,
                ..Msg::default()
            },
            Msg {
                ts: "2.0",
                text: "something else entirely",
                offset_seconds: 2,
                ..Msg::default()
            },
        ],
    )
    .await;
    let hits = search(&f.pool, "gęślą").await;
    assert_eq!(
        hits.iter()
            .map(|h| h.message.ts.as_str())
            .collect::<Vec<_>>(),
        ["1.0"]
    );
    assert!(hits[0].rank > 0.0);
    assert!(hits[0].snippet.contains("gęślą"), "{}", hits[0].snippet);
});

db_test!(search_filters, |f| {
    support::seed(&f.pool).await;
    support::messages(
        &f.pool,
        vec![
            Msg {
                ts: "1.0",
                text: "deploy the thing",
                user_id: Some("U1"),
                offset_seconds: 1,
                ..Msg::default()
            },
            Msg {
                ts: "2.0",
                text: "deploy it again",
                user_id: Some("U2"),
                offset_seconds: 2,
                ..Msg::default()
            },
        ],
    )
    .await;

    let by_author = store::search_messages(
        &f.pool,
        "deploy",
        None,
        None,
        None,
        Some(&[Identity {
            account_id: "work".into(),
            user_id: "U2".into(),
        }]),
        None,
        None,
        None,
        50,
    )
    .await
    .unwrap();
    assert_eq!(
        by_author
            .iter()
            .map(|h| h.message.ts.as_str())
            .collect::<Vec<_>>(),
        ["2.0"]
    );

    let by_team = store::search_messages(
        &f.pool,
        "deploy",
        None,
        Some("T-nope"),
        None,
        None,
        None,
        None,
        None,
        50,
    )
    .await
    .unwrap();
    assert!(by_team.is_empty());

    support::reaction(&f.pool, "work", "C1", "1.0", "tada", &[], 1).await;
    let reacted = store::search_messages(
        &f.pool,
        "deploy",
        None,
        None,
        None,
        None,
        None,
        None,
        Some(true),
        50,
    )
    .await
    .unwrap();
    assert_eq!(
        reacted
            .iter()
            .map(|h| h.message.ts.as_str())
            .collect::<Vec<_>>(),
        ["1.0"]
    );
    let unreacted = store::search_messages(
        &f.pool,
        "deploy",
        None,
        None,
        None,
        None,
        None,
        None,
        Some(false),
        50,
    )
    .await
    .unwrap();
    assert_eq!(
        unreacted
            .iter()
            .map(|h| h.message.ts.as_str())
            .collect::<Vec<_>>(),
        ["2.0"]
    );
});

db_test!(empty_search_returns_nothing_not_everything, |f| {
    support::seed(&f.pool).await;
    support::message(&f.pool, Msg::default()).await;
    assert!(search(&f.pool, "   ").await.is_empty());
});

db_test!(list_mentions_reads_the_corpus_not_the_thread_table, |f| {
    // `mention_thread` only ever holds pings in conversations the filter did NOT capture
    // wholesale. On a DM/named-channel scope it stays permanently empty while the corpus
    // holds hundreds of real mentions, so mentions are read from the text.
    support::seed(&f.pool).await;
    support::messages(
        &f.pool,
        vec![
            Msg {
                ts: "1.0",
                text: "hey <@UME> look",
                offset_seconds: 1,
                ..Msg::default()
            },
            Msg {
                ts: "2.0",
                text: "unrelated",
                offset_seconds: 2,
                ..Msg::default()
            },
        ],
    )
    .await;
    let found = store::list_mentions(&f.pool, "UME", None, None, None, 50)
        .await
        .unwrap();
    assert_eq!(texts(&found), ["hey <@UME> look"]);
});

db_test!(a_self_mention_is_not_a_ping, |f| {
    support::seed(&f.pool).await;
    support::message(
        &f.pool,
        Msg {
            ts: "1.0",
            text: "reminding <@UME> myself",
            user_id: Some("UME"),
            offset_seconds: 1,
            ..Msg::default()
        },
    )
    .await;
    assert!(
        store::list_mentions(&f.pool, "UME", None, None, None, 50)
            .await
            .unwrap()
            .is_empty()
    );
});

db_test!(a_partial_id_match_is_not_a_mention, |f| {
    // `<@UMEOW>` must not count as a ping for `UME`; the token is delimited.
    support::seed(&f.pool).await;
    support::message(
        &f.pool,
        Msg {
            ts: "1.0",
            text: "hi <@UMEOW>",
            offset_seconds: 1,
            ..Msg::default()
        },
    )
    .await;
    assert!(
        store::list_mentions(&f.pool, "UME", None, None, None, 50)
            .await
            .unwrap()
            .is_empty()
    );
});

db_test!(an_alias_spans_two_accounts, |f| {
    // One name, many identities — the point of aliases in a multi-account corpus.
    support::seed(&f.pool).await;
    support::workspace(&f.pool, "T2", "team-T2", "t2").await;
    support::account(&f.pool, "side", "T2", "U9ME").await;
    let members = vec![
        Identity {
            account_id: "work".into(),
            user_id: "U1".into(),
        },
        Identity {
            account_id: "side".into(),
            user_id: "U9".into(),
        },
    ];
    assert!(
        store::set_alias(&f.pool, "ted", &members, Some("Ted"))
            .await
            .unwrap()
            .is_empty()
    );

    let mut resolved = store::resolve_author(&f.pool, "ted").await.unwrap();
    resolved.sort();
    assert_eq!(
        resolved
            .iter()
            .map(|i| (i.account_id.as_str(), i.user_id.as_str()))
            .collect::<Vec<_>>(),
        [("side", "U9"), ("work", "U1")]
    );

    let aliases = store::list_aliases(&f.pool, Some("ted")).await.unwrap();
    assert_eq!(aliases.len(), 1);
    assert_eq!(aliases[0].note.as_deref(), Some("Ted"));
    assert_eq!(aliases[0].members.len(), 2);
});

db_test!(setting_an_alias_reports_identities_moved_off_another, |f| {
    support::seed(&f.pool).await;
    let identity = vec![Identity {
        account_id: "work".into(),
        user_id: "U1".into(),
    }];
    store::set_alias(&f.pool, "ted", &identity, None)
        .await
        .unwrap();
    let moved = store::set_alias(&f.pool, "theodore", &identity, None)
        .await
        .unwrap();
    assert_eq!(
        moved
            .iter()
            .map(|i| (i.account_id.as_str(), i.user_id.as_str()))
            .collect::<Vec<_>>(),
        [("work", "U1")]
    );
    assert_eq!(
        store::resolve_author(&f.pool, "theodore")
            .await
            .unwrap()
            .iter()
            .map(|i| i.user_id.as_str())
            .collect::<Vec<_>>(),
        ["U1"]
    );
    assert!(
        store::resolve_author(&f.pool, "ted")
            .await
            .unwrap()
            .is_empty()
    );
});

db_test!(setting_an_empty_member_list_deletes_the_alias, |f| {
    support::seed(&f.pool).await;
    store::set_alias(
        &f.pool,
        "ted",
        &[Identity {
            account_id: "work".into(),
            user_id: "U1".into(),
        }],
        None,
    )
    .await
    .unwrap();
    store::set_alias(&f.pool, "ted", &[], None).await.unwrap();
    assert!(
        store::list_aliases(&f.pool, Some("ted"))
            .await
            .unwrap()
            .is_empty()
    );
});

db_test!(deleting_an_alias_leaves_messages_intact, |f| {
    support::seed(&f.pool).await;
    support::message(
        &f.pool,
        Msg {
            text: "still here",
            ..Msg::default()
        },
    )
    .await;
    store::set_alias(
        &f.pool,
        "ted",
        &[Identity {
            account_id: "work".into(),
            user_id: "U1".into(),
        }],
        None,
    )
    .await
    .unwrap();
    // An empty member list is how production deletes an alias.
    store::set_alias(&f.pool, "ted", &[], None).await.unwrap();
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM user_alias_member")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(remaining, 0);
    let history = store::get_history(&f.pool, "C1", None, None, None, 50)
        .await
        .unwrap();
    assert_eq!(texts(&history), ["still here"]);
});

db_test!(resolve_author_falls_back_to_a_raw_user_id, |f| {
    support::seed(&f.pool).await;
    support::user(&f.pool, "work", "U1", Some("ted"), None, None).await;
    assert_eq!(
        store::resolve_author(&f.pool, "U1")
            .await
            .unwrap()
            .iter()
            .map(|i| i.user_id.as_str())
            .collect::<Vec<_>>(),
        ["U1"]
    );
    assert!(
        store::resolve_author(&f.pool, "nobody")
            .await
            .unwrap()
            .is_empty()
    );
});

db_test!(resolve_author_matches_a_name_on_screen, |f| {
    // The rendered name has to be usable as `author`, or a result is a dead end.
    support::seed(&f.pool).await;
    support::user(
        &f.pool,
        "work",
        "U1",
        Some("sg"),
        Some("Kyriaki Aikaterini Ganoti"),
        Some("Sandy Ganoti"),
    )
    .await;
    for spelling in ["Sandy Ganoti", "sandy ganoti", "Kyriaki Aikaterini Ganoti"] {
        assert_eq!(
            store::resolve_author(&f.pool, spelling)
                .await
                .unwrap()
                .iter()
                .map(|i| i.user_id.as_str())
                .collect::<Vec<_>>(),
            ["U1"],
            "{spelling}"
        );
    }
    // Whole-string only: a first name must not silently stand in for everyone.
    assert!(
        store::resolve_author(&f.pool, "Sandy")
            .await
            .unwrap()
            .is_empty()
    );
});

db_test!(list_workspaces_counts_in_scope_conversations, |f| {
    support::seed(&f.pool).await;
    support::conversation(
        &f.pool,
        Conv {
            id: "C2",
            name: Some("noise"),
            in_scope: false,
            scope_reason: None,
            ..Conv::default()
        },
    )
    .await;
    let summaries = store::list_workspaces(&f.pool).await.unwrap();
    assert_eq!(
        summaries
            .iter()
            .map(|s| (s.team_id.as_str(), s.conversation_count))
            .collect::<Vec<_>>(),
        [("T1", 1)]
    );
});

db_test!(list_conversations_reports_activity, |f| {
    support::seed(&f.pool).await;
    support::messages(
        &f.pool,
        vec![
            Msg::default(),
            Msg {
                ts: "2.0",
                offset_seconds: 5,
                ..Msg::default()
            },
        ],
    )
    .await;
    let summaries = store::list_conversations(&f.pool, Some("work"), Some("T1"), Some("channel"))
        .await
        .unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].message_count, 2);
    assert_eq!(
        summaries[0].last_activity,
        Some(base() + TimeDelta::seconds(5))
    );
});

db_test!(resolve_message_accepts_the_conversation_name, |f| {
    support::seed(&f.pool).await;
    support::message(&f.pool, Msg::default()).await;
    assert!(
        store::resolve_message(&f.pool, "C1", "1.0", None)
            .await
            .unwrap()
            .is_some()
    );
    // The name a caller actually typed, not only the id.
    assert!(
        store::resolve_message(&f.pool, "general", "1.0", None)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store::resolve_message(&f.pool, "C1", "nope", None)
            .await
            .unwrap()
            .is_none()
    );
});

db_test!(list_revisions_is_ordered_by_seq, |f| {
    support::seed(&f.pool).await;
    support::message(
        &f.pool,
        Msg {
            text: "hello there",
            ..Msg::default()
        },
    )
    .await;
    support::revision(
        &f.pool,
        Rev {
            seq: 2,
            text: "hello there",
            first_seen_at: Some(base()),
            ..Rev::default()
        },
    )
    .await;
    support::revision(
        &f.pool,
        Rev {
            seq: 1,
            text: "hello",
            superseded_at: Some(base()),
            ..Rev::default()
        },
    )
    .await;
    let rows = store::list_revisions(&f.pool, "work", "C1", "1.0")
        .await
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| (r.seq, r.text.as_deref()))
            .collect::<Vec<_>>(),
        [(1, Some("hello")), (2, Some("hello there"))]
    );
    // NULL on seq 1 and only there: nothing records when a message was first stored.
    assert!(rows[0].first_seen_at.is_none());
    assert!(rows[1].superseded_at.is_none());
});

db_test!(
    an_unknown_workspace_resolves_to_nothing_not_to_everything,
    |f| {
        support::seed(&f.pool).await;
        assert_eq!(
            store::resolve_workspace(&f.pool, "t1")
                .await
                .unwrap()
                .as_deref(),
            Some("T1")
        );
        assert_eq!(
            store::resolve_workspace(&f.pool, "TEAM-T1")
                .await
                .unwrap()
                .as_deref(),
            Some("T1")
        );
        assert!(
            store::resolve_workspace(&f.pool, "nope")
                .await
                .unwrap()
                .is_none()
        );
    }
);

db_test!(sync_status_counts_backfilled_conversations, |f| {
    support::seed(&f.pool).await;
    support::conversation(
        &f.pool,
        Conv {
            id: "C2",
            name: Some("other"),
            backfill_done: true,
            ..Conv::default()
        },
    )
    .await;
    let rows = store::sync_status(&f.pool, Some("work")).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].backfilled_count, rows[0].in_scope_count), (1, 2));
});

fn texts(messages: &[slack_core::models::Message]) -> Vec<&str> {
    messages.iter().map(|m| m.text.as_str()).collect()
}

async fn search(pool: &sqlx::PgPool, query: &str) -> Vec<slack_core::models::SearchHit> {
    store::search_messages(pool, query, None, None, None, None, None, None, None, 50)
        .await
        .unwrap()
}
