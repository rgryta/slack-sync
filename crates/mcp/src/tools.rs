//! The eleven read tools, as plain async functions over a pool.
//!
//! Keeping them out of the server object is what lets the tests call them directly,
//! and keeps the MCP wiring to registration. Nothing here holds SQL — every statement
//! lives in `slack_core::store` — and nothing in this crate depends on `slack-client`:
//! this process holds no Slack credential and the linker makes reaching Slack
//! impossible rather than merely discouraged.

use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use slack_core::models::{Identity, Message};
use slack_core::store::{self, Directory};
use sqlx::postgres::PgPool;

use crate::render::{
    collapse_note, dedupe, render_message, render_messages, render_revisions, when, window_note,
};

pub const MAX_LIMIT: i64 = 200;
pub const DEFAULT_LIMIT: i64 = 50;

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// The caller's fault, and reported back to them as text rather than logged.
    #[error("{0}")]
    Usage(String),
    #[error(transparent)]
    Sql(#[from] sqlx::Error),
}

type Result<T> = std::result::Result<T, ToolError>;

/// A caller asking for 10_000 gets 200, not an error and not 10_000.
pub fn clamp(limit: Option<i64>) -> i64 {
    match limit {
        Some(limit) if limit >= 1 => limit.min(MAX_LIMIT),
        _ => DEFAULT_LIMIT,
    }
}

/// Accept an ISO timestamp or a Slack `ts`; anything else is a usage error.
///
/// A value with no offset is read in the server's own timezone, exactly as
/// `datetime.fromisoformat(...).astimezone(UTC)` did: the container sets `TZ`, and a
/// caller typing a plain date means a day where they live.
pub fn parse_when(value: Option<&str>) -> Result<Option<DateTime<Utc>>> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let normalised = value.replace('Z', "+00:00");
    if let Ok(moment) = DateTime::parse_from_rfc3339(&normalised) {
        return Ok(Some(moment.with_timezone(&Utc)));
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(value, format) {
            return Ok(Some(local_to_utc(naive)?));
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Ok(Some(local_to_utc(
            date.and_hms_opt(0, 0, 0).expect("midnight exists"),
        )?));
    }
    // A Slack `ts` is seconds since the epoch with a fractional part.
    if let Ok(epoch) = value.parse::<f64>() {
        let seconds = epoch.trunc() as i64;
        let nanos = ((epoch - epoch.trunc()) * 1e9).round() as u32;
        if let Some(moment) = DateTime::from_timestamp(seconds, nanos) {
            return Ok(Some(moment));
        }
    }
    Err(ToolError::Usage(format!(
        "could not read {value:?} as a time"
    )))
}

fn local_to_utc(naive: NaiveDateTime) -> Result<DateTime<Utc>> {
    // A clock change can make a wall-clock time ambiguous or non-existent; the earlier
    // of two candidates is the same choice Python's `astimezone` makes.
    Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|moment| moment.with_timezone(&Utc))
        .ok_or_else(|| {
            ToolError::Usage(format!(
                "{naive} does not exist in the server's timezone (a clock change)"
            ))
        })
}

async fn directory_of(pool: &PgPool) -> Result<Directory> {
    Ok(Directory::load(pool).await?)
}

/// A named workspace resolves to a team ID, or to a filter that matches nothing.
///
/// A typo must narrow to empty rather than silently widen to every team.
async fn team_filter(pool: &PgPool, workspace: Option<&str>) -> Result<Option<String>> {
    let Some(workspace) = workspace else {
        return Ok(None);
    };
    // A team ID Slack can never issue (its IDs are `T` plus alphanumerics), so an
    // unresolvable name filters everything out instead of matching a real team.
    Ok(Some(
        store::resolve_workspace(pool, workspace)
            .await?
            .unwrap_or_else(|| "T-no-such-workspace".to_owned()),
    ))
}

// ----------------------------------------------------------------------------- tools

