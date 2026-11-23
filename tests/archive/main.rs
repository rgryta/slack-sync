//! One test binary, so one testcontainer is started for the whole suite.
//!
//! Rust has no session-scoped fixture, and a separate `tests/*.rs` file would mean a
//! separate process and a separate Postgres container each. The modules below share
//! one, taking turns through a mutex because they share its database.
//!
//! Several modules say they were ported from a `tests/test_*.py`. Those files were the
//! Python suite this archive replaced; they are in git history up to the commit that
//! removed the Python, and nowhere in the tree.

mod fake_slack;
mod isolation;
mod loops;
mod revisions;
mod server;
mod store;
mod support;
mod tools;
mod writes;
