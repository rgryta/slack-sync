//! One error type for the loops.
//!
//! The distinction that matters is the one the Python made with exception classes: an
//! auth failure means the session is dead and every remaining call is pointless, so it
//! propagates out of a pass; anything else is one conversation failing and is recorded
//! while the pass continues.

use slack_client::SlackError;

#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error(transparent)]
    Slack(#[from] SlackError),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

impl IngestError {
    /// Whether the session credential itself is dead, rather than this one call.
    pub fn is_auth(&self) -> bool {
        matches!(self, Self::Slack(error) if error.is_auth())
    }

    /// The Slack error code, for the `last_auth_error` column and the log line.
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Slack(error) => error.code(),
            Self::Db(_) => None,
        }
    }
}