pub async fn list_conversations(
    pool: &PgPool,
    account: Option<&str>,
    workspace: Option<&str>,
    kind: Option<&str>,
) -> Result<String> {
    let directory = directory_of(pool).await?;
    let team_id = team_filter(pool, workspace).await?;
    let summaries = store::list_conversations(pool, account, team_id.as_deref(), kind).await?;
    if summaries.is_empty() {
        return Ok("no in-scope conversations matched".to_owned());
    }
    let mut lines = vec![format!("{} in-scope conversation(s)", summaries.len())];
    for summary in &summaries {
        lines.push(format!(
            "[{}/{}] {} ({}, id={}) {} messages, last activity {}, scope={}, polled {}",
            summary.account_id,
            directory.workspace_label(summary.team_id.as_deref()),
            directory.conversation_label(&summary.account_id, &summary.id),
            summary.kind,
            summary.id,
            summary.message_count,
            when(summary.last_activity),
            summary
                .scope_reason
                .map(|reason| reason.as_str())
                .unwrap_or("None"),
            when(summary.last_polled_at),
        ));
    }
    Ok(lines.join("\n"))
}

pub async fn history(
    pool: &PgPool,
    conversation: &str,
    account: Option<&str>,
    after: Option<&str>,
    before: Option<&str>,
    limit: Option<i64>,
) -> Result<String> {
    let directory = directory_of(pool).await?;
    let after = parse_when(after)?;
    let before = parse_when(before)?;
    let page = store::get_history(pool, conversation, account, after, before, clamp(limit)).await?;
    let total = store::count_history(pool, conversation, account, after, before).await?;
    let more = match (page.first(), page.last()) {
        (Some(first), Some(last)) => window_note(
            total - page.len() as i64,
            first.posted_at,
            last.posted_at,
            after.is_some(),
        ),
        _ => Vec::new(),
    };
    let (messages, collapsed) = dedupe(page, &directory);
    let label = directory.any_conversation_label(conversation, account);
    let mut footer = collapse_note(collapsed);
    footer.extend(more);
    Ok(render_messages(
        &messages,
        &directory,
        &format!("history of {label} ({conversation}), oldest first"),
        true,
        &footer,
    ))
}

pub async fn thread(
    pool: &PgPool,
    conversation: &str,
    thread_ts: &str,
    account: Option<&str>,
) -> Result<String> {
    let directory = directory_of(pool).await?;
    let messages = store::get_thread(pool, conversation, thread_ts, account).await?;
    let (messages, collapsed) = dedupe(messages, &directory);
    Ok(render_messages(
        &messages,
        &directory,
        &format!("thread {thread_ts} in {conversation}"),
        true,
        &collapse_note(collapsed),
    ))
}

/// Full-text search. `author` accepts an alias or a raw user ID.
///
/// An alias resolves to every identity attached to it, so `author: "ted"` means Ted in
/// every account rather than Ted in one workspace.
#[allow(clippy::too_many_arguments)]
pub async fn search(
    pool: &PgPool,
    query: &str,
    account: Option<&str>,
    workspace: Option<&str>,
    conversation: Option<&str>,
    author: Option<&str>,
    after: Option<&str>,
    before: Option<&str>,
    has_reaction: Option<bool>,
    limit: Option<i64>,
) -> Result<String> {
    let directory = directory_of(pool).await?;
    let authors: Option<Vec<Identity>> = match author {
        Some(author) => {
            let resolved = store::resolve_author(pool, author).await?;
            if resolved.is_empty() {
                return Ok(format!("no user or alias matched '{author}'"));
            }
            Some(resolved)
        }
        None => None,
    };
    let team_id = team_filter(pool, workspace).await?;
    let hits = store::search_messages(
        pool,
        query,
        account,
        team_id.as_deref(),
        conversation,
        authors.as_deref(),
        parse_when(after)?,
        parse_when(before)?,
        has_reaction,
        clamp(limit),
    )
    .await?;

    let ranked: Vec<(String, f32, String)> = hits
        .iter()
        .map(|hit| (hit_key(&hit.message), hit.rank, hit.snippet.clone()))
        .collect();
    let (messages, collapsed) = dedupe(
        hits.into_iter().map(|hit| hit.message).collect(),
        &directory,
    );

    let mut lines = vec![format!("{} result(s) for '{query}'", messages.len())];
    if messages.is_empty() {
        lines.push("(nothing matched)".to_owned());
    }

    // Ranked, not chronological: the caller asked "where is this mentioned", and the
    // best match being buried under three newer weak ones is the wrong answer.
    let mut messages = messages;
    messages.sort_by(|a, b| {
        let (rank_a, rank_b) = (rank_of(&ranked, a), rank_of(&ranked, b));
        rank_b
            .total_cmp(&rank_a)
            .then_with(|| b.posted_at.cmp(&a.posted_at))
    });
    for message in &messages {
        lines.push(render_message(message, &directory, true));
        if let Some(snippet) = snippet_of(&ranked, message)
            && !snippet.is_empty()
            && snippet != message.text
        {
            lines.push(format!("    \u{2026}{snippet}\u{2026}"));
        }
    }
    lines.extend(collapse_note(collapsed));
    Ok(lines.join("\n"))
}

