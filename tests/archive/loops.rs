//! Poll, backfill and discovery against a real database and a fake Slack.
//!
//! Ported from `tests/test_ingest_loops.py`. The pure cases from that file —
//! `is_due`'s cadence — live in `crates/ingest/src/poll.rs`'s unit tests, where they
//! need no container.

use std::collections::HashSet;

use chrono::{TimeDelta, Utc};
use serde_json::{Value, json};
use slack_core::config::IngestSettings;
use slack_core::models::{Account, SyncLoop, Workspace};
use slack_core::store;
use slack_ingest::backfill::{POLL_RESERVE, backfill_step};
use slack_ingest::discovery::discover;
use slack_ingest::pages::IngestContext;
use slack_ingest::poll::{PollPass, poll_conversation, poll_once, to_slack_ts};
use slack_ingest::runner::{BootError, boot, poll_loop};
use sqlx::postgres::PgPool;

use crate::fake_slack::{FakeSlack, Raw, raw};
use crate::support::db_test;

const ME: &str = "U0ME";
const TEAM: &str = "T0001";

fn context() -> IngestContext {
    context_for("work", &["general"])
}

fn context_for(account: &str, scope: &[&str]) -> IngestContext {
    IngestContext::new(
        account.to_owned(),
        ME.to_owned(),
        scope.iter().map(|name| (*name).to_owned()).collect(),
    )
}

fn pass<'a>() -> PollPass<'a> {
    PollPass::new(TEAM, 7)
}

fn settings(poll_interval: u64) -> IngestSettings {
    IngestSettings::from_source(move |name| match name {
        "SLACK_ACCOUNT" => Some("work".to_owned()),
        "SLACK_XOXC" => Some("xoxc-x".to_owned()),
        "SLACK_XOXD" => Some("xoxd-x".to_owned()),
        "SLACK_DSN" => Some("postgresql://u:p@localhost/slack".to_owned()),
        "SLACK_SCOPE_CHANNELS" => Some("#general".to_owned()),
        "SLACK_POLL_INTERVAL" => Some(poll_interval.to_string()),
        _ => None,
    })
    .expect("settings")
}

async fn seed_account(pool: &PgPool, account: &str) {
    store::upsert_workspace(
        pool,
        &Workspace {
            team_id: TEAM.into(),
            name: Some("Acme".into()),
            domain: None,
            is_external: false,
        },
    )
    .await
    .unwrap();
    store::upsert_account(
        pool,
        &Account {
            id: account.to_owned(),
            team_id: TEAM.into(),
            my_user_id: ME.into(),
            enabled: true,
            last_auth_error: None,
            last_auth_error_at: None,
            updated_at: None,
        },
    )
    .await
    .unwrap();
}

/// A ts inside the reaction-refresh window.
///
/// The fixtures' small timestamps sit in 1970, which the window correctly ignores.
fn recent_ts(seconds_ago: i64) -> String {
    to_slack_ts(Utc::now() - TimeDelta::seconds(seconds_ago))
}

async fn texts(pool: &PgPool, conversation_id: &str) -> Vec<String> {
    store::get_history(pool, conversation_id, None, None, None, 500)
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.text)
        .collect()
}

async fn sorted_texts(pool: &PgPool, conversation_id: &str) -> Vec<String> {
    let mut texts = texts(pool, conversation_id).await;
    texts.sort();
    texts
}

