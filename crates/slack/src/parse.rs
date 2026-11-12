//! Slack payloads to the models the storage layer stores.
//!
//! This is where the two content-reduction rules live, so nothing downstream can
//! accidentally persist what we decided not to keep: `files`, `blocks` and
//! `attachments` are removed from `raw` here, at the only place raw payloads become
//! models.
//!
//! Nothing in this module touches the database or the network. A field Slack omits
//! becomes `None`, and a field Slack sends empty becomes `None` wherever the Python
//! used `or` to the same effect — an empty string would otherwise win a `COALESCE`
//! against a perfectly good stored value.

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use slack_core::models::{
    Conversation, ConversationType, Emoji, Message, Reaction, Workspace, WorkspaceUser,
};

use crate::errors::SlackError;

/// Attachments are out of scope, and `blocks` is a rich-text mirror of `text` that
/// multiplies row size for no query we intend to run.
const DROPPED_MESSAGE_KEYS: [&str; 3] = ["files", "blocks", "attachments"];

/// A string field, whether or not Slack sent it empty.
fn text(raw: &Value, key: &str) -> Option<String> {
    raw.get(key)?.as_str().map(str::to_owned)
}

/// A string field, with an empty one treated as absent — Python's `x.get(k) or ...`.
fn nonempty(raw: &Value, key: &str) -> Option<String> {
    text(raw, key).filter(|value| !value.is_empty())
}

fn flag(raw: &Value, key: &str) -> bool {
    raw.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn string_list(raw: &Value, key: &str) -> impl Iterator<Item = String> {
    raw.get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|item| item.as_str().map(str::to_owned))
}

/// Slack's `ts` as an instant. The string itself stays the identifier.
///
/// The `ts` of a real message is always a parseable `seconds.micros`; a payload where
/// it is not is a payload we cannot store, so this reports rather than guesses.
pub fn parse_ts(ts: &str) -> Result<DateTime<Utc>, SlackError> {
    let seconds: f64 = ts
        .parse()
        .map_err(|_| SlackError::Transport(format!("unparseable ts {ts:?}")))?;
    // Microseconds, like `datetime.fromtimestamp` — Slack's ts carries six decimals
    // and a float cannot honestly hold more.
    let micros = (seconds * 1e6).round() as i64;
    DateTime::from_timestamp_micros(micros).ok_or_else(|| {
        SlackError::Transport(format!("ts {ts:?} is outside the representable range"))
    })
}

/// Reactions on a message. Absent means an empty list, never `None`.
///
/// The ingester makes the stored set equal this one, so "no reactions key" has to mean
/// "no reactions" for a removal to ever be observed.
pub fn parse_reactions(raw: &Value) -> Vec<Reaction> {
    raw.get("reactions")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|item| {
            Some(Reaction {
                name: nonempty(item, "name")?,
                user_ids: string_list(item, "users").collect(),
                count: item
                    .get("count")
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
                    .try_into()
                    .unwrap_or(i32::MAX),
            })
        })
        .collect()
}

/// One message.
///
/// `subtype` is preserved rather than dropped: a join or leave message is a real
/// message, and deciding it is uninteresting is the scope filter's job, not this
/// layer's.
pub fn parse_message(
    raw: &Value,
    account_id: &str,
    conversation_id: &str,
) -> Result<Message, SlackError> {
    let ts = nonempty(raw, "ts")
        .ok_or_else(|| SlackError::Transport("a message without a ts".to_owned()))?;
    let mut stored: Map<String, Value> = raw
        .as_object()
        .ok_or_else(|| SlackError::Transport("a message that is not an object".to_owned()))?
        .clone();
    for key in DROPPED_MESSAGE_KEYS {
        stored.remove(key);
    }
    let edited_ts = raw.get("edited").and_then(|edited| nonempty(edited, "ts"));
    let thread_ts = nonempty(raw, "thread_ts");
    Ok(Message {
        account_id: account_id.to_owned(),
        conversation_id: conversation_id.to_owned(),
        // Slack sets thread_ts == ts on a thread parent; that is not a reply, and
        // storing it would make every parent look like its own child.
        thread_ts: thread_ts.filter(|parent| *parent != ts),
        user_id: nonempty(raw, "user").or_else(|| nonempty(raw, "bot_id")),
        text: nonempty(raw, "text").unwrap_or_default(),
        subtype: text(raw, "subtype"),
        edited_at: edited_ts.as_deref().map(parse_ts).transpose()?,
        posted_at: parse_ts(&ts)?,
        deleted_at: None,
        reactions: parse_reactions(raw),
        raw: Value::Object(stored),
        ts,
    })
}

/// Slack's boolean soup reduced to the four values the schema allows.
///
/// Order matters: an `im` is also flagged private, and an `mpim` is also flagged a
/// group, so the most specific flag has to win.
pub fn conversation_type(raw: &Value) -> ConversationType {
    if flag(raw, "is_im") {
        ConversationType::Im
    } else if flag(raw, "is_mpim") {
        ConversationType::Mpim
    } else if flag(raw, "is_private") {
        ConversationType::Group
    } else {
        ConversationType::Channel
    }
}