fn hit_key(message: &Message) -> String {
    format!(
        "{}\u{0}{}\u{0}{}",
        message.account_id, message.conversation_id, message.ts
    )
}

fn rank_of(ranked: &[(String, f32, String)], message: &Message) -> f32 {
    let key = hit_key(message);
    ranked
        .iter()
        .find(|(hit, _, _)| *hit == key)
        .map(|(_, rank, _)| *rank)
        .unwrap_or_default()
}

fn snippet_of<'a>(ranked: &'a [(String, f32, String)], message: &Message) -> Option<&'a str> {
    let key = hit_key(message);
    ranked
        .iter()
        .find(|(hit, _, _)| *hit == key)
        .map(|(_, _, snippet)| snippet.as_str())
}

pub async fn my_mentions(
    pool: &PgPool,
    account: Option<&str>,
    workspace: Option<&str>,
    after: Option<&str>,
    limit: Option<i64>,
) -> Result<String> {
    let directory = directory_of(pool).await?;
    let accounts = store::list_accounts(pool).await?;
    let wanted: Vec<_> = accounts
        .iter()
        .filter(|owner| account.is_none_or(|wanted| owner.id == wanted))
        .collect();
    if wanted.is_empty() {
        return Ok(format!("no such account: {}", account.unwrap_or("None")));
    }
    let team_id = team_filter(pool, workspace).await?;
    let after = parse_when(after)?;
    let mut messages: Vec<Message> = Vec::new();
    for owner in wanted {
        messages.extend(
            store::list_mentions(
                pool,
                &owner.my_user_id,
                Some(&owner.id),
                team_id.as_deref(),
                after,
                clamp(limit),
            )
            .await?,
        );
    }
    messages.sort_by_key(|message| std::cmp::Reverse(message.posted_at));
    messages.truncate(clamp(limit) as usize);
    let header = format!("{} message(s) mentioning you, newest first", messages.len());
    Ok(render_messages(&messages, &directory, &header, true, &[]))
}

/// Everything in scope after a timestamp, grouped by conversation.
///
/// Not called `unread_since`: nothing here tracks read state, and a name promising
/// unreads would claim something the data cannot back.
pub async fn catch_up(
    pool: &PgPool,
    since: &str,
    account: Option<&str>,
    workspace: Option<&str>,
    limit: Option<i64>,
) -> Result<String> {
    let moment = parse_when(Some(since))?
        .ok_or_else(|| ToolError::Usage("`since` is required".to_owned()))?;
    let directory = directory_of(pool).await?;
    let team_id = team_filter(pool, workspace).await?;
    let messages = store::messages_since(
        pool,
        moment,
        account,
        team_id.as_deref(),
        // Widened deliberately: `limit` is the caller's sense of "how much to read",
        // but the rows are about to be grouped across conversations and deduplicated,
        // so fetching exactly `limit` would leave the quietest conversation empty.
        clamp(limit) * 4,
    )
    .await?;
    let (messages, collapsed) = dedupe(messages, &directory);

    // Grouped in first-appearance order, which is chronological: a map would reorder
    // the conversations between two identical requests.
    let mut grouped: Vec<((String, String), Vec<&Message>)> = Vec::new();
    for message in &messages {
        let key = (message.account_id.clone(), message.conversation_id.clone());
        match grouped.iter_mut().find(|(seen, _)| *seen == key) {
            Some((_, group)) => group.push(message),
            None => grouped.push((key, vec![message])),
        }
    }

    let mut lines = vec![format!(
        "{} message(s) since {} across {} conversation(s)",
        messages.len(),
        when(Some(moment)),
        grouped.len()
    )];
    for ((account_id, conversation_id), group) in &grouped {
        let team = directory.team_of(account_id, conversation_id);
        lines.push(format!(
            "\n[{account_id}/{}] {}",
            directory.workspace_label(team),
            directory.conversation_label(account_id, conversation_id),
        ));
        lines.extend(
            group
                .iter()
                .map(|message| render_message(message, &directory, false)),
        );
    }
    lines.extend(collapse_note(collapsed));
    Ok(lines.join("\n"))
}

