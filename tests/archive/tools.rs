//! The eleven tools against a seeded database with two accounts in two workspaces.
//!
//! Ported from `tests/test_mcp_tools.py`.

use chrono::{TimeDelta, Utc};
use slack_mcp::tools;
use sqlx::postgres::PgPool;

use crate::support::{self, Conv, Msg, Rev, base, db_test};

/// Two accounts: `work` in Acme, `side` in Beta. Both are in #general on Acme.
///
/// That overlap is what the dedup rule exists for, so it is present in the baseline
/// rather than bolted onto one test.
async fn seeded(pool: &PgPool) {
    support::workspace(pool, "T0001", "Acme", "acme").await;
    support::workspace(pool, "T0002", "Beta", "beta").await;
    support::account(pool, "work", "T0001", "U0ME").await;
    support::account(pool, "side", "T0002", "U9ME").await;

    for (account, id, team, kind, name) in [
        ("work", "C0GENERAL", "T0001", "channel", Some("general")),
        ("work", "D0TED", "T0001", "im", None),
        ("side", "C0GENERAL", "T0001", "channel", Some("general")),
        ("side", "C0BETA", "T0002", "channel", Some("beta-general")),
    ] {
        support::conversation(
            pool,
            Conv {
                account,
                id,
                team_id: team,
                kind,
                name,
                scope_reason: Some(if name.is_some() {
                    "named_channel"
                } else {
                    "dm"
                }),
                last_polled_at: Some(base()),
                reactions_fresh_since: Some(base() - TimeDelta::days(7)),
                ..Conv::default()
            },
        )
        .await;
    }

    support::user(pool, "work", "U0TED", Some("ted"), Some("Ted Lasso"), None).await;
    support::user(pool, "work", "U0ANN", Some("ann"), Some("Ann Rex"), None).await;
    support::user(
        pool,
        "side",
        "U9TED",
        Some("theodore"),
        Some("T. Lasso"),
        None,
    )
    .await;
}

fn msg<'a>(ts: &'a str, text: &'a str, offset_seconds: i64) -> Msg<'a> {
    Msg {
        ts,
        conversation: "C0GENERAL",
        user_id: Some("U0TED"),
        text,
        offset_seconds,
        ..Msg::default()
    }
}

async fn seed_messages(pool: &PgPool) {
    support::messages(
        pool,
        vec![
            msg("100.000000", "deploy the pipeline", 0),
            Msg {
                user_id: Some("U0ANN"),
                ..msg("101.000000", "zażółć gęślą jaźń", 600)
            },
            msg("102.000000", "thread parent", 900),
            Msg {
                thread_ts: Some("102.000000"),
                ..msg("103.000000", "in the thread", 960)
            },
            Msg {
                conversation: "D0TED",
                ..msg("200.000000", "a private word", 1200)
            },
            Msg {
                account: "side",
                conversation: "C0BETA",
                user_id: Some("U9TED"),
                ..msg("300.000000", "deploy over here", 1500)
            },
        ],
    )
    .await;
}

// ------------------------------------------------------------------------ basic reads

db_test!(list_conversations_labels_account_and_workspace, |f| {
    seeded(&f.pool).await;
    seed_messages(&f.pool).await;
    let body = tools::list_conversations(&f.pool, None, None, None)
        .await
        .unwrap();
    assert!(body.contains("[work/acme] #general"), "{body}");
    assert!(body.contains("[side/beta] #beta-general"), "{body}");
});

db_test!(a_dm_is_labelled_by_who_is_on_the_other_end, |f| {
    // A DM has no name, so without the counterpart it renders as `im:D0TED` and there is
    // no other tool that will tell you who that is.
    seeded(&f.pool).await;
    support::conversation(
        &f.pool,
        Conv {
            id: "D0TED",
            team_id: "T0001",
            kind: "im",
            name: None,
            scope_reason: Some("dm"),
            counterpart_user_id: Some("U0TED"),
            ..Conv::default()
        },
    )
    .await;
    let body = tools::list_conversations(&f.pool, None, None, None)
        .await
        .unwrap();
    assert!(body.contains("DM with Ted Lasso"), "{body}");
    assert!(!body.contains("im:D0TED"), "{body}");
});