/// One conversation, with the workspace axis resolved.
///
/// A Slack Connect channel is owned by one team and shared with others; `context_team`
/// is the owner as this account sees it, and `shared_team_ids` is everyone on it. A
/// channel that names no team at all falls back to the account's own team, because an
/// unattributed conversation would otherwise vanish from every workspace filter.
pub fn parse_conversation(
    raw: &Value,
    account_id: &str,
    default_team_id: Option<&str>,
) -> Result<Conversation, SlackError> {
    let mut shared: Vec<String> = Vec::new();
    for key in ["shared_team_ids", "internal_team_ids", "connected_team_ids"] {
        for team in string_list(raw, key) {
            if !shared.contains(&team) {
                shared.push(team);
            }
        }
    }
    let team_id = nonempty(raw, "context_team_id")
        .or_else(|| nonempty(raw, "context_team"))
        .or_else(|| nonempty(raw, "shared_team_id"))
        .or_else(|| shared.first().cloned())
        .or_else(|| default_team_id.map(str::to_owned));
    Ok(Conversation {
        account_id: account_id.to_owned(),
        id: nonempty(raw, "id")
            .ok_or_else(|| SlackError::Transport("a conversation without an id".to_owned()))?,
        team_id,
        shared_team_ids: shared,
        kind: conversation_type(raw),
        name: nonempty(raw, "name").or_else(|| nonempty(raw, "name_normalized")),
        is_member: flag(raw, "is_member") || flag(raw, "is_im") || flag(raw, "is_mpim"),
        counterpart_user_id: text(raw, "user"),
        is_ext_shared: flag(raw, "is_ext_shared") || flag(raw, "is_pending_ext_shared"),
        in_scope: false,
        scope_reason: None,
        poll_cursor: None,
        backfill_cursor: None,
        backfill_done: false,
        reactions_fresh_since: None,
        last_polled_at: None,
    })
}

pub fn parse_user(raw: &Value, account_id: &str) -> Result<WorkspaceUser, SlackError> {
    let profile = raw.get("profile").cloned().unwrap_or(Value::Null);
    Ok(WorkspaceUser {
        account_id: account_id.to_owned(),
        user_id: nonempty(raw, "id")
            .ok_or_else(|| SlackError::Transport("a user without an id".to_owned()))?,
        team_id: text(raw, "team_id"),
        name: text(raw, "name"),
        real_name: nonempty(raw, "real_name").or_else(|| nonempty(&profile, "real_name")),
        // Slack sends "" rather than omitting it when unset, and an empty string would
        // win a COALESCE against a perfectly good stored value.
        display_name: nonempty(&profile, "display_name")
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty()),
        is_bot: flag(raw, "is_bot"),
    })
}

pub fn parse_workspace(raw: &Value, is_external: bool) -> Result<Workspace, SlackError> {
    Ok(Workspace {
        team_id: nonempty(raw, "id")
            .ok_or_else(|| SlackError::Transport("a team without an id".to_owned()))?,
        name: text(raw, "name"),
        domain: text(raw, "domain"),
        is_external,
    })
}

