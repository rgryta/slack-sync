//! Domain models shared by the storage layer and the MCP server.
//!
//! These mirror the schema in `db-migrations/pg/0001_init.sql`. Slack's `ts` stays a
//! `String` everywhere for the same reason it is `text` in the schema: it is an opaque
//! identifier, and `posted_at` carries the parsed instant.
//!
//! The models the ingester fills in from Slack payloads live here too, next to the
//! read-side ones they share a schema with.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A `Literal[...]` from the Python models: a closed set stored as `text` and guarded
/// by a CHECK constraint. The macro gives each one the three conversions the schema
/// boundary needs, so a new one costs a single line rather than forty.
macro_rules! text_enum {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            pub fn as_str(&self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl std::str::FromStr for $name {
            type Err = UnknownVariant;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $($text => Ok(Self::$variant),)+
                    other => Err(UnknownVariant {
                        kind: stringify!($name),
                        value: other.to_owned(),
                    }),
                }
            }
        }
    };
}

/// A value outside the CHECK constraint the column carries. Reaching this means the
/// database disagrees with the schema, so it is an error rather than a default.
#[derive(Debug, thiserror::Error)]
#[error("{value:?} is not a valid {kind}")]
pub struct UnknownVariant {
    pub kind: &'static str,
    pub value: String,
}

text_enum!(ConversationType {
    Im => "im",
    Mpim => "mpim",
    Channel => "channel",
    Group => "group",
});

text_enum!(SyncLoop {
    Poll => "poll",
    Backfill => "backfill",
});

text_enum!(ScopeReason {
    Dm => "dm",
    GroupDm => "group_dm",
    NamedChannel => "named_channel",
    MentionThread => "mention_thread",
});

/// One Slack workspace, ours or a Slack Connect partner's.
///
/// The Python model carries a `first_seen` field that nothing reads and no statement
/// writes — the column has a database default — so it is not repeated here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub team_id: String,
    pub name: Option<String>,
    pub domain: Option<String>,
    pub is_external: bool,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Account {
    pub id: String,
    pub team_id: String,
    pub my_user_id: String,
    pub enabled: bool,
    pub last_auth_error: Option<String>,
    pub last_auth_error_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
}

/// One conversation as the ingester sees it: the Slack listing's view in the first
/// fields, and the archive's own bookkeeping in the rest.
///
/// `parse_conversation` fills only what a payload can say; the scope and cursor fields
/// are the ingester's and come back from the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversation {
    pub account_id: String,
    pub id: String,
    pub team_id: Option<String>,
    pub shared_team_ids: Vec<String>,
    pub kind: ConversationType,
    pub name: Option<String>,
    pub is_member: bool,
    /// For an `im`, the person on the other end. Slack's listing carries it and
    /// nothing else does: a DM has no name, so without this a DM can only ever render
    /// as its channel id.
    pub counterpart_user_id: Option<String>,
    pub in_scope: bool,
    pub scope_reason: Option<ScopeReason>,
    pub poll_cursor: Option<String>,
    pub backfill_cursor: Option<String>,
    pub backfill_done: bool,
    pub reactions_fresh_since: Option<DateTime<Utc>>,
    pub last_polled_at: Option<DateTime<Utc>>,
    /// Transient, not a column: Slack's listing says a channel is externally shared
    /// but not who with, and discovery uses this to decide whether one extra
    /// `conversations.info` call is worth making. Nothing persists it.
    pub is_ext_shared: bool,
}

/// One custom emoji, or one alias of one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Emoji {
    pub team_id: String,
    pub name: String,
    pub url: Option<String>,
    pub alias_for: Option<String>,
}

/// One member of a workspace, as one account sees them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceUser {
    pub account_id: String,
    pub user_id: String,
    pub team_id: Option<String>,
    pub name: Option<String>,
    pub real_name: Option<String>,
    /// What the Slack client puts on a message. `real_name` is the legal name on the
    /// profile and is frequently not what anyone in the workspace calls the person.
    pub display_name: Option<String>,
    pub is_bot: bool,
}

/// One emoji name on one message. `user_ids` is the observed reactor set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reaction {
    pub name: String,
    pub user_ids: Vec<String>,
    pub count: i32,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Message {
    pub account_id: String,
    pub conversation_id: String,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub user_id: Option<String>,
    pub text: String,
    pub subtype: Option<String>,
    pub edited_at: Option<DateTime<Utc>>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub raw: serde_json::Value,
    pub posted_at: DateTime<Utc>,
    /// Loaded by a second query rather than a join, so one page of history costs two
    /// round trips instead of one per message.
    #[sqlx(skip)]
    pub reactions: Vec<Reaction>,
}

