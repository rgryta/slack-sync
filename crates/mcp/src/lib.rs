//! The read surface.
//!
//! This crate must never depend on `slack-client`: the MCP process holds no Slack
//! credential and must be unable to call Slack even by accident. In Python that needed
//! a test that spawns a fresh interpreter and inspects `sys.modules`; here the absence
//! of the dependency edge is what the linker enforces, and `tests/isolation.rs` checks
//! the manifests still say so.

pub mod render;
pub mod server;
pub mod tools;