/// `emoji.list`'s `{name: url}`, with `alias:<name>` entries kept as aliases.
///
/// An alias has no URL of its own; flattening it to the target's URL would lose the
/// fact that two names were in use, which is exactly what a reaction refers to.
pub fn parse_emoji(entries: &Value, team_id: &str) -> Vec<Emoji> {
    let Some(entries) = entries.as_object() else {
        return Vec::new();
    };
    entries
        .iter()
        .map(|(name, value)| {
            let value = value.as_str().unwrap_or_default();
            match value.strip_prefix("alias:") {
                Some(target) => Emoji {
                    team_id: team_id.to_owned(),
                    name: name.clone(),
                    url: None,
                    alias_for: Some(target.to_owned()),
                },
                None => Emoji {
                    team_id: team_id.to_owned(),
                    name: name.clone(),
                    url: Some(value.to_owned()),
                    alias_for: None,
                },
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::load;

    fn history_messages() -> Vec<Value> {
        load("conversations_history")["messages"]
            .as_array()
            .expect("messages")
            .clone()
    }

    fn parse_nth(index: usize) -> Message {
        parse_message(&history_messages()[index], "work", "C0GENERAL").unwrap()
    }

    #[test]
    fn ts_is_kept_verbatim_and_parsed_alongside() {
        let message = parse_nth(0);
        assert_eq!(message.ts, "1723640400.001200");
        assert_eq!(
            message.posted_at,
            "2024-08-14T13:00:00.001200Z"
                .parse::<DateTime<Utc>>()
                .unwrap()
        );
    }

    #[test]
    fn files_and_blocks_never_reach_stored_raw() {
        // The only place stripping happens, so nothing downstream can persist them.
        let with_blocks = parse_nth(0);
        let with_files = parse_nth(3);
        assert!(with_blocks.raw.get("blocks").is_none());
        assert!(with_files.raw.get("files").is_none());
        // The rest of the payload survives — this is a filter, not a rewrite.
        assert_eq!(with_files.raw["ts"], "1723640700.000300");
        assert_eq!(with_files.text, "here is the log");
    }

    #[test]
    fn missing_reactions_parse_to_an_empty_list() {
        // `None` here would make "no reactions" indistinguishable from "not observed",
        // and the ingester clears stale reactions on exactly that distinction.
        assert_eq!(parse_nth(1).reactions, Vec::new());
    }

    #[test]
    fn reactions_carry_users_and_counts() {
        let reactions = parse_nth(0).reactions;
        assert_eq!(
            reactions,
            [
                Reaction {
                    name: "eyes".into(),
                    count: 1,
                    user_ids: vec!["U0ME".into()],
                },
                Reaction {
                    name: "tada".into(),
                    count: 2,
                    user_ids: vec!["U0ME".into(), "U0ANN".into()],
                },
            ]
        );
    }

    #[test]
    fn thread_parent_is_not_its_own_reply() {
        let replies = load("conversations_replies");
        let messages = replies["messages"].as_array().unwrap();
        let parent = parse_message(&messages[0], "work", "C0GENERAL").unwrap();
        let reply = parse_message(&messages[1], "work", "C0GENERAL").unwrap();
        assert_eq!(parent.thread_ts, None);
        assert_eq!(reply.thread_ts.as_deref(), Some("1723640400.001200"));
    }

    #[test]
    fn edit_and_subtype_are_preserved() {
        let edited = parse_nth(1);
        assert_eq!(
            edited.edited_at,
            Some("2024-08-14T13:01:40Z".parse::<DateTime<Utc>>().unwrap())
        );
        // A join message is a real message; dropping it is the scope filter's decision.
        assert_eq!(parse_nth(2).subtype.as_deref(), Some("channel_join"));
    }

    #[test]
    fn a_message_without_a_ts_is_reported_rather_than_stored_at_the_epoch() {
        let error = parse_message(&serde_json::json!({ "text": "hi" }), "work", "C1").unwrap_err();
        assert!(error.to_string().contains("without a ts"), "{error}");
    }

    #[test]
    fn conversation_type_takes_the_most_specific_flag() {
        for (raw, expected) in [
            (
                serde_json::json!({ "id": "D1", "is_im": true, "is_private": true }),
                ConversationType::Im,
            ),
            (
                serde_json::json!({ "id": "G1", "is_mpim": true, "is_group": true, "is_private": true }),
                ConversationType::Mpim,
            ),
            (
                serde_json::json!({ "id": "G2", "is_group": true, "is_private": true }),
                ConversationType::Group,
            ),
            (
                serde_json::json!({ "id": "C1", "is_channel": true }),
                ConversationType::Channel,
            ),
        ] {
            assert_eq!(conversation_type(&raw), expected, "{raw}");
        }
    }

    #[test]
    fn shared_channel_keeps_every_team() {
        let page = load("users_conversations_page2");
        let shared = parse_conversation(&page["channels"][1], "work", None).unwrap();
        assert_eq!(shared.kind, ConversationType::Channel);
        assert_eq!(shared.team_id.as_deref(), Some("T0001"));
        let mut teams = shared.shared_team_ids.clone();
        teams.sort();
        assert_eq!(teams, ["T0001", "T0EXT"]);
    }

    #[test]
    fn conversation_without_a_team_falls_back_to_the_account_team() {
        // An unattributed conversation would otherwise vanish from every workspace
        // filter.
        let conversation = parse_conversation(
            &serde_json::json!({ "id": "D9", "is_im": true }),
            "work",
            Some("T0001"),
        )
        .unwrap();
        assert_eq!(conversation.team_id.as_deref(), Some("T0001"));
        // A DM is always joined; Slack does not send `is_member` for one.
        assert!(conversation.is_member);
    }

    #[test]
    fn emoji_aliases_stay_aliases() {
        let emojis = parse_emoji(&load("emoji_list")["emoji"], "T0001");
        let find = |name: &str| {
            emojis
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("{name}"))
                .clone()
        };
        assert!(find("shipit").url.is_some());
        assert_eq!(find("yolo").alias_for.as_deref(), Some("shipit"));
        assert_eq!(find("yolo").url, None);
    }

    #[test]
    fn user_parsing_reads_the_profile_fallback() {
        let members = load("users_list");
        let bot = parse_user(&members["members"][1], "work").unwrap();
        assert!(bot.is_bot);
        assert_eq!(bot.real_name.as_deref(), Some("Deploy Bot"));
    }

    #[test]
    fn display_name_is_kept_and_blanks_become_null() {
        // Slack sends `display_name: ""` when unset, which must not beat a real name.
        let members = load("users_list");
        assert_eq!(
            parse_user(&members["members"][0], "work")
                .unwrap()
                .display_name
                .as_deref(),
            Some("Coach Ted")
        );
        assert_eq!(
            parse_user(&members["members"][1], "work")
                .unwrap()
                .display_name,
            None
        );
    }
}
