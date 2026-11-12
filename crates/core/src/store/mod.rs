//! Every SQL statement in the project.
//!
//! No other module writes a query. Functions are plain `async fn` over a `PgPool`
//! rather than methods on a type, so tests can call them directly without
//! constructing a service.
//!
//! Split by concern rather than left as one 1,400-line module, which is what the Python
//! original is. The write side of the schema lives beside the reads over the same
//! tables, except for the edit-tracking machinery, which has `revisions.rs` to itself:
//! losing an edit is a different kind of bug from losing a message.
//!
//! Write ordering is forced by the foreign keys: workspace -> account -> conversation ->
//! message -> reaction, with mention_thread after its conversation. Nothing here creates
//! a parent implicitly; a child written without its parent is an error rather than a stub
//! row appearing from nowhere.
//!
//! Two rules hold throughout. Queries are **runtime-checked** `sqlx::query*`, never
//! the `query_as!` macros: those need a live `DATABASE_URL` at build time or a
//! committed `.sqlx/` cache regenerated on every SQL edit, inside what has to stay a
//! hermetic Docker build — and `tests/store.rs` already runs every query against a
//! real Postgres. And **no caller value is ever interpolated into SQL**: where a
//! direction or a filter has to vary, there are two static query constants rather than
//! one formatted string.

mod accounts;
mod aliases;
mod conversations;
mod directory;
mod messages;
mod revisions;

pub use accounts::{
    get_sync_state, list_accounts, set_account_auth_error, set_sync_progress, set_sync_state,
    sync_status, upsert_account,
};
pub use aliases::{list_aliases, resolve_author, set_alias};
pub use conversations::{
    CursorUpdate, get_conversation, last_activity, list_conversations,
    list_member_conversation_ids, list_pollable_conversations, list_workspaces,
    next_backfill_conversation, resolve_workspace, set_conversation_cursors,
    set_conversation_membership, upsert_conversation, upsert_workspace,
};
pub use directory::{
    Directory, DirectoryConversation, DirectoryUser, DirectoryWorkspace, upsert_emoji, upsert_users,
};
pub use messages::{
    count_history, get_history, get_thread, list_mention_thread_keys, list_mentions,
    list_revisions, list_unhydrated_threads, messages_since, register_mention_thread,
    replace_reactions, resolve_message, search_messages, upsert_messages,
};
pub use revisions::{
    mark_absent, mark_present, record_revision, release_stranded, stored_texts, stored_thread,
    stored_window, text_differs,
};

use crate::models::UnknownVariant;

/// A column value the schema's CHECK constraint should have made impossible. Mapped
/// into `sqlx::Error` so the whole store speaks one error type.
fn decode_error(err: UnknownVariant) -> sqlx::Error {
    sqlx::Error::Decode(Box::new(err))
}

/// Parse a closed-set `text` column into its enum.
fn parse_column<T>(value: &str) -> Result<T, sqlx::Error>
where
    T: std::str::FromStr<Err = UnknownVariant>,
{
    value.parse::<T>().map_err(decode_error)
}

/// Parse a nullable closed-set `text` column.
fn parse_optional_column<T>(value: Option<&str>) -> Result<Option<T>, sqlx::Error>
where
    T: std::str::FromStr<Err = UnknownVariant>,
{
    value.map(parse_column).transpose()
}