db_test!(a_dm_alias_outranks_the_slack_name, |f| {
    seeded(&f.pool).await;
    support::conversation(
        &f.pool,
        Conv {
            id: "D0TED",
            team_id: "T0001",
            kind: "im",
            name: None,
            scope_reason: Some("dm"),
            counterpart_user_id: Some("U0TED"),
            ..Conv::default()
        },
    )
    .await;
    tools::set_alias(&f.pool, "coach", &["work:U0TED".to_string()], None)
        .await
        .unwrap();
    let body = tools::list_conversations(&f.pool, None, None, None)
        .await
        .unwrap();
    assert!(body.contains("DM with coach"), "{body}");
});

db_test!(a_dm_with_no_known_counterpart_still_renders, |f| {
    seeded(&f.pool).await;
    let body = tools::list_conversations(&f.pool, None, None, None)
        .await
        .unwrap();
    assert!(body.contains("im:D0TED"), "{body}");
});

db_test!(list_conversations_filters, |f| {
    seeded(&f.pool).await;
    let body = tools::list_conversations(&f.pool, Some("side"), Some("beta"), None)
        .await
        .unwrap();
    assert!(body.contains("#beta-general"), "{body}");
    assert!(!body.contains("work/"), "{body}");
});

db_test!(an_unknown_workspace_narrows_to_nothing, |f| {
    // A typo must not silently widen the query to every team.
    seeded(&f.pool).await;
    let body = tools::list_conversations(&f.pool, None, Some("nope"), None)
        .await
        .unwrap();
    assert!(body.contains("no in-scope conversations"), "{body}");
});

db_test!(history_is_oldest_first_and_labelled, |f| {
    seeded(&f.pool).await;
    seed_messages(&f.pool).await;
    let body = tools::history(&f.pool, "C0GENERAL", Some("work"), None, None, None)
        .await
        .unwrap();
    let pipeline = body.find("deploy the pipeline").expect("the first message");
    let parent = body.find("thread parent").expect("the later message");
    assert!(pipeline < parent, "{body}");
    assert!(body.contains("last polled:"), "{body}");
});

db_test!(history_range_filters, |f| {
    seeded(&f.pool).await;
    seed_messages(&f.pool).await;
    let after = (base() + TimeDelta::seconds(840)).to_rfc3339();
    let body = tools::history(&f.pool, "C0GENERAL", Some("work"), Some(&after), None, None)
        .await
        .unwrap();
    assert!(!body.contains("deploy the pipeline"), "{body}");
    assert!(body.contains("thread parent"), "{body}");
});

db_test!(thread_includes_the_parent, |f| {
    seeded(&f.pool).await;
    seed_messages(&f.pool).await;
    let body = tools::thread(&f.pool, "C0GENERAL", "102.000000", Some("work"))
        .await
        .unwrap();
    assert!(body.contains("thread parent"), "{body}");
    assert!(body.contains("in the thread"), "{body}");
});

db_test!(a_capped_page_says_how_many_more_the_period_holds, |f| {
    // The honesty footer, end to end: a page of 2 out of 4 must say the rest exists and
    // give the cursor that fetches it.
    seeded(&f.pool).await;
    seed_messages(&f.pool).await;
    let after = base().to_rfc3339();
    let body = tools::history(
        &f.pool,
        "C0GENERAL",
        Some("work"),
        Some(&after),
        None,
        Some(2),
    )
    .await
    .unwrap();
    assert!(body.contains("more message(s) in this period"), "{body}");
    assert!(body.contains("next page: after="), "{body}");
});

// ----------------------------------------------------------------------------- search

db_test!(search_ranks_and_snippets, |f| {
    seeded(&f.pool).await;
    seed_messages(&f.pool).await;
    let body = tools::search(
        &f.pool, "pipeline", None, None, None, None, None, None, None, None,
    )
    .await
    .unwrap();
    assert!(body.contains("deploy the pipeline"), "{body}");
    assert!(body.contains("1 result"), "{body}");
});

db_test!(search_matches_a_non_english_token, |f| {
    seeded(&f.pool).await;
    seed_messages(&f.pool).await;
    let body = tools::search(
        &f.pool, "gęślą", None, None, None, None, None, None, None, None,
    )
    .await
    .unwrap();
    assert!(body.contains("1 result"), "{body}");
});

