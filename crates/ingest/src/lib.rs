//! The ingester: Slack in, rows out.
//!
//! The MCP binary never links this crate — it holds no Slack credential and must not be
//! able to reach Slack even by accident. `tests/archive/isolation.rs` keeps the manifests
//! honest about it.
//!
//! The loops are generic over [`api::Slack`] rather than taking a `SlackClient`, so the
//! tests can drive them with a stand-in: what they must get right is cursors, ordering
//! and commit points, and the transport decides none of it.

pub mod api;
pub mod backfill;
pub mod discovery;
pub mod error;
pub mod filter;
pub mod pages;
pub mod poll;
pub mod runner;

pub use api::Slack;
pub use error::IngestError;
pub use filter::{Action, ScopeDecision, classify};
pub use pages::IngestContext;
pub use runner::run;