pub async fn list_workspaces(pool: &PgPool) -> Result<String> {
    let summaries = store::list_workspaces(pool).await?;
    if summaries.is_empty() {
        return Ok("no workspaces known yet".to_owned());
    }
    let mut lines = vec!["known workspaces".to_owned()];
    for summary in &summaries {
        lines.push(format!(
            "{} {} [{}] {} in-scope conversation(s)",
            summary.team_id,
            summary
                .domain
                .as_deref()
                .or(summary.name.as_deref())
                .unwrap_or("(unnamed)"),
            if summary.is_external {
                "external"
            } else {
                "own"
            },
            summary.conversation_count,
        ));
    }
    Ok(lines.join("\n"))
}

/// The one tool that writes, and it writes only the two alias tables.
///
/// A member already attached to another alias is moved, and the move is reported
/// rather than applied silently.
pub async fn set_alias(
    pool: &PgPool,
    alias: &str,
    members: &[String],
    note: Option<&str>,
) -> Result<String> {
    let mut identities = Vec::with_capacity(members.len());
    for member in members {
        let (account_id, user_id) = member
            .split_once(':')
            .filter(|(_, user_id)| !user_id.is_empty())
            .ok_or_else(|| {
                ToolError::Usage(format!(
                    "member '{member}' must be written as account:user_id"
                ))
            })?;
        identities.push(Identity {
            account_id: account_id.to_owned(),
            user_id: user_id.to_owned(),
        });
    }

    let moved = store::set_alias(pool, alias, &identities, note).await?;
    if identities.is_empty() {
        return Ok(format!("alias '{alias}' deleted"));
    }

    let directory = directory_of(pool).await?;
    let named: Vec<String> = identities
        .iter()
        .map(|i| {
            format!(
                "{}:{} ({})",
                i.account_id,
                i.user_id,
                directory.slack_name(&i.account_id, &i.user_id)
            )
        })
        .collect();
    let mut lines = vec![format!("alias '{alias}' now names {}", named.join(", "))];
    if !moved.is_empty() {
        let moved: Vec<String> = moved
            .iter()
            .map(|i| format!("{}:{}", i.account_id, i.user_id))
            .collect();
        lines.push(format!("moved off another alias: {}", moved.join(", ")));
    }
    Ok(lines.join("\n"))
}

pub async fn list_aliases(pool: &PgPool, alias: Option<&str>) -> Result<String> {
    let directory = directory_of(pool).await?;
    let aliases = store::list_aliases(pool, alias).await?;
    if aliases.is_empty() {
        return Ok("no aliases set".to_owned());
    }
    let mut lines = Vec::with_capacity(aliases.len());
    for entry in &aliases {
        let members: Vec<String> = entry
            .members
            .iter()
            .map(|m| {
                format!(
                    "{}:{} ({})",
                    m.account_id,
                    m.user_id,
                    directory.slack_name(&m.account_id, &m.user_id)
                )
            })
            .collect();
        let note = match &entry.note {
            Some(note) => format!(" \u{2014} {note}"),
            None => String::new(),
        };
        let members = if members.is_empty() {
            "(no members)".to_owned()
        } else {
            members.join(", ")
        };
        lines.push(format!("{}{note}: {members}", entry.alias));
    }
    Ok(lines.join("\n"))
}

