//! Failure types the caller has to tell apart.
//!
//! The split is `Auth` against everything else, because the ingester reacts to a dead
//! session by stopping and recording it, while any other API error is just this call
//! failing. The Python classes carried the whole response payload as well; nothing ever
//! read it, so it is not kept here.

/// A session credential that Slack no longer honours. Retrying these burns requests
/// against a dead session and never succeeds; only a re-pasted token fixes them.
pub const AUTH_ERRORS: [&str; 5] = [
    "invalid_auth",
    "not_authed",
    "token_revoked",
    "account_inactive",
    "token_expired",
];

#[derive(Debug, thiserror::Error)]
pub enum SlackError {
    /// Slack answered `ok: false`.
    #[error("{method}: {code}")]
    Api { method: String, code: String },
    /// The session credential is no longer valid. Never retried.
    #[error("{method}: {code}")]
    Auth { method: String, code: String },
    /// The request never produced a usable answer, after every retry — or produced one
    /// that could not be read as the payload it claims to be.
    #[error("{0}")]
    Transport(String),
    /// Settings that cannot produce a working client: the `ValueError` the Python
    /// `TokenBucket` raises on a non-positive rate, in the crate's one error type.
    #[error("{0}")]
    Config(String),
}

impl SlackError {
    /// An `ok: false` answer, classified. One constructor, so no caller can forget
    /// that some error codes mean the session itself is gone.
    pub fn api(method: &str, code: &str) -> Self {
        if AUTH_ERRORS.contains(&code) {
            Self::Auth {
                method: method.to_owned(),
                code: code.to_owned(),
            }
        } else {
            Self::Api {
                method: method.to_owned(),
                code: code.to_owned(),
            }
        }
    }

    /// The Slack error code (`invalid_auth`, `channel_not_found`), for the callers that
    /// log or store it. `None` when Slack never answered.
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Api { code, .. } | Self::Auth { code, .. } => Some(code),
            Self::Transport(_) | Self::Config(_) => None,
        }
    }

    /// Whether the session credential itself is dead, rather than this one call.
    pub fn is_auth(&self) -> bool {
        matches!(self, Self::Auth { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_auth_code_classifies_itself_without_the_caller_remembering_to() {
        let dead = SlackError::api("auth.test", "token_revoked");
        assert!(dead.is_auth());
        assert_eq!(dead.code(), Some("token_revoked"));

        let ordinary = SlackError::api("conversations.info", "channel_not_found");
        assert!(!ordinary.is_auth());
        assert_eq!(ordinary.code(), Some("channel_not_found"));
    }
}
