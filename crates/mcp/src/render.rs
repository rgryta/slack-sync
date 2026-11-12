//! Compact text rendering.
//!
//! Output is prose, not JSON: Slack payloads are large enough to eat a context window
//! for no benefit, and a model reads a rendered line better than a nested object.
//! Every line keeps its raw `ts`, because that is the key every drill-in tool needs.

use std::collections::{BTreeSet, HashSet};
use std::sync::LazyLock;

use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use regex::{Captures, Regex};
use slack_core::models::{Message, Revision};
use slack_core::store::Directory;

// All five are plain patterns — no lookaround, no backreferences — so the `regex`
// crate takes them as written.
static USER_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<@([A-Z0-9]+)(?:\|[^>]*)?>").unwrap());
static CHANNEL_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<#([A-Z0-9]+)(?:\|([^>]*))?>").unwrap());
static LINK_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<(https?://[^|>]+)(?:\|([^>]*))?>").unwrap());
static BROADCAST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<!(channel|here|everyone)>").unwrap());
static EMOJI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r":([a-z0-9_+-]+):").unwrap());

/// A capture group that matched nothing, or matched the empty string, is an absence —
/// `<#C0ABCD|>` must fall back to the ID exactly as a group-less match does.
fn non_empty<'a>(caps: &Captures<'a>, index: usize) -> Option<&'a str> {
    caps.get(index)
        .map(|m| m.as_str())
        .filter(|text| !text.is_empty())
}

/// ISO 8601 in UTC, to the minute.
///
/// `2026-08-14 20:50` is ambiguous — it drops the offset, so a model cannot tell it
/// from UTC and cannot order it against anything else. UTC with an explicit offset is
/// unambiguous, directly parseable, and comparable to whatever else is on screen.
pub fn when(moment: Option<DateTime<Utc>>) -> String {
    match moment {
        // UTC, not local: these timestamps are read alongside output from other tools,
        // and a shared zero point makes them directly comparable.
        Some(moment) => moment.format("%Y-%m-%dT%H:%M+00:00").to_string(),
        None => "never".to_owned(),
    }
}

