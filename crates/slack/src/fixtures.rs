//! Recorded Slack payloads, for the parser and client tests.
//!
//! Real answers from `conversations.history`, `users.conversations`, `users.list`,
//! `emoji.list` and the two error shapes, kept as files rather than inline so a test
//! reads as an assertion rather than as a wall of JSON. They sit above the crate because
//! the integration tests under `tests/` read them too.

use serde_json::Value;

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/slack");

pub fn load(name: &str) -> Value {
    let path = format!("{DIR}/{name}.json");
    let body = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("{path}: {e}"))
}