impl Message {
    /// Slack's own reply count, which only a thread parent carries.
    pub fn reply_count(&self) -> Option<i64> {
        self.raw.get("reply_count")?.as_i64()
    }
}

/// One `(account, user)` pair. `resolve_author` returns a set of these.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, sqlx::FromRow)]
pub struct Identity {
    pub account_id: String,
    pub user_id: String,
}

#[derive(Debug, Clone)]
pub struct Alias {
    pub alias: String,
    pub note: Option<String>,
    pub members: Vec<Identity>,
}

/// A conversation plus the read-side aggregates `slack-conversations` shows.
#[derive(Debug, Clone)]
pub struct ConversationSummary {
    pub account_id: String,
    pub id: String,
    pub team_id: Option<String>,
    pub kind: ConversationType,
    pub name: Option<String>,
    pub scope_reason: Option<ScopeReason>,
    pub message_count: i64,
    pub last_activity: Option<DateTime<Utc>>,
    pub last_polled_at: Option<DateTime<Utc>>,
    pub reactions_fresh_since: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub message: Message,
    pub rank: f32,
    pub snippet: String,
}

#[derive(Debug, Clone)]
pub struct WorkspaceSummary {
    pub team_id: String,
    pub name: Option<String>,
    pub domain: Option<String>,
    pub is_external: bool,
    pub conversation_count: i64,
}

/// Where one of an account's two loops has got to, and how it last failed.
///
/// `r#loop` keeps the column's own name. Progress and failure are written by two
/// different functions on purpose — see `store::set_sync_progress`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncState {
    pub account_id: String,
    pub r#loop: SyncLoop,
    pub current_conversation_id: Option<String>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub last_error_at: Option<DateTime<Utc>>,
}

/// One row of `slack-status`: how fresh one account's slice of the corpus is.
#[derive(Debug, Clone)]
pub struct SyncStatus {
    pub account_id: String,
    pub team_id: String,
    pub enabled: bool,
    pub last_auth_error: Option<String>,
    pub last_auth_error_at: Option<DateTime<Utc>>,
    pub in_scope_count: i64,
    pub backfilled_count: i64,
    pub last_polled_at: Option<DateTime<Utc>>,
    pub reactions_fresh_since: Option<DateTime<Utc>>,
}

/// One row of a message's text over time.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Revision {
    pub seq: i32,
    pub text: Option<String>,
    /// NULL on seq 1 and only there, meaning unknown: nothing records when a message
    /// was first stored, so the earliest text held has no honest start time.
    pub first_seen_at: Option<DateTime<Utc>>,
    pub superseded_at: Option<DateTime<Utc>>,
}

/// What `resolve_message` found: enough to read the revision log and to say whether
/// Slack and the archive disagree about an edit.
#[derive(Debug, Clone)]
pub struct ResolvedMessage {
    pub account_id: String,
    pub conversation_id: String,
    pub deleted_at: Option<DateTime<Utc>>,
    pub edited_at: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_enums_round_trip_through_their_column_value() {
        assert_eq!(
            "group_dm".parse::<ScopeReason>().unwrap(),
            ScopeReason::GroupDm
        );
        assert_eq!(ScopeReason::GroupDm.as_str(), "group_dm");
        assert_eq!(
            "im".parse::<ConversationType>().unwrap(),
            ConversationType::Im
        );
    }

    #[test]
    fn a_value_outside_the_check_constraint_is_an_error_not_a_default() {
        assert!("dms".parse::<ScopeReason>().is_err());
    }

    #[test]
    fn reply_count_is_absent_on_an_ordinary_message() {
        let mut raw = serde_json::json!({});
        assert_eq!(reply_count_of(&raw), None);
        raw = serde_json::json!({ "reply_count": 12 });
        assert_eq!(reply_count_of(&raw), Some(12));
    }

    fn reply_count_of(raw: &serde_json::Value) -> Option<i64> {
        Message {
            account_id: "work".into(),
            conversation_id: "C1".into(),
            ts: "1.0".into(),
            thread_ts: None,
            user_id: None,
            text: String::new(),
            subtype: None,
            edited_at: None,
            deleted_at: None,
            raw: raw.clone(),
            posted_at: Utc::now(),
            reactions: Vec::new(),
        }
        .reply_count()
    }
}
