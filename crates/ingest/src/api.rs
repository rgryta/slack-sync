//! The Slack surface the loops use, as a trait.
//!
//! The loops must be testable against a stand-in: what they have to get right is
//! cursors, ordering and commit points, and the transport decides none of it. So the
//! surface is written down here, `SlackClient` implements it, and
//! `tests/archive/fake_slack.rs` is the other implementor.
//!
//! The trait is generic rather than `dyn`, so there is no boxing and no indirect call
//! per request.

use slack_client::{HistoryQuery, SlackClient, SlackError};
use slack_core::models::{Conversation, Emoji, Message, Workspace, WorkspaceUser};

/// What the ingester asks of Slack. Exactly `SlackClient`'s public surface.
///
/// `async fn` rather than `-> impl Future + Send`: the two loops run as two futures in
/// one task (`tokio::join!` in `runner::run_with_pool`), so nothing here is ever spawned
/// and a `Send` bound would be forty lines of desugaring to permit something the design
/// deliberately does not do. If a loop is ever spawned, this is the line to revisit.
#[allow(async_fn_in_trait)]
pub trait Slack {
    /// `(team_id, user_id)` for the credential in hand.
    async fn auth_test(&self) -> Result<(String, String), SlackError>;

    async fn team_info(&self, team: Option<&str>) -> Result<Workspace, SlackError>;

    async fn users_conversations(
        &self,
        account_id: &str,
        default_team_id: Option<&str>,
    ) -> Result<Vec<Conversation>, SlackError>;

    async fn conversations_info(
        &self,
        channel: &str,
        account_id: &str,
        default_team_id: Option<&str>,
    ) -> Result<Conversation, SlackError>;

    async fn conversations_history(
        &self,
        channel: &str,
        account_id: &str,
        query: HistoryQuery<'_>,
    ) -> Result<(Vec<Message>, Option<String>), SlackError>;

    async fn conversations_replies(
        &self,
        channel: &str,
        thread_ts: &str,
        account_id: &str,
        reserve: f64,
    ) -> Result<Vec<Message>, SlackError>;

    async fn users_list(&self, account_id: &str) -> Result<Vec<WorkspaceUser>, SlackError>;

    async fn emoji_list(&self, team_id: &str) -> Result<Vec<Emoji>, SlackError>;
}

impl Slack for SlackClient {
    async fn auth_test(&self) -> Result<(String, String), SlackError> {
        SlackClient::auth_test(self).await
    }

    async fn team_info(&self, team: Option<&str>) -> Result<Workspace, SlackError> {
        SlackClient::team_info(self, team).await
    }

    async fn users_conversations(
        &self,
        account_id: &str,
        default_team_id: Option<&str>,
    ) -> Result<Vec<Conversation>, SlackError> {
        SlackClient::users_conversations(self, account_id, default_team_id).await
    }

    async fn conversations_info(
        &self,
        channel: &str,
        account_id: &str,
        default_team_id: Option<&str>,
    ) -> Result<Conversation, SlackError> {
        SlackClient::conversations_info(self, channel, account_id, default_team_id).await
    }

    async fn conversations_history(
        &self,
        channel: &str,
        account_id: &str,
        query: HistoryQuery<'_>,
    ) -> Result<(Vec<Message>, Option<String>), SlackError> {
        SlackClient::conversations_history(self, channel, account_id, query).await
    }

    async fn conversations_replies(
        &self,
        channel: &str,
        thread_ts: &str,
        account_id: &str,
        reserve: f64,
    ) -> Result<Vec<Message>, SlackError> {
        SlackClient::conversations_replies(self, channel, thread_ts, account_id, reserve).await
    }

    async fn users_list(&self, account_id: &str) -> Result<Vec<WorkspaceUser>, SlackError> {
        SlackClient::users_list(self, account_id).await
    }

    async fn emoji_list(&self, team_id: &str) -> Result<Vec<Emoji>, SlackError> {
        SlackClient::emoji_list(self, team_id).await
    }
}