db_test!(search_by_workspace_and_account, |f| {
    seeded(&f.pool).await;
    seed_messages(&f.pool).await;
    let beta = tools::search(
        &f.pool,
        "deploy",
        None,
        Some("beta"),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(beta.contains("deploy over here"), "{beta}");
    let acme = tools::search(
        &f.pool,
        "deploy",
        None,
        Some("acme"),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(!acme.contains("deploy over here"), "{acme}");
});

db_test!(
    a_query_matching_nothing_is_an_empty_answer_not_an_error,
    |f| {
        seeded(&f.pool).await;
        seed_messages(&f.pool).await;
        for query in ["zeppelin", "   "] {
            let body = tools::search(
                &f.pool, query, None, None, None, None, None, None, None, None,
            )
            .await
            .unwrap();
            assert!(body.contains("nothing matched"), "{query}: {body}");
        }
    }
);

db_test!(search_by_alias_spans_two_accounts, |f| {
    // `author: "ted"` means Ted anywhere, which is the whole point of aliases.
    seeded(&f.pool).await;
    seed_messages(&f.pool).await;
    tools::set_alias(
        &f.pool,
        "ted",
        &["work:U0TED".to_string(), "side:U9TED".to_string()],
        None,
    )
    .await
    .unwrap();
    let body = tools::search(
        &f.pool,
        "deploy",
        None,
        None,
        None,
        Some("ted"),
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(body.contains("deploy the pipeline"), "{body}");
    assert!(body.contains("deploy over here"), "{body}");
});

db_test!(search_by_raw_user_id_still_works, |f| {
    seeded(&f.pool).await;
    seed_messages(&f.pool).await;
    let body = tools::search(
        &f.pool,
        "deploy",
        None,
        None,
        None,
        Some("U0TED"),
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(body.contains("deploy the pipeline"), "{body}");
    assert!(!body.contains("deploy over here"), "{body}");
});

db_test!(an_unknown_author_says_so, |f| {
    seeded(&f.pool).await;
    let body = tools::search(
        &f.pool,
        "deploy",
        None,
        None,
        None,
        Some("nobody"),
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(body.contains("no user or alias matched"), "{body}");
});

db_test!(search_has_reaction_filter, |f| {
    seeded(&f.pool).await;
    seed_messages(&f.pool).await;
    support::reaction(&f.pool, "work", "C0GENERAL", "100.000000", "tada", &[], 1).await;
    let body = tools::search(
        &f.pool,
        "deploy",
        None,
        None,
        None,
        None,
        None,
        None,
        Some(true),
        None,
    )
    .await
    .unwrap();
    assert!(body.contains("deploy the pipeline"), "{body}");
    assert!(!body.contains("deploy over here"), "{body}");
});

db_test!(search_orders_by_rank_not_recency, |f| {
    // The best match buried under newer weak ones is the wrong answer.
    seeded(&f.pool).await;
    support::messages(
        &f.pool,
        vec![
            msg("500.000000", "pipeline pipeline pipeline", 0),
            msg(
                "501.000000",
                "a pipeline and much other unrelated text",
                3540,
            ),
        ],
    )
    .await;
    let body = tools::search(
        &f.pool, "pipeline", None, None, None, None, None, None, None, None,
    )
    .await
    .unwrap();
    let best = body.find("500.000000").expect("the strong match");
    let weak = body.find("501.000000").expect("the weak match");
    assert!(best < weak, "{body}");
});

// ------------------------------------------------------------------------------ dedup

db_test!(
    the_same_message_seen_by_two_accounts_collapses_to_one,
    |f| {
        // Both copies stay in the database; the collapse is a read-side courtesy.
        seeded(&f.pool).await;
        support::messages(
            &f.pool,
            vec![
                Msg {
                    account: "work",
                    ..msg("400.000000", "shared channel chatter", 0)
                },
                Msg {
                    account: "side",
                    ..msg("400.000000", "shared channel chatter", 0)
                },
            ],
        )
        .await;
        let body = tools::history(&f.pool, "C0GENERAL", None, None, None, None)
            .await
            .unwrap();
        assert_eq!(body.matches("shared channel chatter").count(), 1, "{body}");
        assert!(
            body.contains("duplicate row(s) from a second account collapsed"),
            "{body}"
        );
        // The lexically first account is the one kept.
        assert!(body.contains("[side/acme #general]"), "{body}");
        let stored: i64 =
            sqlx::query_scalar("SELECT count(*) FROM message WHERE ts = '400.000000'")
                .fetch_one(&f.pool)
                .await
                .unwrap();
        assert_eq!(stored, 2);
    }
);

db_test!(messages_in_different_workspaces_never_collapse, |f| {
    seeded(&f.pool).await;
    seed_messages(&f.pool).await;
    let since = (base() - TimeDelta::hours(1)).to_rfc3339();
    let body = tools::catch_up(&f.pool, &since, None, None, None)
        .await
        .unwrap();
    assert!(body.contains("deploy the pipeline"), "{body}");
    assert!(body.contains("deploy over here"), "{body}");
    assert!(!body.contains("collapsed"), "{body}");
});

// ------------------------------------------------------------------ mentions / catch-up

db_test!(my_mentions_finds_a_ping_in_an_in_scope_conversation, |f| {
    // The old implementation read `mention_thread`, which a DM/named-channel scope never
    // populates: those conversations are stored wholesale by earlier filter rules, so the
    // mention rule never fires and the table stays empty.
    seeded(&f.pool).await;
    seed_messages(&f.pool).await;
    support::message(&f.pool, msg("400.000000", "<@U0ME> can you look", 1500)).await;
    let body = tools::my_mentions(&f.pool, None, None, None, None)
        .await
        .unwrap();
    assert!(body.contains("can you look"), "{body}");
    assert!(body.contains("1 message(s) mentioning you"), "{body}");
});

db_test!(my_mentions_ignores_a_ping_at_someone_else, |f| {
    seeded(&f.pool).await;
    seed_messages(&f.pool).await;
    support::message(&f.pool, msg("401.000000", "<@U0OTHER> ping", 1500)).await;
    let body = tools::my_mentions(&f.pool, None, None, None, None)
        .await
        .unwrap();
    assert!(body.contains("0 message(s) mentioning you"), "{body}");
});

db_test!(catch_up_groups_by_conversation, |f| {
    seeded(&f.pool).await;
    seed_messages(&f.pool).await;
    let since = (base() - TimeDelta::hours(1)).to_rfc3339();
    let body = tools::catch_up(&f.pool, &since, None, None, None)
        .await
        .unwrap();
    assert!(body.contains("[work/acme] #general"), "{body}");
    assert!(body.contains("[work/acme] im:D0TED"), "{body}");
});

db_test!(catch_up_accepts_a_slack_ts, |f| {
    seeded(&f.pool).await;
    seed_messages(&f.pool).await;
    let since = format!("{:.6}", (base() - TimeDelta::hours(1)).timestamp() as f64);
    let body = tools::catch_up(&f.pool, &since, None, None, None)
        .await
        .unwrap();
    assert!(body.contains("a private word"), "{body}");
});

db_test!(an_unreadable_since_is_a_usage_error, |f| {
    seeded(&f.pool).await;
    let error = tools::catch_up(&f.pool, "last tuesday", None, None, None)
        .await
        .unwrap_err();
    assert!(format!("{error}").contains("could not read"), "{error}");
});

// ---------------------------------------------------------------------------- aliases

db_test!(setting_an_alias_reports_a_move, |f| {
    seeded(&f.pool).await;
    tools::set_alias(&f.pool, "ted", &["work:U0TED".to_string()], None)
        .await
        .unwrap();
    let body = tools::set_alias(
        &f.pool,
        "coach",
        &["work:U0TED".to_string()],
        Some("the coach"),
    )
    .await
    .unwrap();
    assert!(body.contains("moved off another alias"), "{body}");
    assert!(body.contains("work:U0TED"), "{body}");
});

db_test!(an_empty_member_list_deletes_the_alias, |f| {
    seeded(&f.pool).await;
    tools::set_alias(&f.pool, "ted", &["work:U0TED".to_string()], None)
        .await
        .unwrap();
    let body = tools::set_alias(&f.pool, "ted", &[], None).await.unwrap();
    assert!(body.contains("deleted"), "{body}");
    let listing = tools::list_aliases(&f.pool, None).await.unwrap();
    assert!(listing.contains("no aliases set"), "{listing}");
});

db_test!(a_malformed_member_is_rejected, |f| {
    seeded(&f.pool).await;
    let error = tools::set_alias(&f.pool, "ted", &["U0TED".to_string()], None)
        .await
        .unwrap_err();
    assert!(format!("{error}").contains("account:user_id"), "{error}");
    // And nothing was written on the way to the error.
    let aliases: i64 = sqlx::query_scalar("SELECT count(*) FROM user_alias")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(aliases, 0);
});

db_test!(list_aliases_resolves_members_to_names, |f| {
    seeded(&f.pool).await;
    tools::set_alias(
        &f.pool,
        "ted",
        &["work:U0TED".to_string(), "side:U9TED".to_string()],
        Some("same person"),
    )
    .await
    .unwrap();
    let body = tools::list_aliases(&f.pool, None).await.unwrap();
    assert!(body.contains("Ted Lasso"), "{body}");
    assert!(body.contains("T. Lasso"), "{body}");
    assert!(body.contains("same person"), "{body}");
});

// ----------------------------------------------------------------------------- status

db_test!(sync_status_reports_progress_and_a_dead_session, |f| {
    seeded(&f.pool).await;
    let body = tools::sync_status(&f.pool, Some("work")).await.unwrap();
    assert!(body.contains("0/2 conversations backfilled"), "{body}");

    support::auth_error(&f.pool, "work", "invalid_auth").await;
    let body = tools::sync_status(&f.pool, Some("work")).await.unwrap();
    assert!(body.contains("invalid_auth"), "{body}");
    assert!(
        body.contains("frozen until the token is replaced"),
        "{body}"
    );
});

db_test!(list_workspaces_counts_conversations, |f| {
    seeded(&f.pool).await;
    let body = tools::list_workspaces(&f.pool).await.unwrap();
    assert!(body.contains("acme"), "{body}");
    assert!(body.contains("beta"), "{body}");
});

db_test!(history_names_the_conversation_without_an_account, |f| {
    seeded(&f.pool).await;
    seed_messages(&f.pool).await;
    let body = tools::history(&f.pool, "C0GENERAL", None, None, None, None)
        .await
        .unwrap();
    assert!(body.contains("#general"), "{body}");
});

// -------------------------------------------------------------------------- revisions

db_test!(revisions_show_the_text_every_other_view_withholds, |f| {
    // The only place a deleted message's text is shown. Asking for the history is
    // asking for exactly this.
    seeded(&f.pool).await;
    support::message(
        &f.pool,
        Msg {
            deleted_at: Some(Utc::now()),
            ..msg("600.000000", "the final wording", 0)
        },
    )
    .await;
    let key = Rev {
        conversation: "C0GENERAL",
        ts: "600.000000",
        ..Rev::default()
    };
    support::revision(
        &f.pool,
        Rev {
            seq: 1,
            text: "the first wording",
            superseded_at: Some(base()),
            ..key
        },
    )
    .await;
    support::revision(
        &f.pool,
        Rev {
            seq: 2,
            text: "the final wording",
            first_seen_at: Some(base()),
            ..key
        },
    )
    .await;

    let body = tools::revisions(&f.pool, "C0GENERAL", "600.000000", None)
        .await
        .unwrap();
    assert!(body.contains("the first wording"), "{body}");
    assert!(body.contains("the final wording"), "{body}");
    assert!(body.contains("2 revision(s)"), "{body}");
    assert!(body.contains("deleted "), "{body}");
    // And the default view still withholds it.
    let history = tools::history(&f.pool, "C0GENERAL", None, None, None, None)
        .await
        .unwrap();
    assert!(history.contains("[deleted]"), "{history}");
    assert!(!history.contains("the final wording"), "{history}");
});

db_test!(a_message_the_archive_does_not_hold_is_not_an_error, |f| {
    seeded(&f.pool).await;
    let body = tools::revisions(&f.pool, "C0GENERAL", "999.000000", None)
        .await
        .unwrap();
    assert!(body.contains("no message 999.000000"), "{body}");
});

// -------------------------------------------------------------------- custom emoji

db_test!(
    custom_emoji_are_listed_when_they_resolve_in_the_workspace,
    |f| {
        seeded(&f.pool).await;
        support::emoji(&f.pool, "T0001", "shipit").await;
        support::message(
            &f.pool,
            msg("700.000000", "ship it :shipit: :nosuchemoji:", 0),
        )
        .await;
        let body = tools::history(&f.pool, "C0GENERAL", Some("work"), None, None, None)
            .await
            .unwrap();
        assert!(body.contains("custom emoji seen: :shipit:"), "{body}");
    }
);
