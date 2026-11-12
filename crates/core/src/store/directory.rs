//! The lookup tables that turn IDs into names, and the precedence rules that pick one.
//!
//! Loaded per request rather than cached: it is small, and a stale directory would
//! render a renamed user under their old name with nothing to signal it.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use sqlx::Row;
use sqlx::postgres::PgPool;

use crate::models::{ConversationType, Emoji, WorkspaceUser};
use crate::store::parse_column;

#[derive(Debug, Clone, Default)]
pub struct DirectoryUser {
    pub name: Option<String>,
    pub real_name: Option<String>,
    /// What the Slack client puts on a message. `real_name` is the legal name on the
    /// profile and is frequently not what anyone in the workspace calls the person.
    pub display_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DirectoryConversation {
    pub name: Option<String>,
    pub team_id: Option<String>,
    pub kind: ConversationType,
    /// For an `im`, the person on the other end. Slack's listing carries it and
    /// nothing else does: a DM has no name, so without this a DM can only ever render
    /// as its channel id.
    pub counterpart_user_id: Option<String>,
    pub last_polled_at: Option<DateTime<Utc>>,
    pub reactions_fresh_since: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default)]
pub struct DirectoryWorkspace {
    pub name: Option<String>,
    pub domain: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Directory {
    pub users: HashMap<(String, String), DirectoryUser>,
    pub aliases: HashMap<(String, String), String>,
    pub conversations: HashMap<(String, String), DirectoryConversation>,
    pub workspaces: HashMap<String, DirectoryWorkspace>,
    pub emoji: HashSet<(String, String)>,
}

/// Empty strings are absences, not names: an unset Slack profile field comes back as
/// `""` as often as NULL, and "called nothing" must fall through to the next source.
/// Applied at lookup rather than at load, so a `Directory` built by hand — in a test,
/// or by a later caller — cannot carry a blank name past the precedence rules.
fn named(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|text| !text.is_empty())
}

impl Directory {
    /// Everything the renderer needs to turn IDs into names, in five queries.
    pub async fn load(pool: &PgPool) -> Result<Self, sqlx::Error> {
        let mut directory = Self::default();

        let rows = sqlx::query(
            "SELECT account_id, user_id, name, real_name, display_name FROM workspace_user",
        )
        .fetch_all(pool)
        .await?;
        for row in rows {
            directory.users.insert(
                (row.try_get("account_id")?, row.try_get("user_id")?),
                DirectoryUser {
                    name: row.try_get("name")?,
                    real_name: row.try_get("real_name")?,
                    display_name: row.try_get("display_name")?,
                },
            );
        }

        let rows = sqlx::query("SELECT account_id, user_id, alias FROM user_alias_member")
            .fetch_all(pool)
            .await?;
        for row in rows {
            directory.aliases.insert(
                (row.try_get("account_id")?, row.try_get("user_id")?),
                row.try_get("alias")?,
            );
        }

        let rows = sqlx::query(
            "SELECT account_id, id, name, team_id, type, counterpart_user_id, \
             last_polled_at, reactions_fresh_since FROM conversation",
        )
        .fetch_all(pool)
        .await?;
        for row in rows {
            let kind: String = row.try_get("type")?;
            directory.conversations.insert(
                (row.try_get("account_id")?, row.try_get("id")?),
                DirectoryConversation {
                    name: row.try_get("name")?,
                    team_id: row.try_get("team_id")?,
                    kind: parse_column::<ConversationType>(&kind)?,
                    counterpart_user_id: row.try_get("counterpart_user_id")?,
                    last_polled_at: row.try_get("last_polled_at")?,
                    reactions_fresh_since: row.try_get("reactions_fresh_since")?,
                },
            );
        }

        let rows = sqlx::query("SELECT team_id, name, domain FROM workspace")
            .fetch_all(pool)
            .await?;
        for row in rows {
            directory.workspaces.insert(
                row.try_get("team_id")?,
                DirectoryWorkspace {
                    name: row.try_get("name")?,
                    domain: row.try_get("domain")?,
                },
            );
        }

        let rows = sqlx::query("SELECT team_id, name FROM emoji")
            .fetch_all(pool)
            .await?;
        for row in rows {
            directory
                .emoji
                .insert((row.try_get("team_id")?, row.try_get("name")?));
        }

        Ok(directory)
    }