async fn scalar<T>(pool: &PgPool, sql: &'static str) -> T
where
    T: for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres> + Send + Unpin,
{
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

// -------------------------------------------------------------------------- discovery

db_test!(discovery_sets_scope_from_the_conversation_rules, |f| {
    seed_account(&f.pool, "work").await;
    let slack = FakeSlack::new();
    discover(&f.pool, &slack, "work", TEAM, &context().scope_channels)
        .await
        .unwrap();

    let mut rows: Vec<(String, bool, Option<String>)> =
        sqlx::query_as("SELECT id, in_scope, scope_reason FROM conversation ORDER BY id")
            .fetch_all(&f.pool)
            .await
            .unwrap();
    rows.sort();
    assert_eq!(
        rows,
        [
            (
                "C0GENERAL".to_owned(),
                true,
                Some("named_channel".to_owned())
            ),
            ("C0RANDOM".to_owned(), false, None),
            ("D0TED".to_owned(), true, Some("dm".to_owned())),
        ]
    );
});

db_test!(leaving_a_channel_keeps_its_messages, |f| {
    // A channel you left does not retroactively become something you never saw.
    seed_account(&f.pool, "work").await;
    let slack = FakeSlack::new().with_history(
        "C0GENERAL",
        vec![raw(Raw {
            text: "kept",
            ..Raw::default()
        })],
    );
    let ctx = context();
    discover(&f.pool, &slack, "work", TEAM, &ctx.scope_channels)
        .await
        .unwrap();
    poll_once(&f.pool, &slack, &ctx, &pass()).await.unwrap();

    slack.set_conversations(
        crate::fake_slack::channels()
            .into_iter()
            .filter(|row| row["id"] != "C0GENERAL")
            .collect(),
    );
    discover(&f.pool, &slack, "work", TEAM, &ctx.scope_channels)
        .await
        .unwrap();

    let is_member: bool = scalar(
        &f.pool,
        "SELECT is_member FROM conversation WHERE id = 'C0GENERAL'",
    )
    .await;
    assert!(!is_member);
    assert_eq!(texts(&f.pool, "C0GENERAL").await, ["kept"]);
});

db_test!(external_teams_are_registered_without_a_name, |f| {
    // `team.info` on a foreign team is not reliably available; a named row would claim
    // we verified something we did not.
    seed_account(&f.pool, "work").await;
    let slack = FakeSlack::new().with_conversations(vec![json!({
        "id": "C0PARTNER",
        "name": "acme-partner",
        "is_channel": true,
        "is_member": true,
        "is_ext_shared": true,
        "context_team_id": TEAM,
        "shared_team_ids": [TEAM, "T0EXT"],
    })]);
    discover(&f.pool, &slack, "work", TEAM, &HashSet::new())
        .await
        .unwrap();

    let row: (Option<String>, bool) =
        sqlx::query_as("SELECT name, is_external FROM workspace WHERE team_id = 'T0EXT'")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(row.0, None);
    assert!(row.1);
});

db_test!(
    a_channel_that_cannot_be_inspected_keeps_its_listing_attribution,
    |f| {
        seed_account(&f.pool, "work").await;
        let slack = FakeSlack::new().with_conversations(vec![json!({
            "id": "C0PARTNER",
            "name": "acme-partner",
            "is_channel": true,
            "is_member": true,
            "is_ext_shared": true,
        })]);
        slack.fail("conversations.info", "channel_not_found");
        discover(&f.pool, &slack, "work", TEAM, &HashSet::new())
            .await
            .unwrap();
        let team_id: Option<String> = scalar(
            &f.pool,
            "SELECT team_id FROM conversation WHERE id = 'C0PARTNER'",
        )
        .await;
        assert_eq!(team_id.as_deref(), Some(TEAM));
    }
);

// ------------------------------------------------------------------------- poll loop

db_test!(poll_stores_only_what_the_filter_keeps, |f| {
    seed_account(&f.pool, "work").await;
    let slack = FakeSlack::new()
        .with_history(
            "C0GENERAL",
            vec![raw(Raw {
                text: "in the named channel",
                ..Raw::default()
            })],
        )
        .with_history(
            "C0RANDOM",
            vec![
                raw(Raw {
                    ts: "200.000000",
                    text: "chatter",
                    ..Raw::default()
                }),
                raw(Raw {
                    ts: "201.000000",
                    text: "joined",
                    subtype: Some("channel_join"),
                    ..Raw::default()
                }),
            ],
        )
        .with_history(
            "D0TED",
            vec![raw(Raw {
                ts: "300.000000",
                text: "a direct message",
                ..Raw::default()
            })],
        );
    poll_once(&f.pool, &slack, &context(), &pass())
        .await
        .unwrap();
    assert_eq!(texts(&f.pool, "C0GENERAL").await, ["in the named channel"]);
    assert!(texts(&f.pool, "C0RANDOM").await.is_empty());
    assert_eq!(texts(&f.pool, "D0TED").await, ["a direct message"]);
});

db_test!(a_mention_pulls_in_the_whole_thread, |f| {
    // Storing only the ping would leave the thread with a hole where the context was.
    seed_account(&f.pool, "work").await;
    let slack = FakeSlack::new().with_history(
        "C0RANDOM",
        vec![
            raw(Raw {
                text: "setting up the release",
                ..Raw::default()
            }),
            raw(Raw {
                ts: "101.000000",
                text: "<@U0ME> can you look?",
                thread_ts: Some("100.000000"),
                ..Raw::default()
            }),
            raw(Raw {
                ts: "102.000000",
                text: "here is the log",
                thread_ts: Some("100.000000"),
                ..Raw::default()
            }),
        ],
    );
    poll_once(&f.pool, &slack, &context(), &pass())
        .await
        .unwrap();

    let thread = store::get_thread(&f.pool, "C0RANDOM", "100.000000", None)
        .await
        .unwrap();
    assert_eq!(
        thread.iter().map(|m| m.ts.as_str()).collect::<Vec<_>>(),
        ["100.000000", "101.000000", "102.000000"]
    );
    let row: (String, String) = sqlx::query_as(
        "SELECT thread_ts, mention_ts FROM mention_thread WHERE conversation_id = 'C0RANDOM'",
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(row, ("100.000000".to_owned(), "101.000000".to_owned()));
    // The mention is also what brings the channel into scope.
    let in_scope: bool = scalar(
        &f.pool,
        "SELECT in_scope FROM conversation WHERE id = 'C0RANDOM'",
    )
    .await;
    assert!(in_scope);
});

db_test!(a_later_reply_in_a_registered_thread_is_captured, |f| {
    seed_account(&f.pool, "work").await;
    let slack = FakeSlack::new().with_history(
        "C0RANDOM",
        vec![
            raw(Raw {
                text: "topic",
                ..Raw::default()
            }),
            raw(Raw {
                ts: "101.000000",
                text: "<@U0ME> look",
                thread_ts: Some("100.000000"),
                ..Raw::default()
            }),
        ],
    );
    let ctx = context();
    poll_once(&f.pool, &slack, &ctx, &pass()).await.unwrap();

    slack.push_history(
        "C0RANDOM",
        raw(Raw {
            ts: "102.000000",
            text: "later reply",
            thread_ts: Some("100.000000"),
            ..Raw::default()
        }),
    );
    poll_once(&f.pool, &slack, &ctx, &pass()).await.unwrap();
    assert!(
        texts(&f.pool, "C0RANDOM")
            .await
            .contains(&"later reply".to_owned())
    );
});

db_test!(the_poll_cursor_advances_only_after_the_writes_commit, |f| {
    // A crash must re-read a page, never skip one: a re-read costs an idempotent upsert,
    // a skip loses a message permanently. The Python monkeypatched `upsert_messages`;
    // here the write is made to fail where it actually happens, with a trigger.
    seed_account(&f.pool, "work").await;
    let slack = FakeSlack::new().with_history(
        "C0GENERAL",
        vec![raw(Raw {
            text: "unlucky",
            ..Raw::default()
        })],
    );
    let ctx = context();
    discover(&f.pool, &slack, "work", TEAM, &ctx.scope_channels)
        .await
        .unwrap();
    let mut conversation = store::get_conversation(&f.pool, "work", "C0GENERAL")
        .await
        .unwrap()
        .expect("C0GENERAL");

    sqlx::raw_sql(
        "CREATE OR REPLACE FUNCTION refuse_write() RETURNS trigger AS $$
         BEGIN RAISE EXCEPTION 'write failed'; END $$ LANGUAGE plpgsql;
         CREATE TRIGGER refuse_message BEFORE INSERT ON message
           FOR EACH ROW EXECUTE FUNCTION refuse_write();",
    )
    .execute(&f.pool)
    .await
    .unwrap();

    let result = poll_conversation(&f.pool, &slack, &mut conversation, &ctx, &pass()).await;

    sqlx::raw_sql("DROP TRIGGER refuse_message ON message")
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(result.is_err(), "the write was supposed to fail");

    let cursor: Option<String> = scalar(
        &f.pool,
        "SELECT poll_cursor FROM conversation WHERE id = 'C0GENERAL'",
    )
    .await;
    assert_eq!(cursor, None);
});

db_test!(the_second_pass_only_asks_for_what_is_new, |f| {
    seed_account(&f.pool, "work").await;
    let slack = FakeSlack::new().with_history(
        "C0GENERAL",
        vec![raw(Raw {
            text: "first",
            ..Raw::default()
        })],
    );
    let ctx = context();
    poll_once(&f.pool, &slack, &ctx, &pass()).await.unwrap();
    let cursor: Option<String> = scalar(
        &f.pool,
        "SELECT poll_cursor FROM conversation WHERE id = 'C0GENERAL'",
    )
    .await;
    assert_eq!(cursor.as_deref(), Some("100.000000"));

    slack.clear_calls();
    slack.push_history(
        "C0GENERAL",
        raw(Raw {
            ts: "200.000000",
            text: "second",
            ..Raw::default()
        }),
    );
    poll_once(&f.pool, &slack, &ctx, &pass()).await.unwrap();
    let asked: Vec<Option<String>> = slack
        .history_calls("C0GENERAL")
        .into_iter()
        .map(|call| call.oldest)
        .collect();
    assert!(asked.contains(&Some("100.000000".to_owned())), "{asked:?}");
    assert_eq!(
        sorted_texts(&f.pool, "C0GENERAL").await,
        ["first", "second"]
    );
});

db_test!(the_poll_asks_forward_and_the_backfill_asks_backward, |f| {
    // The one swap that is silent in production: `oldest` and `latest` are one
    // keystroke apart, and exchanging them makes the poll re-read all of history and
    // the backfill crawl forwards into nothing. Both directions are asserted here, in
    // one place, so a swap cannot pass.
    seed_account(&f.pool, "work").await;
    let slack = FakeSlack::new().with_history(
        "C0GENERAL",
        (0..3)
            .map(|i| {
                raw(Raw {
                    ts: &format!("10{i}.000000"),
                    ..Raw::default()
                })
            })
            .collect(),
    );
    let ctx = context();
    discover(&f.pool, &slack, "work", TEAM, &ctx.scope_channels)
        .await
        .unwrap();
    poll_once(&f.pool, &slack, &ctx, &pass()).await.unwrap();

    // The poll walks forward: it bounds the window from below and never from above.
    let poll_calls = slack.history_calls("C0GENERAL");
    assert!(
        poll_calls.iter().all(|call| call.latest.is_none()),
        "the poll asked for a `latest`: {poll_calls:?}"
    );
    // The first pass has no cursor and no floor, so `oldest` is None; the second pass
    // has one, and it is the newest ts it stored.
    poll_once(&f.pool, &slack, &ctx, &pass()).await.unwrap();
    let second = slack.history_calls("C0GENERAL");
    assert!(
        second
            .iter()
            .any(|call| call.oldest.as_deref() == Some("102.000000")),
        "the poll did not ask forward from its cursor: {second:?}"
    );

    // The backfill walks backward: it bounds from above and never from below.
    slack.clear_calls();
    let mut skip = HashSet::new();
    backfill_step(&f.pool, &slack, &ctx, 2, &mut skip)
        .await
        .unwrap();
    let backfill_calls = slack.history_calls("C0GENERAL");
    assert!(
        backfill_calls.iter().all(|call| call.oldest.is_none()),
        "the backfill asked for an `oldest`: {backfill_calls:?}"
    );
    slack.clear_calls();
    backfill_step(&f.pool, &slack, &ctx, 2, &mut skip)
        .await
        .unwrap();
    let resumed = slack.history_calls("C0GENERAL");
    assert_eq!(
        resumed.first().and_then(|call| call.latest.clone()),
        Some("101.000000".to_owned()),
        "the backfill did not resume from its cursor: {resumed:?}"
    );
});

db_test!(one_bad_conversation_does_not_stop_the_pass, |f| {
    seed_account(&f.pool, "work").await;
    let slack = FakeSlack::new().with_history(
        "D0TED",
        vec![raw(Raw {
            ts: "300.000000",
            text: "a direct message",
            ..Raw::default()
        })],
    );
    slack.fail("conversations.history:C0GENERAL", "internal_error");
    poll_once(&f.pool, &slack, &context(), &pass())
        .await
        .unwrap();

    assert_eq!(texts(&f.pool, "D0TED").await, ["a direct message"]);
    let states = store::get_sync_state(&f.pool, "work").await.unwrap();
    let poll = states
        .iter()
        .find(|state| state.r#loop == SyncLoop::Poll)
        .expect("a poll state");
    let error = poll.last_error.as_deref().expect("an error");
    assert!(error.contains("C0GENERAL"), "{error}");
});

db_test!(paging_reads_every_page, |f| {
    seed_account(&f.pool, "work").await;
    let history: Vec<Value> = (0..25)
        .map(|i| {
            raw(Raw {
                ts: &format!("{}.000000", 100 + i),
                text: &format!("m{i}"),
                ..Raw::default()
            })
        })
        .collect();
    let slack = FakeSlack::new()
        .with_history("C0GENERAL", history)
        .with_page_size(10);
    poll_once(&f.pool, &slack, &context(), &pass())
        .await
        .unwrap();
    assert_eq!(texts(&f.pool, "C0GENERAL").await.len(), 25);
});

// ------------------------------------------------------------------------- reactions

db_test!(reaction_refresh_clears_a_reaction_removed_upstream, |f| {
    // Slack reports reactions as a snapshot, so only re-reading reveals a removal. The
    // message has to be inside the refresh window for this to happen at all — that is the
    // whole point of `reactions_fresh_since`, and a message older than the window keeps
    // whatever count was last observed.
    seed_account(&f.pool, "work").await;
    let ts = recent_ts(60);
    let slack = FakeSlack::new().with_history(
        "C0GENERAL",
        vec![raw(Raw {
            ts: &ts,
            text: "ship it",
            reactions: Some(json!([{ "name": "tada", "users": ["U0ANN"], "count": 1 }])),
            ..Raw::default()
        })],
    );
    let ctx = context();
    poll_once(&f.pool, &slack, &ctx, &pass()).await.unwrap();
    assert_eq!(
        scalar::<i64>(&f.pool, "SELECT count(*) FROM reaction").await,
        1
    );

    slack.edit_message("C0GENERAL", &ts, |row| {
        row.as_object_mut().expect("an object").remove("reactions");
    });
    poll_once(&f.pool, &slack, &ctx, &pass()).await.unwrap();
    assert_eq!(
        scalar::<i64>(&f.pool, "SELECT count(*) FROM reaction").await,
        0
    );
});

db_test!(
    reaction_freshness_is_recorded_even_for_a_silent_conversation,
    |f| {
        // The guarantee is about the time range, not about whether anything was found.
        seed_account(&f.pool, "work").await;
        poll_once(&f.pool, &FakeSlack::new(), &context(), &pass())
            .await
            .unwrap();
        let fresh: Option<chrono::DateTime<Utc>> = scalar(
            &f.pool,
            "SELECT reactions_fresh_since FROM conversation WHERE id = 'C0GENERAL'",
        )
        .await;
        assert!(fresh.is_some());
    }
);

db_test!(reaction_refresh_only_reads_the_window, |f| {
    seed_account(&f.pool, "work").await;
    let slack = FakeSlack::new().with_history(
        "C0GENERAL",
        vec![
            raw(Raw::default()),
            raw(Raw {
                ts: &recent_ts(60),
                ..Raw::default()
            }),
        ],
    );
    poll_once(&f.pool, &slack, &context(), &pass())
        .await
        .unwrap();
    let windows: Vec<Option<String>> = slack
        .history_calls("C0GENERAL")
        .into_iter()
        .map(|call| call.oldest)
        .collect();
    // The last call of the pass is the refresh, and it starts inside the window.
    let last = windows.last().expect("a call").clone().expect("an oldest");
    assert!(
        last.parse::<f64>().unwrap() > 100.0,
        "the refresh read from the beginning: {last}"
    );
});

db_test!(a_window_holding_nothing_is_not_read, |f| {
    // There is nothing in it to reconcile, and a read per silent conversation per pass is
    // most of what a pass used to cost.
    seed_account(&f.pool, "work").await;
    let slack = FakeSlack::new().with_history("C0GENERAL", vec![raw(Raw::default())]);
    poll_once(&f.pool, &slack, &context(), &pass())
        .await
        .unwrap();
    assert_eq!(
        slack.history_calls("C0GENERAL").len(),
        1,
        "the poll, and no refresh"
    );
    let fresh: Option<chrono::DateTime<Utc>> = scalar(
        &f.pool,
        "SELECT reactions_fresh_since FROM conversation WHERE id = 'C0GENERAL'",
    )
    .await;
    assert!(fresh.is_some());
});

db_test!(
    replies_to_a_stored_parent_land_in_the_pass_that_notices_them,
    |f| {
        // The parent was stored before anyone replied, so only the refresh sees it gain
        // replies. Filling threads before the refresh left them waiting a whole pass.
        seed_account(&f.pool, "work").await;
        let parent_ts = recent_ts(120);
        let slack = FakeSlack::new().with_history(
            "D0TED",
            vec![raw(Raw {
                ts: &parent_ts,
                text: "ping me when free",
                ..Raw::default()
            })],
        );
        let ctx = context();
        poll_once(&f.pool, &slack, &ctx, &pass()).await.unwrap();

        slack.edit_message("D0TED", &parent_ts, |row| {
            let object = row.as_object_mut().expect("an object");
            object.insert("thread_ts".into(), json!(object["ts"].clone()));
            object.insert("reply_count".into(), json!(1));
        });
        slack.set_replies(
            "D0TED",
            vec![raw(Raw {
                ts: &recent_ts(60),
                text: "free now",
                thread_ts: Some(&parent_ts),
                ..Raw::default()
            })],
        );
        poll_once(&f.pool, &slack, &ctx, &pass()).await.unwrap();
        assert!(
            texts(&f.pool, "D0TED")
                .await
                .contains(&"free now".to_owned()),
            "the reply did not land in the pass that noticed it"
        );
    }
);

db_test!(
    the_poll_pass_skips_dormant_conversations_off_their_slot,
    |f| {
        seed_account(&f.pool, "work").await;
        let slack = FakeSlack::new()
            .with_history(
                "C0GENERAL",
                vec![raw(Raw {
                    text: "long ago",
                    ..Raw::default()
                })],
            )
            .with_history(
                "D0TED",
                vec![raw(Raw {
                    ts: &recent_ts(60),
                    text: "today",
                    ..Raw::default()
                })],
            );
        let ctx = context();
        poll_once(&f.pool, &slack, &ctx, &pass()).await.unwrap();

        slack.clear_calls();
        for number in 0..5 {
            let mut pass = pass();
            pass.pass_number = number;
            pass.dormant_days = Some(30);
            pass.dormant_every = 5;
            poll_once(&f.pool, &slack, &ctx, &pass).await.unwrap();
        }
        let polls: Vec<String> = slack
            .calls_to("conversations.history")
            .into_iter()
            .filter(|call| call.oldest.is_some())
            .filter_map(|call| call.channel)
            .collect();
        assert_eq!(
            polls.iter().filter(|id| *id == "C0GENERAL").count(),
            1,
            "{polls:?}"
        );
        // Read every pass by the poll, and by the refresh because its window is not empty.
        assert_eq!(polls.iter().filter(|id| *id == "D0TED").count(), 10);
    }
);

// ---------------------------------------------------------------------- backfill loop

db_test!(backfill_pages_backwards_and_terminates, |f| {
    seed_account(&f.pool, "work").await;
    let history: Vec<Value> = (0..25)
        .map(|i| {
            raw(Raw {
                ts: &format!("{}.000000", 100 + i),
                text: &format!("m{i}"),
                ..Raw::default()
            })
        })
        .collect();
    let slack = FakeSlack::new()
        .with_history("C0GENERAL", history)
        .with_page_size(10);
    let ctx = context();
    discover(&f.pool, &slack, "work", TEAM, &ctx.scope_channels)
        .await
        .unwrap();

    let mut skip = HashSet::new();
    let mut steps = 0;
    while backfill_step(&f.pool, &slack, &ctx, 10, &mut skip)
        .await
        .unwrap()
        && steps < 20
    {
        steps += 1;
        let all_done: Option<bool> = scalar(
            &f.pool,
            "SELECT bool_and(backfill_done) FROM conversation WHERE in_scope",
        )
        .await;
        if all_done == Some(true) {
            break;
        }
    }

    assert_eq!(texts(&f.pool, "C0GENERAL").await.len(), 25);
    let latests: Vec<Option<String>> = slack
        .history_calls("C0GENERAL")
        .into_iter()
        .map(|call| call.latest)
        .collect();
    // Each page asks for history older than the last page's oldest message.
    assert_eq!(latests[0], None);
    assert_eq!(latests[1].as_deref(), Some("115.000000"));
});

db_test!(backfill_resumes_from_its_cursor, |f| {
    seed_account(&f.pool, "work").await;
    let history: Vec<Value> = (0..25)
        .map(|i| {
            raw(Raw {
                ts: &format!("{}.000000", 100 + i),
                text: &format!("m{i}"),
                ..Raw::default()
            })
        })
        .collect();
    let slack = FakeSlack::new()
        .with_history("C0GENERAL", history)
        .with_page_size(10);
    let ctx = context();
    discover(&f.pool, &slack, "work", TEAM, &ctx.scope_channels)
        .await
        .unwrap();
    let mut skip = HashSet::new();
    backfill_step(&f.pool, &slack, &ctx, 10, &mut skip)
        .await
        .unwrap();

    // A restart loses everything except what the database recorded.
    slack.clear_calls();
    let mut fresh_skip = HashSet::new();
    backfill_step(&f.pool, &slack, &ctx, 10, &mut fresh_skip)
        .await
        .unwrap();
    let asked: Vec<Option<String>> = slack
        .history_calls("C0GENERAL")
        .into_iter()
        .map(|call| call.latest)
        .collect();
    assert_eq!(asked, [Some("115.000000".to_owned())]);
});

db_test!(an_empty_conversation_is_marked_done_immediately, |f| {
    // Otherwise the loop spins on a null cursor forever.
    seed_account(&f.pool, "work").await;
    let slack = FakeSlack::new();
    let ctx = context();
    discover(&f.pool, &slack, "work", TEAM, &ctx.scope_channels)
        .await
        .unwrap();
    let mut skip = HashSet::new();
    // One step per in-scope conversation, each finishing on its first empty page.
    for _ in 0..2 {
        assert!(
            backfill_step(&f.pool, &slack, &ctx, 200, &mut skip)
                .await
                .unwrap()
        );
    }
    let all_done: Option<bool> = scalar(
        &f.pool,
        "SELECT bool_and(backfill_done) FROM conversation WHERE in_scope",
    )
    .await;
    assert_eq!(all_done, Some(true));
    assert!(
        !backfill_step(&f.pool, &slack, &ctx, 200, &mut skip)
            .await
            .unwrap()
    );
});

db_test!(
    a_conversation_that_left_scope_stops_collecting_but_keeps_its_rows,
    |f| {
        seed_account(&f.pool, "work").await;
        let history: Vec<Value> = (0..25)
            .map(|i| {
                raw(Raw {
                    ts: &format!("{}.000000", 100 + i),
                    ..Raw::default()
                })
            })
            .collect();
        let slack = FakeSlack::new().with_history("C0GENERAL", history);
        let ctx = context();
        discover(&f.pool, &slack, "work", TEAM, &ctx.scope_channels)
            .await
            .unwrap();
        let mut skip = HashSet::new();
        backfill_step(&f.pool, &slack, &ctx, 5, &mut skip)
            .await
            .unwrap();
        let before = texts(&f.pool, "C0GENERAL").await.len();

        // The channel is removed from SLACK_SCOPE_CHANNELS.
        let narrowed = context_for("work", &[]);
        assert!(
            backfill_step(&f.pool, &slack, &narrowed, 5, &mut skip)
                .await
                .unwrap()
        );
        let done: bool = scalar(
            &f.pool,
            "SELECT backfill_done FROM conversation WHERE id = 'C0GENERAL'",
        )
        .await;
        assert!(done);
        // Deleting history because a config line changed would be a destructive act.
        assert_eq!(texts(&f.pool, "C0GENERAL").await.len(), before);
    }
);

db_test!(backfill_still_finishes_a_conversation_that_was_left, |f| {
    // Abandoning it half-crawled would leave a permanent hole with no record of why.
    seed_account(&f.pool, "work").await;
    let history: Vec<Value> = (0..12)
        .map(|i| {
            raw(Raw {
                ts: &format!("{}.000000", 100 + i),
                ..Raw::default()
            })
        })
        .collect();
    let slack = FakeSlack::new().with_history("C0GENERAL", history);
    let ctx = context();
    discover(&f.pool, &slack, "work", TEAM, &ctx.scope_channels)
        .await
        .unwrap();
    store::set_conversation_membership(&f.pool, "work", "C0GENERAL", false)
        .await
        .unwrap();
    let mut skip = HashSet::new();
    while backfill_step(&f.pool, &slack, &ctx, 5, &mut skip)
        .await
        .unwrap()
    {
        let done: bool = scalar(
            &f.pool,
            "SELECT backfill_done FROM conversation WHERE id = 'C0GENERAL'",
        )
        .await;
        if done {
            break;
        }
    }
    assert_eq!(texts(&f.pool, "C0GENERAL").await.len(), 12);
});

db_test!(a_failing_conversation_is_skipped_not_marked_complete, |f| {
    // Marking it done would record "history complete" for a page never read. The failure
    // is remembered for this run only, so a restart tries again.
    seed_account(&f.pool, "work").await;
    let slack = FakeSlack::new().with_history(
        "D0TED",
        vec![raw(Raw {
            text: "a direct message",
            ..Raw::default()
        })],
    );
    slack.fail("conversations.history:C0GENERAL", "internal_error");
    let ctx = context();
    discover(&f.pool, &slack, "work", TEAM, &ctx.scope_channels)
        .await
        .unwrap();

    let mut skip = HashSet::new();
    for _ in 0..3 {
        backfill_step(&f.pool, &slack, &ctx, 200, &mut skip)
            .await
            .unwrap();
    }

    assert_eq!(skip.iter().cloned().collect::<Vec<_>>(), ["C0GENERAL"]);
    let general: bool = scalar(
        &f.pool,
        "SELECT backfill_done FROM conversation WHERE id = 'C0GENERAL'",
    )
    .await;
    assert!(!general);
    // The healthy conversation is still crawled in the same run.
    let ted: bool = scalar(
        &f.pool,
        "SELECT backfill_done FROM conversation WHERE id = 'D0TED'",
    )
    .await;
    assert!(ted);
    let states = store::get_sync_state(&f.pool, "work").await.unwrap();
    let backfill = states
        .iter()
        .find(|state| state.r#loop == SyncLoop::Backfill)
        .expect("a backfill state");
    assert!(backfill.last_error.is_some());
});

db_test!(backfill_marks_its_calls_as_background_work, |f| {
    // FIFO alone would split the budget with polling, doubling how long a poll pass takes
    // to surface a message from a minute ago.
    seed_account(&f.pool, "work").await;
    let slack = FakeSlack::new().with_history("C0GENERAL", vec![raw(Raw::default())]);
    let ctx = context();
    discover(&f.pool, &slack, "work", TEAM, &ctx.scope_channels)
        .await
        .unwrap();

    slack.clear_calls();
    let mut skip = HashSet::new();
    backfill_step(&f.pool, &slack, &ctx, 200, &mut skip)
        .await
        .unwrap();
    assert_eq!(
        slack
            .calls_to("conversations.history")
            .iter()
            .map(|call| call.reserve)
            .collect::<Vec<_>>(),
        [POLL_RESERVE]
    );

    slack.clear_calls();
    poll_once(&f.pool, &slack, &ctx, &pass()).await.unwrap();
    assert!(
        slack
            .calls_to("conversations.history")
            .iter()
            .all(|call| call.reserve == 0.0)
    );
});

// ------------------------------------------------------------------------------- boot

db_test!(boot_registers_the_account_and_reference_data, |f| {
    let slack = FakeSlack::new()
        .with_users(vec![
            json!({ "id": "U0TED", "team_id": TEAM, "name": "ted", "real_name": "Ted" }),
        ])
        .with_emoji(json!({ "shipit": "https://emoji/x.png" }));
    let (ctx, team_id) = match boot(&f.pool, &slack, &settings(0)).await {
        Ok(booted) => booted,
        Err(error) => panic!("boot failed: {error}"),
    };
    assert_eq!(team_id, TEAM);
    assert_eq!(ctx.my_user_id, ME);
    assert_eq!(
        ctx.scope_channels.iter().cloned().collect::<Vec<_>>(),
        ["general"]
    );
    assert_eq!(
        scalar::<i64>(&f.pool, "SELECT count(*) FROM workspace_user").await,
        1
    );
    assert_eq!(
        scalar::<i64>(&f.pool, "SELECT count(*) FROM emoji").await,
        1
    );
    let error: Option<String> = scalar(&f.pool, "SELECT last_auth_error FROM account").await;
    assert_eq!(error, None);
});

db_test!(a_dead_credential_at_boot_is_fatal, |f| {
    // Idling on a token that will never work looks like an ingester finding nothing.
    seed_account(&f.pool, "work").await;
    let slack = FakeSlack::new();
    slack.fail("auth.test", "invalid_auth");
    let error = boot(&f.pool, &slack, &settings(0)).await.unwrap_err();
    assert!(matches!(error, BootError::Auth(_)), "{error}");
    let recorded: Option<String> = scalar(&f.pool, "SELECT last_auth_error FROM account").await;
    assert_eq!(recorded.as_deref(), Some("invalid_auth"));
});

db_test!(an_auth_failure_mid_poll_records_and_keeps_running, |f| {
    // Data goes stale while the MCP tools keep serving; a dead container serves nothing.
    seed_account(&f.pool, "work").await;
    let slack = FakeSlack::new();
    slack.fail("users.conversations", "token_revoked");
    poll_loop(&f.pool, &slack, &context(), &settings(0), TEAM, Some(2)).await;
    let recorded: Option<String> = scalar(&f.pool, "SELECT last_auth_error FROM account").await;
    assert_eq!(recorded.as_deref(), Some("token_revoked"));
    assert_eq!(slack.calls_to("users.conversations").len(), 2);
});

db_test!(the_auth_flag_is_cleared_by_a_pass_that_works, |f| {
    // The flag used to be cleared only at boot, so a single transient `invalid_auth` left
    // `status` announcing frozen data for the life of the container while the poll loop
    // went on ingesting. It has to answer for the latest pass, not the worst one.
    seed_account(&f.pool, "work").await;
    store::set_account_auth_error(&f.pool, "work", Some("invalid_auth"))
        .await
        .unwrap();
    let failing_since: Option<chrono::DateTime<Utc>> =
        scalar(&f.pool, "SELECT last_auth_error_at FROM account").await;
    assert!(failing_since.is_some());

    poll_loop(
        &f.pool,
        &FakeSlack::new(),
        &context(),
        &settings(0),
        TEAM,
        Some(1),
    )
    .await;

    let error: Option<String> = scalar(&f.pool, "SELECT last_auth_error FROM account").await;
    assert_eq!(error, None);
    let at: Option<chrono::DateTime<Utc>> =
        scalar(&f.pool, "SELECT last_auth_error_at FROM account").await;
    assert_eq!(at, None);
});

// ------------------------------------------------------------------ multiple accounts

db_test!(two_accounts_keep_independent_copies, |f| {
    // Each account's view is genuinely its own; deduplication is a read-side concern.
    seed_account(&f.pool, "work").await;
    seed_account(&f.pool, "side").await;
    let slack = FakeSlack::new().with_history(
        "C0GENERAL",
        vec![raw(Raw {
            text: "shared channel",
            ..Raw::default()
        })],
    );
    poll_once(&f.pool, &slack, &context(), &pass())
        .await
        .unwrap();
    poll_once(&f.pool, &slack, &context_for("side", &["general"]), &pass())
        .await
        .unwrap();
    let mut accounts: Vec<String> =
        sqlx::query_scalar("SELECT account_id FROM message WHERE conversation_id = 'C0GENERAL'")
            .fetch_all(&f.pool)
            .await
            .unwrap();
    accounts.sort();
    assert_eq!(accounts, ["side", "work"]);
});

// ------------------------------------------------------------------- the first pass

db_test!(the_first_poll_pass_does_not_crawl_all_of_history, |f| {
    // Walking every conversation to its beginning is backfill's job, and doing it in the
    // poll loop delays the new messages that loop exists to surface.
    seed_account(&f.pool, "work").await;
    let slack = FakeSlack::new().with_history(
        "C0GENERAL",
        vec![
            raw(Raw::default()),
            raw(Raw {
                ts: &recent_ts(60),
                ..Raw::default()
            }),
        ],
    );
    let mut pass = pass();
    pass.bootstrap_days = Some(2);
    poll_once(&f.pool, &slack, &context(), &pass).await.unwrap();
    let asked: Vec<String> = slack
        .history_calls("C0GENERAL")
        .into_iter()
        .filter_map(|call| call.oldest)
        .collect();
    assert!(
        !asked.is_empty() && asked.iter().all(|ts| ts.parse::<f64>().unwrap() > 0.0),
        "the first pass asked for all of history: {asked:?}"
    );
    // Only the message inside the window is stored; the 1970 one is backfill's.
    assert_eq!(texts(&f.pool, "C0GENERAL").await.len(), 1);
});

db_test!(without_a_floor_the_poll_pass_still_reads_everything, |f| {
    // With backfill disabled there is no one else to crawl history, so the poll loop must
    // keep doing it.
    seed_account(&f.pool, "work").await;
    let slack = FakeSlack::new().with_history(
        "C0GENERAL",
        vec![
            raw(Raw::default()),
            raw(Raw {
                ts: &recent_ts(60),
                ..Raw::default()
            }),
        ],
    );
    poll_once(&f.pool, &slack, &context(), &pass())
        .await
        .unwrap();
    assert_eq!(texts(&f.pool, "C0GENERAL").await.len(), 2);
});

db_test!(
    a_silent_conversation_advances_its_cursor_off_the_floor,
    |f| {
        // Otherwise every pass re-scans the same empty window forever.
        seed_account(&f.pool, "work").await;
        let mut pass = pass();
        pass.bootstrap_days = Some(2);
        poll_once(&f.pool, &FakeSlack::new(), &context(), &pass)
            .await
            .unwrap();
        let cursor: Option<String> = scalar(
            &f.pool,
            "SELECT poll_cursor FROM conversation WHERE id = 'C0GENERAL'",
        )
        .await;
        assert!(cursor.is_some());
    }
);