pub async fn sync_status(pool: &PgPool, account: Option<&str>) -> Result<String> {
    let rows = store::sync_status(pool, account).await?;
    if rows.is_empty() {
        return Ok("no accounts registered yet".to_owned());
    }
    let mut lines = vec!["ingest status".to_owned()];
    for row in &rows {
        lines.push(format!(
            "{} ({}, {}): {}/{} conversations backfilled, last poll {}, \
             reactions fresh since {}",
            row.account_id,
            row.team_id,
            if row.enabled { "enabled" } else { "disabled" },
            row.backfilled_count,
            row.in_scope_count,
            when(row.last_polled_at),
            when(row.reactions_fresh_since),
        ));
        if let Some(error) = &row.last_auth_error {
            lines.push(format!(
                "  session failing since {}: {error} \u{2014} data is frozen until the \
                 token is replaced",
                when(row.last_auth_error_at),
            ));
        }
    }
    Ok(lines.join("\n"))
}

/// One message's text over time, and the deleted text if it was deleted.
///
/// `ts` is only unique within a conversation, so both are required.
pub async fn revisions(
    pool: &PgPool,
    conversation: &str,
    ts: &str,
    account: Option<&str>,
) -> Result<String> {
    let Some(resolved) = store::resolve_message(pool, conversation, ts, account).await? else {
        return Ok(format!("no message {ts} in {conversation}"));
    };
    let rows =
        store::list_revisions(pool, &resolved.account_id, &resolved.conversation_id, ts).await?;
    Ok(render_revisions(
        &rows,
        &format!(
            "{} revision(s) of {ts} in {conversation}, oldest first",
            rows.len()
        ),
        resolved.deleted_at,
        // Slack's own claim, which is a different fact from ours: a message edited
        // before this archive first read it carries this and no revisions at all, and
        // saying so beats implying we hold a history we never saw.
        resolved.edited_at,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_are_clamped() {
        assert_eq!(clamp(None), 50);
        assert_eq!(clamp(Some(0)), 50);
        assert_eq!(clamp(Some(-3)), 50);
        assert_eq!(clamp(Some(10)), 10);
        assert_eq!(clamp(Some(10_000)), 200);
    }

    #[test]
    fn an_iso_timestamp_with_an_offset_is_read_as_written() {
        let parsed = parse_when(Some("2026-08-14T10:00:00+02:00"))
            .unwrap()
            .unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-08-14T08:00:00+00:00");
        let zulu = parse_when(Some("2026-08-14T10:00:00Z")).unwrap().unwrap();
        assert_eq!(zulu.to_rfc3339(), "2026-08-14T10:00:00+00:00");
    }

    #[test]
    fn a_slack_ts_is_accepted_as_a_time() {
        let parsed = parse_when(Some("1723640400.001200")).unwrap().unwrap();
        assert_eq!(parsed.timestamp(), 1_723_640_400);
    }

    #[test]
    fn a_plain_date_is_fine() {
        assert!(parse_when(Some("2026-07-22")).unwrap().is_some());
    }

    #[test]
    fn an_unreadable_value_is_a_usage_error() {
        let err = parse_when(Some("last tuesday")).unwrap_err();
        assert!(
            matches!(&err, ToolError::Usage(message) if message.contains("could not read")),
            "{err}"
        );
    }

    #[test]
    fn no_value_is_no_filter_rather_than_an_error() {
        assert!(parse_when(None).unwrap().is_none());
        assert!(parse_when(Some("")).unwrap().is_none());
    }

    #[test]
    fn a_member_without_a_user_id_is_rejected_before_any_sql_runs() {
        // The parse is what the test suite pins; it must reject before the write.
        for bad in ["U0TED", "work:", ""] {
            assert!(
                bad.split_once(':')
                    .filter(|(_, user)| !user.is_empty())
                    .is_none(),
                "{bad} should not parse"
            );
        }
        assert_eq!("work:U0TED".split_once(':'), Some(("work", "U0TED")));
    }
}
