//! Finding conversations and attributing them to workspaces.
//!
//! Runs at the top of every poll pass. Scope from rules 1–2 (DMs, group DMs, named
//! channels) is decided here; mention scope is set lazily when a mention actually lands,
//! because it cannot be known from a conversation listing.

use std::collections::HashSet;

use slack_core::models::{Conversation, ConversationType, ScopeReason, Workspace};
use slack_core::store;
use sqlx::postgres::PgPool;
use tracing::{info, warn};

use crate::api::Slack;
use crate::error::IngestError;

/// Rules 1–2 of the scope filter, applied to a conversation rather than a message.
pub fn scope_for(
    conversation: &Conversation,
    scope_channels: &HashSet<String>,
) -> (bool, Option<ScopeReason>) {
    match conversation.kind {
        ConversationType::Im => (true, Some(ScopeReason::Dm)),
        ConversationType::Mpim => (true, Some(ScopeReason::GroupDm)),
        _ if conversation
            .name
            .as_ref()
            .is_some_and(|name| scope_channels.contains(name)) =>
        {
            (true, Some(ScopeReason::NamedChannel))
        }
        _ => (false, None),
    }
}

/// Upsert every conversation this account is a member of; return them.
///
/// Conversations that vanish from the listing are marked `is_member = false` and their
/// messages are kept: a channel you left does not retroactively become something you
/// never saw.
pub async fn discover<C: Slack>(
    pool: &PgPool,
    client: &C,
    account_id: &str,
    team_id: &str,
    scope_channels: &HashSet<String>,
) -> Result<Vec<Conversation>, IngestError> {
    let mut conversations = client
        .users_conversations(account_id, Some(team_id))
        .await?;
    let mut seen: HashSet<String> = HashSet::new();

    for conversation in &mut conversations {
        resolve_external(client, conversation, account_id, team_id).await;
        upsert_teams(pool, conversation, team_id).await?;
        let (in_scope, reason) = scope_for(conversation, scope_channels);
        conversation.in_scope = in_scope;
        conversation.scope_reason = reason;
        conversation.is_member = true;
        store::upsert_conversation(pool, conversation).await?;
        seen.insert(conversation.id.clone());
    }

    mark_departed(pool, account_id, &seen).await?;
    Ok(conversations)
}

/// Fill in the shared-team fields for a Slack Connect channel.
///
/// `users.conversations` does not reliably carry them, so an externally-shared channel
/// costs one extra call per pass — only channels that declare themselves shared pay it.
async fn resolve_external<C: Slack>(
    client: &C,
    conversation: &mut Conversation,
    account_id: &str,
    team_id: &str,
) {
    if !matches!(
        conversation.kind,
        ConversationType::Channel | ConversationType::Group
    ) {
        return;
    }
    if !conversation.shared_team_ids.is_empty() && conversation.team_id.is_some() {
        return;
    }
    if !conversation.is_ext_shared {
        return;
    }
    match client
        .conversations_info(&conversation.id, account_id, Some(team_id))
        .await
    {
        Ok(resolved) => {
            conversation.team_id = resolved.team_id.or(conversation.team_id.take());
            if !resolved.shared_team_ids.is_empty() {
                conversation.shared_team_ids = resolved.shared_team_ids;
            }
        }
        // A channel we can list but not inspect keeps its listing-level attribution
        // rather than blocking the whole pass. An auth failure is not special here: the
        // next call in the pass raises it, and this one has nothing to record.
        Err(error) => warn!(
            "conversations.info failed for {}: {}",
            conversation.id,
            error.code().unwrap_or("transport")
        ),
    }
}

/// Register every team a conversation touches, so the FK on `conversation` holds.
///
/// External teams get a null name: `team.info` on a foreign team is not reliably
/// available, and a named row would imply we verified something we did not.
async fn upsert_teams(
    pool: &PgPool,
    conversation: &Conversation,
    own_team_id: &str,
) -> Result<(), IngestError> {
    let mut teams: HashSet<&str> = conversation
        .shared_team_ids
        .iter()
        .map(String::as_str)
        .collect();
    if let Some(team) = conversation.team_id.as_deref() {
        teams.insert(team);
    }
    teams.remove(own_team_id);
    for team in teams {
        store::upsert_workspace(
            pool,
            &Workspace {
                team_id: team.to_owned(),
                name: None,
                domain: None,
                is_external: true,
            },
        )
        .await?;
    }
    Ok(())
}

async fn mark_departed(
    pool: &PgPool,
    account_id: &str,
    seen: &HashSet<String>,
) -> Result<(), IngestError> {
    let known = store::list_member_conversation_ids(pool, account_id).await?;
    for conversation_id in known.difference(seen) {
        info!("no longer a member of {conversation_id}; keeping its messages");
        store::set_conversation_membership(pool, account_id, conversation_id, false).await?;
    }
    Ok(())
}