    /// Alias, else display name, else real name, else handle, else the raw ID.
    ///
    /// The precedence is fixed: an alias is something the user set deliberately, so it
    /// outranks anything Slack supplies. `display_name` outranks `real_name` for the
    /// same reason it does in the Slack client — it is the name the person chose and
    /// the only one their colleagues ever see. `real_name` is the legal name on the
    /// profile, which can be a name nobody in the workspace would recognise.
    pub fn user_name(&self, account_id: &str, user_id: &str) -> String {
        if let Some(alias) = self.aliases.get(&key(account_id, user_id)) {
            return alias.clone();
        }
        self.slack_name(account_id, user_id)
    }

    /// The name Slack knows, ignoring aliases.
    ///
    /// The alias listing needs this: showing the alias as the member's name would just
    /// repeat the alias back and say nothing about who is behind it.
    pub fn slack_name(&self, account_id: &str, user_id: &str) -> String {
        match self.users.get(&key(account_id, user_id)) {
            Some(user) => named(&user.display_name)
                .or_else(|| named(&user.real_name))
                .or_else(|| named(&user.name))
                .unwrap_or(user_id)
                .to_owned(),
            None => user_id.to_owned(),
        }
    }

    pub fn conversation_label(&self, account_id: &str, conversation_id: &str) -> String {
        let Some(conversation) = self.conversations.get(&key(account_id, conversation_id)) else {
            return conversation_id.to_owned();
        };
        if let Some(name) = named(&conversation.name) {
            return match conversation.kind {
                ConversationType::Channel | ConversationType::Group => format!("#{name}"),
                _ => name.to_owned(),
            };
        }
        // A DM has no name. Without the counterpart it renders as `im:D09QVUDMLGM`,
        // which says nothing about who is on the other end - and there is no other
        // tool that will tell you.
        if conversation.kind == ConversationType::Im {
            if let Some(counterpart) = &conversation.counterpart_user_id {
                return format!("DM with {}", self.user_name(account_id, counterpart));
            }
        }
        format!("{}:{conversation_id}", conversation.kind)
    }

    /// Name a conversation even when no account was named.
    ///
    /// Without an account there is no key into the directory, and falling back to the
    /// raw ID for the common "just read #general" call would make the header useless.
    pub fn any_conversation_label(&self, conversation_id: &str, account: Option<&str>) -> String {
        if let Some(account) = account {
            return self.conversation_label(account, conversation_id);
        }
        // Sorted so the label does not change between two identical requests just
        // because a hash map iterated differently.
        let mut owners: Vec<&String> = self
            .conversations
            .keys()
            .filter(|(_, id)| id == conversation_id)
            .map(|(account_id, _)| account_id)
            .collect();
        owners.sort();
        match owners.first() {
            Some(account_id) => self.conversation_label(account_id, conversation_id),
            None => conversation_id.to_owned(),
        }
    }

    pub fn workspace_label(&self, team_id: Option<&str>) -> String {
        let Some(team_id) = team_id else {
            return "unknown-workspace".to_owned();
        };
        match self.workspaces.get(team_id) {
            Some(workspace) => named(&workspace.domain)
                .or_else(|| named(&workspace.name))
                .unwrap_or(team_id)
                .to_owned(),
            None => team_id.to_owned(),
        }
    }

    pub fn team_of(&self, account_id: &str, conversation_id: &str) -> Option<&str> {
        self.conversations
            .get(&key(account_id, conversation_id))?
            .team_id
            .as_deref()
    }

    pub fn reactions_fresh_since(
        &self,
        account_id: &str,
        conversation_id: &str,
    ) -> Option<DateTime<Utc>> {
        self.conversations
            .get(&key(account_id, conversation_id))?
            .reactions_fresh_since
    }

