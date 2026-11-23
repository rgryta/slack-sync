//! Slack Web API access over a browser session credential.
//!
//! Nothing here touches `slack_core::store`: this crate fetches and parses, and the
//! decisions about what to keep belong to the ingester. `tests/archive/isolation.rs`
//! checks both halves of that — no SQL reaches in here, and no Slack code reaches the
//! MCP binary, which the linker enforces because its package does not depend on this
//! crate.

pub mod client;
pub mod errors;
#[cfg(test)]
mod fixtures;
pub mod parse;
pub mod ratelimit;

pub use client::{HistoryQuery, SlackClient, encode_cookie_value};
pub use errors::{AUTH_ERRORS, SlackError};
pub use ratelimit::TokenBucket;