/// Replace Slack's reference syntax with something readable.
pub fn substitute(text: &str, directory: &Directory, account_id: &str) -> String {
    let text = USER_REF.replace_all(text, |caps: &Captures| {
        format!("@{}", directory.user_name(account_id, &caps[1]))
    });
    let text = CHANNEL_REF.replace_all(&text, |caps: &Captures| {
        format!("#{}", non_empty(caps, 2).unwrap_or(&caps[1]))
    });
    let text = LINK_REF.replace_all(&text, |caps: &Captures| match non_empty(caps, 2) {
        Some(label) => format!("{label} ({})", &caps[1]),
        None => caps[1].to_owned(),
    });
    let text = BROADCAST.replace_all(&text, |caps: &Captures| format!("@{}", &caps[1]));
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

/// Custom emoji that actually resolve in this workspace.
///
/// Listing them lets the model tell a workspace emoji from a typo, which it cannot do
/// from `:shipit:` alone.
pub fn emoji_legend(texts: &[&str], directory: &Directory, team_id: Option<&str>) -> Vec<String> {
    let mut names: BTreeSet<&str> = BTreeSet::new();
    for text in texts {
        for caps in EMOJI.captures_iter(text) {
            let name = caps.get(1).expect("group 1 is not optional").as_str();
            if directory.known_emoji(team_id, name) {
                names.insert(name);
            }
        }
    }
    names.into_iter().map(str::to_owned).collect()
}

/// One message as one line, with its reactions inline.
pub fn render_message(message: &Message, directory: &Directory, with_label: bool) -> String {
    let mut line = String::new();
    if with_label {
        let team = directory.team_of(&message.account_id, &message.conversation_id);
        line.push_str(&format!(
            "[{}/{} {}] ",
            message.account_id,
            directory.workspace_label(team),
            directory.conversation_label(&message.account_id, &message.conversation_id),
        ));
    }
    let author = match &message.user_id {
        Some(user_id) => directory.user_name(&message.account_id, user_id),
        None => "unknown".to_owned(),
    };
    let body = if message.deleted_at.is_some() {
        // The original text is suppressed rather than shown struck through: it was
        // deleted, and reproducing it would defeat the deletion.
        "[deleted]".to_owned()
    } else {
        let mut body = substitute(&message.text, directory, &message.account_id);
        if body.is_empty() {
            body = "[no text]".to_owned();
        }
        if message.edited_at.is_some() {
            body.push_str(" (edited)");
        }
        body
    };
    line.push_str(&format!(
        "{} {} {author}: {body}",
        when(Some(message.posted_at)),
        message.ts,
    ));
    if let Some(subtype) = &message.subtype {
        line.push_str(&format!(" [{subtype}]"));
    }
    if !message.reactions.is_empty() {
        let reactions: Vec<String> = message
            .reactions
            .iter()
            .map(|r| format!(":{}: \u{d7}{}", r.name, r.count))
            .collect();
        line.push_str(&format!("  \u{2014} {}", reactions.join(" ")));
    }
    if let Some(thread_ts) = &message.thread_ts {
        line.push_str(&format!("  (in thread {thread_ts})"));
    }
    if let Some(replies) = message.reply_count().filter(|count| *count != 0) {
        // Without this a thread parent looks like an ordinary message and the model has
        // no reason to call slack-thread, so the answers stay invisible.
        line.push_str(&format!(
            "  (thread root, {replies} replies \u{2014} slack-thread thread_ts={})",
            message.ts
        ));
    }
    line
}

/// Say so when a shown reaction count is older than the refresh window.
///
/// Reactions are only re-read inside `SLACK_REACTION_WINDOW` days, so an older count is
/// a floor rather than a current value — and a conversation that has never been
/// refreshed is entirely stale, not entirely fresh.
pub fn stale_reaction_note(messages: &[Message], directory: &Directory) -> Option<String> {
    let stale = messages
        .iter()
        .filter(|message| !message.reactions.is_empty())
        .filter(|message| {
            match directory.reactions_fresh_since(&message.account_id, &message.conversation_id) {
                Some(fresh_since) => message.posted_at < fresh_since,
                None => true,
            }
        })
        .count();
    if stale == 0 {
        return None;
    }
    Some(format!(
        "note: reaction counts on {stale} message(s) predate the refresh window; \
         treat them as a floor, not a current value"
    ))
}

/// Last-poll times for the conversations involved, plus any staleness note.
pub fn freshness_footer(messages: &[Message], directory: &Directory) -> Vec<String> {
    let keys: BTreeSet<(&str, &str)> = messages
        .iter()
        .map(|m| (m.account_id.as_str(), m.conversation_id.as_str()))
        .collect();
    let mut lines = Vec::new();
    if !keys.is_empty() {
        let polled: Vec<String> = keys
            .into_iter()
            .map(|(account, conversation)| {
                format!(
                    "{account}/{} {}",
                    directory.conversation_label(account, conversation),
                    when(directory.last_polled_at(account, conversation)),
                )
            })
            .collect();
        lines.push(format!("last polled: {}", polled.join(", ")));
    }
    if let Some(note) = stale_reaction_note(messages, directory) {
        lines.push(note);
    }
    lines
}

/// A block of messages with its header and freshness footer.
///
/// An empty result is a rendered answer with a footer, not an error: "nothing matched"
/// is information, and raising would make the caller guess why.
pub fn render_messages(
    messages: &[Message],
    directory: &Directory,
    header: &str,
    with_label: bool,
    extra_footer: &[String],
) -> String {
    let mut lines = vec![header.to_owned()];
    if messages.is_empty() {
        lines.push("(no messages matched)".to_owned());
    } else {
        lines.extend(
            messages
                .iter()
                .map(|message| render_message(message, directory, with_label)),
        );
        let first = &messages[0];
        let team = directory.team_of(&first.account_id, &first.conversation_id);
        let texts: Vec<&str> = messages.iter().map(|m| m.text.as_str()).collect();
        let legend = emoji_legend(&texts, directory, team);
        if !legend.is_empty() {
            let rendered: Vec<String> = legend.iter().map(|name| format!(":{name}:")).collect();
            lines.push(format!("custom emoji seen: {}", rendered.join(", ")));
        }
    }
    lines.extend(extra_footer.iter().cloned());
    lines.extend(freshness_footer(messages, directory));
    lines.join("\n")
}

/// One message's text over time, oldest first.
///
/// The only place a deleted message's text is shown. Every other view withholds it on
/// purpose; asking for the history is asking for exactly this.
pub fn render_revisions(
    revisions: &[Revision],
    header: &str,
    deleted_at: Option<DateTime<Utc>>,
    provider_edited_at: Option<DateTime<Utc>>,
) -> String {
    let mut lines = vec![header.to_owned()];
    for revision in revisions {
        // A NULL start is unknown, not missing: nothing records when a message was
        // first stored, so the earliest text held has no honest start time.
        let start = match revision.first_seen_at {
            Some(moment) => when(Some(moment)),
            None => "unknown".to_owned(),
        };
        let end = match revision.superseded_at {
            Some(moment) => when(Some(moment)),
            None => "current".to_owned(),
        };
        let text = revision
            .text
            .as_deref()
            .filter(|text| !text.is_empty())
            .unwrap_or("(no text)");
        lines.push(format!("  {}  {start} -> {end}   \"{text}\"", revision.seq));
    }
    if deleted_at.is_some() {
        lines.push(format!("deleted {}", when(deleted_at)));
    }
    if !revisions.is_empty() {
        lines.push(
            "note: revision 1 is the earliest text this archive saw, which is not \
             necessarily what was first sent."
                .to_owned(),
        );
    } else if provider_edited_at.is_some() {
        // The two signals disagree, and that is a correct state rather than a fault.
        lines.push(format!(
            "Slack reports this message was edited at {}, but the change happened \
             before this archive first read it, so no earlier text was ever stored.",
            when(provider_edited_at)
        ));
    } else if deleted_at.is_none() {
        lines.push("no recorded changes".to_owned());
    }
    lines.join("\n")
}

/// Say how much of the requested period was not shown, and the call that fetches it.
///
/// Without this a capped page is indistinguishable from the whole period, and a caller
/// who asked for July reads the last day of it as all of July. The cursor overlaps the
/// page by a second so that a message sharing the boundary's timestamp is never
/// skipped.
pub fn window_note(
    more: i64,
    first: DateTime<Utc>,
    last: DateTime<Utc>,
    oldest_first: bool,
) -> Vec<String> {
    if more <= 0 {
        return Vec::new();
    }
    let second = TimeDelta::seconds(1);
    if oldest_first {
        let cursor = (last - second).to_rfc3339_opts(SecondsFormat::Secs, true);
        return vec![format!(
            "note: {more} more message(s) in this period after the last one shown - \
             next page: after={cursor} (a second of overlap, so none is skipped)"
        )];
    }
    let cursor = (first + second).to_rfc3339_opts(SecondsFormat::Secs, true);
    vec![format!(
        "note: {more} older message(s) match this request - \
         next page: before={cursor} (a second of overlap, so none is skipped)"
    )]
}

/// Collapse the same message seen through two accounts in the same workspace.
///
/// Two accounts can both sit in one channel, and each stores its own copy — that is
/// deliberate on the write side, because which account can see a message is itself
/// information. On read it is noise, so identical `(team, conversation, ts)` rows
/// collapse to the lexically first account and the count of what was dropped is
/// reported rather than hidden.
pub fn dedupe(mut messages: Vec<Message>, directory: &Directory) -> (Vec<Message>, usize) {
    messages.sort_by(|a, b| a.account_id.cmp(&b.account_id));
    let mut seen: HashSet<(Option<String>, String, String)> = HashSet::new();
    let mut kept: Vec<Message> = Vec::with_capacity(messages.len());
    let mut collapsed = 0;
    for message in messages {
        let team = directory
            .team_of(&message.account_id, &message.conversation_id)
            .map(str::to_owned);
        let key = (team, message.conversation_id.clone(), message.ts.clone());
        if seen.insert(key) {
            kept.push(message);
        } else {
            collapsed += 1;
        }
    }
    kept.sort_by(|a, b| (a.posted_at, &a.ts).cmp(&(b.posted_at, &b.ts)));
    (kept, collapsed)
}

pub fn collapse_note(collapsed: usize) -> Vec<String> {
    if collapsed == 0 {
        return Vec::new();
    }
    vec![format!(
        "note: {collapsed} duplicate row(s) from a second account collapsed"
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use slack_core::models::{ConversationType, Reaction};
    use slack_core::store::{DirectoryConversation, DirectoryUser, DirectoryWorkspace};

    fn now() -> DateTime<Utc> {
        "2026-08-14T10:00:00Z".parse().unwrap()
    }

    fn directory() -> Directory {
        let mut directory = Directory::default();
        directory.users.insert(
            ("work".into(), "U0TED".into()),
            DirectoryUser {
                name: Some("ted".into()),
                real_name: Some("Ted Lasso".into()),
                display_name: Some("Coach Ted".into()),
            },
        );
        directory
            .aliases
            .insert(("work".into(), "U0TED".into()), "coach".into());
        directory.conversations.insert(
            ("work".into(), "C0GENERAL".into()),
            DirectoryConversation {
                name: Some("general".into()),
                team_id: Some("T0001".into()),
                kind: ConversationType::Channel,
                counterpart_user_id: None,
                last_polled_at: Some(now()),
                reactions_fresh_since: Some(now() - TimeDelta::days(7)),
            },
        );
        directory.workspaces.insert(
            "T0001".into(),
            DirectoryWorkspace {
                name: Some("Acme".into()),
                domain: Some("acme".into()),
            },
        );
        directory.emoji.insert(("T0001".into(), "shipit".into()));
        directory
    }

    fn message() -> Message {
        Message {
            account_id: "work".into(),
            conversation_id: "C0GENERAL".into(),
            ts: "1723640400.001200".into(),
            thread_ts: None,
            user_id: Some("U0TED".into()),
            text: "hello".into(),
            subtype: None,
            edited_at: None,
            deleted_at: None,
            raw: serde_json::json!({}),
            posted_at: now(),
            reactions: Vec::new(),
        }
    }

    // ----------------------------------------------------------------- substitution

    #[test]
    fn user_channel_and_link_references_are_readable() {
        let text =
            "hey <@U0TED> see <#C0GENERAL|general> and <https://x.dev|the docs> or <https://y.dev>";
        assert_eq!(
            substitute(text, &directory(), "work"),
            "hey @coach see #general and the docs (https://x.dev) or https://y.dev"
        );
    }

    #[test]
    fn broadcasts_and_entities_are_unescaped() {
        assert_eq!(
            substitute("<!here> a &amp; b &lt;c&gt;", &directory(), "work"),
            "@here a & b <c>"
        );
    }

    #[test]
    fn a_channel_reference_with_an_empty_label_falls_back_to_the_id() {
        assert_eq!(substitute("<#C0NEW|>", &directory(), "work"), "#C0NEW");
    }

    // --------------------------------------------------------------------- messages

    #[test]
    fn a_message_carries_its_raw_ts_and_a_readable_time() {
        // The raw ts stays on the line because it is the key every drill-in tool needs.
        let line = render_message(&message(), &directory(), true);
        assert!(line.contains("1723640400.001200"), "{line}");
        assert!(line.contains(&when(Some(now()))), "{line}");
        assert!(line.contains("[work/acme #general]"), "{line}");
    }

    #[test]
    fn edited_and_deleted_messages_say_so() {
        let edited = Message {
            edited_at: Some(now()),
            ..message()
        };
        assert!(render_message(&edited, &directory(), true).contains("(edited)"));

        let deleted = Message {
            deleted_at: Some(now()),
            text: "the original".into(),
            ..message()
        };
        let line = render_message(&deleted, &directory(), true);
        assert!(line.contains("[deleted]"), "{line}");
        // Reproducing the text would defeat the deletion.
        assert!(!line.contains("the original"), "{line}");
    }

    #[test]
    fn a_message_with_no_text_says_so_rather_than_rendering_blank() {
        let empty = Message {
            text: String::new(),
            ..message()
        };
        assert!(render_message(&empty, &directory(), true).contains("[no text]"));
    }

    #[test]
    fn reactions_render_inline() {
        let reacted = Message {
            reactions: vec![Reaction {
                name: "thumbsup".into(),
                user_ids: vec![],
                count: 3,
            }],
            ..message()
        };
        let line = render_message(&reacted, &directory(), true);
        assert!(line.contains(":thumbsup: \u{d7}3"), "{line}");
    }

    #[test]
    fn custom_emoji_are_listed_when_they_resolve() {
        let shipped = Message {
            text: "ship it :shipit: :nosuchemoji:".into(),
            ..message()
        };
        let body = render_messages(&[shipped], &directory(), "h", true, &[]);
        assert!(body.contains("custom emoji seen: :shipit:"), "{body}");
        let legend = body.split("custom emoji seen:").nth(1).unwrap();
        assert!(!legend.contains(":nosuchemoji:"), "{legend}");
    }

    #[test]
    fn an_empty_result_is_still_an_answer() {
        // "Nothing matched" is information; an error would make the caller guess why.
        let body = render_messages(&[], &directory(), "history of #general", true, &[]);
        assert!(body.contains("no messages matched"), "{body}");
    }

    // ---------------------------------------------------------------------- footers

    #[test]
    fn reactions_older_than_the_window_are_flagged_as_a_floor() {
        let old = Message {
            posted_at: now() - TimeDelta::days(30),
            reactions: vec![Reaction {
                name: "tada".into(),
                user_ids: vec![],
                count: 1,
            }],
            ..message()
        };
        let note = stale_reaction_note(&[old], &directory()).expect("a note");
        assert!(note.contains("floor"), "{note}");
    }

    #[test]
    fn reactions_inside_the_window_are_not_flagged() {
        let fresh = Message {
            reactions: vec![Reaction {
                name: "tada".into(),
                user_ids: vec![],
                count: 1,
            }],
            ..message()
        };
        assert!(stale_reaction_note(&[fresh], &directory()).is_none());
    }

    #[test]
    fn a_never_refreshed_conversation_is_entirely_stale() {
        // A NULL `reactions_fresh_since` means nothing has been re-read, not everything.
        let mut directory = directory();
        directory
            .conversations
            .get_mut(&("work".to_owned(), "C0GENERAL".to_owned()))
            .unwrap()
            .reactions_fresh_since = None;
        let fresh = Message {
            reactions: vec![Reaction {
                name: "tada".into(),
                user_ids: vec![],
                count: 1,
            }],
            ..message()
        };
        assert!(stale_reaction_note(&[fresh], &directory).is_some());
    }

    #[test]
    fn messages_without_reactions_never_trigger_the_note() {
        let old = Message {
            posted_at: now() - TimeDelta::days(30),
            ..message()
        };
        assert!(stale_reaction_note(&[old], &directory()).is_none());
    }

    #[test]
    fn the_footer_reports_the_last_poll() {
        let body = render_messages(&[message()], &directory(), "h", true, &[]);
        assert!(body.contains("last polled:"), "{body}");
    }

    #[test]
    fn timestamps_are_iso_8601_in_utc() {
        // UTC so they compare directly against whatever other tools emit; the offset so
        // nothing has to guess which zone they are in.
        let rendered = when(Some(now()));
        assert_eq!(rendered, "2026-08-14T10:00+00:00");
        assert_eq!(
            DateTime::parse_from_rfc3339("2026-08-14T10:00:00+00:00").unwrap(),
            now()
        );
    }

    #[test]
    fn a_missing_timestamp_reads_as_never() {
        assert_eq!(when(None), "never");
    }

    #[test]
    fn a_thread_root_advertises_its_replies_and_how_to_read_them() {
        // Otherwise a parent looks like an ordinary message, the model never calls
        // slack-thread, and the answers stay invisible.
        let parent = Message {
            raw: serde_json::json!({ "reply_count": 12 }),
            ..message()
        };
        let line = render_message(&parent, &directory(), true);
        assert!(line.contains("12 replies"), "{line}");
        assert!(
            line.contains("slack-thread thread_ts=1723640400.001200"),
            "{line}"
        );
    }

    #[test]
    fn an_ordinary_message_says_nothing_about_threads() {
        assert!(!render_message(&message(), &directory(), true).contains("thread"));
    }

    #[test]
    fn a_capped_page_says_how_many_more_the_period_holds() {
        let first: DateTime<Utc> = "2026-07-22T11:55:02Z".parse().unwrap();
        let last: DateTime<Utc> = "2026-07-26T09:00:00Z".parse().unwrap();

        let forward = window_note(405, first, last, true);
        let backward = window_note(405, first, last, false);

        assert!(forward[0].contains("405 more message(s)"), "{}", forward[0]);
        assert!(
            forward[0].contains("after=2026-07-26T08:59:59Z"),
            "{}",
            forward[0]
        );
        assert!(
            backward[0].contains("405 older message(s)"),
            "{}",
            backward[0]
        );
        assert!(
            backward[0].contains("before=2026-07-22T11:55:03Z"),
            "{}",
            backward[0]
        );
        assert!(window_note(0, first, last, true).is_empty());
    }

    // --------------------------------------------------------------------- revisions

    #[test]
    fn render_says_so_when_slack_and_the_archive_disagree() {
        // A message edited before the archive first read it carries Slack's flag and no
        // revisions, and that combination is correct rather than a fault.
        let out = render_revisions(
            &[],
            "h",
            None,
            Some("2026-08-01T00:00:00Z".parse().unwrap()),
        );
        assert!(out.contains("before this archive first read it"), "{out}");
    }

    #[test]
    fn render_is_not_an_error_when_there_is_no_history() {
        assert!(render_revisions(&[], "h", None, None).contains("no recorded changes"));
    }

    // ------------------------------------------------------------------------ dedupe

    #[test]
    fn the_lexically_first_account_is_the_one_kept() {
        let mut directory = directory();
        directory.conversations.insert(
            ("side".into(), "C0GENERAL".into()),
            DirectoryConversation {
                name: Some("general".into()),
                team_id: Some("T0001".into()),
                kind: ConversationType::Channel,
                counterpart_user_id: None,
                last_polled_at: None,
                reactions_fresh_since: None,
            },
        );
        let (kept, collapsed) = dedupe(
            vec![
                Message {
                    account_id: "work".into(),
                    ..message()
                },
                Message {
                    account_id: "side".into(),
                    ..message()
                },
            ],
            &directory,
        );
        assert_eq!(collapsed, 1);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].account_id, "side");
    }

    #[test]
    fn two_workspaces_never_collapse() {
        let mut directory = directory();
        directory.conversations.insert(
            ("side".into(), "C0GENERAL".into()),
            DirectoryConversation {
                name: Some("general".into()),
                team_id: Some("T0002".into()),
                kind: ConversationType::Channel,
                counterpart_user_id: None,
                last_polled_at: None,
                reactions_fresh_since: None,
            },
        );
        let (kept, collapsed) = dedupe(
            vec![
                Message {
                    account_id: "work".into(),
                    ..message()
                },
                Message {
                    account_id: "side".into(),
                    ..message()
                },
            ],
            &directory,
        );
        assert_eq!((kept.len(), collapsed), (2, 0));
    }
}