    pub fn last_polled_at(&self, account_id: &str, conversation_id: &str) -> Option<DateTime<Utc>> {
        self.conversations
            .get(&key(account_id, conversation_id))?
            .last_polled_at
    }

    pub fn known_emoji(&self, team_id: Option<&str>, name: &str) -> bool {
        match team_id {
            Some(team_id) => self.emoji.contains(&key(team_id, name)),
            None => false,
        }
    }
}

fn key(a: &str, b: &str) -> (String, String) {
    (a.to_owned(), b.to_owned())
}

/// Insert or refresh the people one account can see.
pub async fn upsert_users(pool: &PgPool, users: &[WorkspaceUser]) -> Result<(), sqlx::Error> {
    for user in users {
        sqlx::query(
            "
            INSERT INTO workspace_user (
              account_id, user_id, team_id, name, real_name, display_name, is_bot, updated_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, now())
            ON CONFLICT (account_id, user_id) DO UPDATE SET
              team_id    = EXCLUDED.team_id,
              name       = EXCLUDED.name,
              real_name  = EXCLUDED.real_name,
              -- A payload without the field must not blank a name that is already known;
              -- only a present value replaces one.
              display_name = COALESCE(EXCLUDED.display_name, workspace_user.display_name),
              is_bot     = EXCLUDED.is_bot,
              updated_at = now()
            ",
        )
        .bind(&user.account_id)
        .bind(&user.user_id)
        .bind(user.team_id.as_deref())
        .bind(user.name.as_deref())
        .bind(user.real_name.as_deref())
        .bind(user.display_name.as_deref())
        .bind(user.is_bot)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Insert or refresh a workspace's custom emoji.
pub async fn upsert_emoji(pool: &PgPool, emojis: &[Emoji]) -> Result<(), sqlx::Error> {
    for emoji in emojis {
        sqlx::query(
            "
            INSERT INTO emoji (team_id, name, url, alias_for, updated_at)
            VALUES ($1, $2, $3, $4, now())
            ON CONFLICT (team_id, name) DO UPDATE SET
              url        = EXCLUDED.url,
              alias_for  = EXCLUDED.alias_for,
              updated_at = now()
            ",
        )
        .bind(&emoji.team_id)
        .bind(&emoji.name)
        .bind(emoji.url.as_deref())
        .bind(emoji.alias_for.as_deref())
        .execute(pool)
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory() -> Directory {
        let mut directory = Directory::default();
        directory.users.insert(
            key("work", "U0TED"),
            DirectoryUser {
                name: Some("ted".into()),
                real_name: Some("Ted Lasso".into()),
                display_name: Some("Coach Ted".into()),
            },
        );
        directory.users.insert(
            key("work", "U0SAM"),
            DirectoryUser {
                name: Some("sam".into()),
                real_name: Some("Sam Obisanya".into()),
                display_name: None,
            },
        );
        directory.users.insert(
            key("work", "U0ANN"),
            DirectoryUser {
                name: Some("ann".into()),
                ..DirectoryUser::default()
            },
        );
        directory
            .users
            .insert(key("work", "U0NIL"), DirectoryUser::default());
        directory
            .aliases
            .insert(key("work", "U0TED"), "coach".into());
        directory.conversations.insert(
            key("work", "C0GENERAL"),
            DirectoryConversation {
                name: Some("general".into()),
                team_id: Some("T0001".into()),
                kind: ConversationType::Channel,
                counterpart_user_id: None,
                last_polled_at: None,
                reactions_fresh_since: None,
            },
        );
        directory.conversations.insert(
            key("work", "D0TED"),
            DirectoryConversation {
                name: None,
                team_id: Some("T0001".into()),
                kind: ConversationType::Im,
                counterpart_user_id: Some("U0TED".into()),
                last_polled_at: None,
                reactions_fresh_since: None,
            },
        );
        directory.workspaces.insert(
            "T0001".into(),
            DirectoryWorkspace {
                name: Some("Acme".into()),
                domain: Some("acme".into()),
            },
        );
        directory.emoji.insert(key("T0001", "shipit"));
        directory
    }

    #[test]
    fn an_alias_beats_everything_slack_supplies() {
        assert_eq!(directory().user_name("work", "U0TED"), "coach");
    }

    #[test]
    fn display_name_beats_real_name() {
        // Preferring `real_name` filed a colleague everyone calls "Sandy Ganoti" under
        // "Kyriaki Aikaterini Ganoti" — a name that appears nowhere the user can see.
        let mut directory = directory();
        directory.aliases.clear();
        assert_eq!(directory.user_name("work", "U0TED"), "Coach Ted");
    }

    #[test]
    fn real_name_is_used_when_no_display_name_is_set() {
        assert_eq!(directory().user_name("work", "U0SAM"), "Sam Obisanya");
    }

    #[test]
    fn the_handle_is_used_when_there_is_no_name_at_all() {
        assert_eq!(directory().user_name("work", "U0ANN"), "ann");
    }

    #[test]
    fn slack_name_follows_the_same_order_but_ignores_aliases() {
        let directory = directory();
        assert_eq!(directory.slack_name("work", "U0TED"), "Coach Ted");
        assert_eq!(directory.slack_name("work", "U0SAM"), "Sam Obisanya");
        assert_eq!(directory.slack_name("work", "U0ANN"), "ann");
        assert_eq!(directory.slack_name("work", "U0NIL"), "U0NIL");
    }

    #[test]
    fn the_raw_id_is_the_last_resort() {
        let directory = directory();
        assert_eq!(directory.user_name("work", "U0NIL"), "U0NIL");
        assert_eq!(directory.user_name("work", "U0GHOST"), "U0GHOST");
    }

    #[test]
    fn an_empty_profile_field_falls_through_to_the_next_source() {
        let mut directory = directory();
        directory.users.insert(
            key("work", "U0EMPTY"),
            DirectoryUser {
                name: Some("handle".into()),
                real_name: Some(String::new()),
                display_name: Some(String::new()),
            },
        );
        // `named()` is what makes this hold; an empty string is as much an absence as
        // NULL, and Slack returns both for an unset profile field.
        assert_eq!(directory.slack_name("work", "U0EMPTY"), "handle");
    }

    #[test]
    fn a_channel_is_hashed_and_a_dm_names_who_is_on_the_other_end() {
        let directory = directory();
        assert_eq!(
            directory.conversation_label("work", "C0GENERAL"),
            "#general"
        );
        assert_eq!(
            directory.conversation_label("work", "D0TED"),
            "DM with coach"
        );
    }

    #[test]
    fn a_dm_with_no_known_counterpart_still_renders() {
        let mut directory = directory();
        directory
            .conversations
            .get_mut(&key("work", "D0TED"))
            .unwrap()
            .counterpart_user_id = None;
        assert_eq!(directory.conversation_label("work", "D0TED"), "im:D0TED");
    }

    #[test]
    fn a_conversation_is_named_even_with_no_account_given() {
        assert_eq!(
            directory().any_conversation_label("C0GENERAL", None),
            "#general"
        );
        assert_eq!(directory().any_conversation_label("C0NOPE", None), "C0NOPE");
    }

    #[test]
    fn the_workspace_label_prefers_the_domain() {
        let directory = directory();
        assert_eq!(directory.workspace_label(Some("T0001")), "acme");
        assert_eq!(directory.workspace_label(Some("T0999")), "T0999");
        assert_eq!(directory.workspace_label(None), "unknown-workspace");
    }

    #[test]
    fn emoji_are_only_known_within_their_own_workspace() {
        let directory = directory();
        assert!(directory.known_emoji(Some("T0001"), "shipit"));
        assert!(!directory.known_emoji(Some("T0002"), "shipit"));
        assert!(!directory.known_emoji(None, "shipit"));
    }
}
