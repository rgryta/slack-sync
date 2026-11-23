//! The single decision point for what gets stored.
//!
//! Pure, no I/O, and deliberately small: everything the user asked to keep — DMs, group
//! DMs, the named channels, and threads they were pinged on — is decided here, and a
//! message that reaches no rule is discarded and never written.
//!
//! Everything it drops is discarded and never written, so a false DROP is data the user
//! asked for and will not get, and a false STORE is data they explicitly said they did
//! not want kept. That is why this is the one unit with no database in reach.

use std::collections::HashSet;

use slack_core::models::{Conversation, ConversationType, Message, ScopeReason};

/// Membership events rendered as messages. They fill history with noise no query wants
/// and never carry a mention, so they are dropped before any other rule runs — placing
/// this first keeps it from having to reason about interactions with the rest.
pub const MEMBERSHIP_SUBTYPES: [&str; 4] =
    ["channel_join", "channel_leave", "group_join", "group_leave"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Drop,
    Store,
    StoreAndRegisterThread,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeDecision {
    pub action: Action,
    pub reason: Option<ScopeReason>,
    pub thread_key: Option<String>,
}

impl ScopeDecision {
    pub fn stores(&self) -> bool {
        self.action != Action::Drop
    }

    const DROP: Self = Self {
        action: Action::Drop,
        reason: None,
        thread_key: None,
    };

    fn store(reason: ScopeReason) -> Self {
        Self {
            action: Action::Store,
            reason: Some(reason),
            thread_key: None,
        }
    }
}

/// Slack renders a user mention as `<@Uxxxx>`, always by ID.
///
/// Matching the literal token rather than a display name is what keeps "ask radek about
/// it" from counting as a ping and keeps a rename from silently ending mention capture.
pub fn mention_token(user_id: &str) -> String {
    format!("<@{user_id}>")
}

/// Decide what to do with one message. Rules are evaluated in order.
///
/// Only direct `<@me>` mentions register a thread: `@channel`, `@here` and user-group
/// pings do not. `@channel` in a busy channel would drag most of that channel into
/// scope, which is the opposite of the intent, and user groups would need a separate
/// membership sync to evaluate. That gap is real and is stated in the README.
pub fn classify(
    message: &Message,
    conversation: &Conversation,
    my_user_id: &str,
    scope_channels: &HashSet<String>,
    known_mention_threads: &HashSet<(String, String)>,
) -> ScopeDecision {
    if message
        .subtype
        .as_deref()
        .is_some_and(|subtype| MEMBERSHIP_SUBTYPES.contains(&subtype))
    {
        return ScopeDecision::DROP;
    }

    match conversation.kind {
        ConversationType::Im => return ScopeDecision::store(ScopeReason::Dm),
        ConversationType::Mpim => return ScopeDecision::store(ScopeReason::GroupDm),
        ConversationType::Channel | ConversationType::Group => {}
    }

    if conversation
        .name
        .as_ref()
        .is_some_and(|name| scope_channels.contains(name))
    {
        return ScopeDecision::store(ScopeReason::NamedChannel);
    }

    // A self-mention is not a ping, so rule 3 never fires on the user's own message.
    // Rules 1 and 2 already ran, so this only narrows the channel case.
    if message.user_id.as_deref() != Some(my_user_id)
        && message.text.contains(&mention_token(my_user_id))
    {
        // A mention on a non-threaded message registers that message as its own thread
        // root, so replies to it are captured from then on.
        return ScopeDecision {
            action: Action::StoreAndRegisterThread,
            reason: Some(ScopeReason::MentionThread),
            thread_key: Some(
                message
                    .thread_ts
                    .clone()
                    .unwrap_or_else(|| message.ts.clone()),
            ),
        };
    }

    if let Some(thread_ts) = message.thread_ts.as_deref()
        && known_mention_threads.contains(&(conversation.id.clone(), thread_ts.to_owned()))
    {
        return ScopeDecision::store(ScopeReason::MentionThread);
    }

    ScopeDecision::DROP
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    const ME: &str = "U0ME";

    fn scope() -> HashSet<String> {
        ["general", "general-dev"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    fn conversation(kind: ConversationType, name: Option<&str>, id: &str) -> Conversation {
        Conversation {
            account_id: "work".into(),
            id: id.into(),
            team_id: None,
            shared_team_ids: Vec::new(),
            kind,
            name: name.map(str::to_owned),
            is_member: false,
            counterpart_user_id: None,
            in_scope: false,
            scope_reason: None,
            poll_cursor: None,
            backfill_cursor: None,
            backfill_done: false,
            reactions_fresh_since: None,
            last_polled_at: None,
            is_ext_shared: false,
        }
    }

    struct Msg<'a> {
        text: &'a str,
        user: &'a str,
        ts: &'a str,
        thread_ts: Option<&'a str>,
        subtype: Option<&'a str>,
    }

    impl Default for Msg<'_> {
        fn default() -> Self {
            Self {
                text: "hello",
                user: "U0TED",
                ts: "100.0",
                thread_ts: None,
                subtype: None,
            }
        }
    }

    fn message(msg: Msg<'_>) -> Message {
        Message {
            account_id: "work".into(),
            conversation_id: "C1".into(),
            ts: msg.ts.into(),
            thread_ts: msg.thread_ts.map(str::to_owned),
            user_id: Some(msg.user.into()),
            text: msg.text.into(),
            subtype: msg.subtype.map(str::to_owned),
            edited_at: None,
            deleted_at: None,
            raw: serde_json::json!({}),
            posted_at: Utc::now(),
            reactions: Vec::new(),
        }
    }

    fn decide(msg: Msg<'_>, conv: &Conversation, threads: &[(&str, &str)]) -> ScopeDecision {
        let known: HashSet<(String, String)> = threads
            .iter()
            .map(|(c, t)| ((*c).to_owned(), (*t).to_owned()))
            .collect();
        classify(&message(msg), conv, ME, &scope(), &known)
    }

    // -------------------------------------------------------------- wholesale scope

    #[test]
    fn a_dm_is_stored() {
        let decision = decide(
            Msg::default(),
            &conversation(ConversationType::Im, None, "D1"),
            &[],
        );
        assert_eq!(decision.reason, Some(ScopeReason::Dm));
    }

    #[test]
    fn a_group_dm_is_stored() {
        let decision = decide(
            Msg::default(),
            &conversation(ConversationType::Mpim, None, "G1"),
            &[],
        );
        assert_eq!(decision.reason, Some(ScopeReason::GroupDm));
    }

    #[test]
    fn named_channels_are_stored() {
        for name in ["general", "general-dev"] {
            let decision = decide(
                Msg::default(),
                &conversation(ConversationType::Channel, Some(name), "C1"),
                &[],
            );
            assert_eq!(decision.reason, Some(ScopeReason::NamedChannel), "{name}");
        }
    }

    #[test]
    fn an_unrelated_channel_is_dropped() {
        let decision = decide(
            Msg::default(),
            &conversation(ConversationType::Channel, Some("random"), "C1"),
            &[],
        );
        assert_eq!(decision.action, Action::Drop);
    }

    // ------------------------------------------------------------------- membership

    #[test]
    fn membership_events_are_dropped_even_in_a_dm() {
        // Rule 0 runs first, so it never has to reason about the rules after it.
        for subtype in MEMBERSHIP_SUBTYPES {
            let msg = || Msg {
                text: "<@U0TED> has joined",
                subtype: Some(subtype),
                ..Msg::default()
            };
            assert_eq!(
                decide(msg(), &conversation(ConversationType::Im, None, "D1"), &[]).action,
                Action::Drop,
                "{subtype}"
            );
            assert_eq!(
                decide(
                    msg(),
                    &conversation(ConversationType::Channel, Some("general"), "C1"),
                    &[]
                )
                .action,
                Action::Drop,
                "{subtype}"
            );
        }
    }

    // --------------------------------------------------------------------- mentions

    #[test]
    fn a_mention_in_an_unrelated_channel_stores_and_registers_the_thread() {
        let decision = decide(
            Msg {
                text: "heads up <@U0ME>",
                ..Msg::default()
            },
            &conversation(ConversationType::Channel, Some("random"), "C1"),
            &[],
        );
        assert_eq!(decision.action, Action::StoreAndRegisterThread);
        assert_eq!(decision.reason, Some(ScopeReason::MentionThread));
    }

    #[test]
    fn a_mention_on_an_unthreaded_message_registers_that_message_as_the_root() {
        // Otherwise a reply that arrives later has no registered thread to match.
        let decision = decide(
            Msg {
                text: "<@U0ME> look",
                ts: "500.0",
                ..Msg::default()
            },
            &conversation(ConversationType::Channel, Some("random"), "C1"),
            &[],
        );
        assert_eq!(decision.thread_key.as_deref(), Some("500.0"));
    }

    #[test]
    fn a_mention_inside_a_thread_registers_the_thread_root() {
        let decision = decide(
            Msg {
                text: "<@U0ME> look",
                ts: "500.0",
                thread_ts: Some("400.0"),
                ..Msg::default()
            },
            &conversation(ConversationType::Channel, Some("random"), "C1"),
            &[],
        );
        assert_eq!(decision.thread_key.as_deref(), Some("400.0"));
    }

    #[test]
    fn a_reply_in_a_registered_thread_is_stored() {
        let decision = decide(
            Msg {
                text: "more context",
                ts: "600.0",
                thread_ts: Some("400.0"),
                ..Msg::default()
            },
            &conversation(ConversationType::Channel, Some("random"), "C1"),
            &[("C1", "400.0")],
        );
        assert_eq!(decision.action, Action::Store);
        assert_eq!(decision.reason, Some(ScopeReason::MentionThread));
    }

    #[test]
    fn a_reply_in_an_unregistered_thread_is_dropped() {
        let decision = decide(
            Msg {
                text: "more context",
                ts: "600.0",
                thread_ts: Some("999.0"),
                ..Msg::default()
            },
            &conversation(ConversationType::Channel, Some("random"), "C1"),
            &[("C1", "400.0")],
        );
        assert_eq!(decision.action, Action::Drop);
    }

    #[test]
    fn registered_threads_are_scoped_to_their_conversation() {
        let decision = decide(
            Msg {
                text: "more context",
                ts: "600.0",
                thread_ts: Some("400.0"),
                ..Msg::default()
            },
            &conversation(ConversationType::Channel, Some("random"), "C2"),
            &[("C1", "400.0")],
        );
        assert_eq!(decision.action, Action::Drop);
    }

    #[test]
    fn a_mention_of_someone_else_is_dropped() {
        let decision = decide(
            Msg {
                text: "<@U0ANN> ping",
                ..Msg::default()
            },
            &conversation(ConversationType::Channel, Some("random"), "C1"),
            &[],
        );
        assert_eq!(decision.action, Action::Drop);
    }

    #[test]
    fn a_self_mention_does_not_register_a_thread() {
        // Quoting your own handle is not someone pinging you.
        let decision = decide(
            Msg {
                text: "as <@U0ME> said",
                user: ME,
                ..Msg::default()
            },
            &conversation(ConversationType::Channel, Some("random"), "C1"),
            &[],
        );
        assert_eq!(decision.action, Action::Drop);
    }

    #[test]
    fn my_own_message_in_a_named_channel_is_still_stored() {
        let decision = decide(
            Msg {
                text: "shipping it",
                user: ME,
                ..Msg::default()
            },
            &conversation(ConversationType::Channel, Some("general"), "C1"),
            &[],
        );
        assert_eq!(decision.reason, Some(ScopeReason::NamedChannel));
    }

    #[test]
    fn a_display_name_is_not_a_mention() {
        // Slack stores mentions as IDs; matching prose would both miss renames and fire
        // on anyone who happens to type the name.
        let decision = decide(
            Msg {
                text: "ask radek about it",
                ..Msg::default()
            },
            &conversation(ConversationType::Channel, Some("random"), "C1"),
            &[],
        );
        assert_eq!(decision.action, Action::Drop);
    }

    #[test]
    fn broadcast_and_group_pings_do_not_register_a_thread() {
        // A documented v1 gap: `@here` would drag most of a busy channel into scope,
        // and user groups need a membership sync to evaluate. The README says so.
        for token in ["<!channel>", "<!here>", "<!subteam^S123|@team>"] {
            let text = format!("{token} deploying");
            let decision = decide(
                Msg {
                    text: &text,
                    ..Msg::default()
                },
                &conversation(ConversationType::Channel, Some("random"), "C1"),
                &[],
            );
            assert_eq!(decision.action, Action::Drop, "{token}");
        }
    }
}
