//! The credential-isolation guard.
//!
//! The MCP process holds no Slack credential and must not be able to reach Slack, and in
//! Cargo that is a link-time fact: the binary's package does not depend on
//! `slack-client`, so the code that could use a token is not in the process at all.
//!
//! What is left to check is that the manifests still say so, and that the tools layer
//! still holds no SQL of its own. Both are cheap and both are the invariants
//! `AGENTS.md` names.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn manifest(path: &str) -> String {
    std::fs::read_to_string(repo_root().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// One manifest section, ending where the next one starts.
///
/// `[dependencies]` is what reaches a binary target; `[dev-dependencies]` does not, and
/// reading to the end of the file would conflate the two — the loop tests are a dev
/// dependency on `slack-ingest`, so that distinction is now load-bearing.
fn section(path: &str, name: &str) -> String {
    let manifest = manifest(path);
    let after = manifest
        .split_once(&format!("\n[{name}]"))
        .unwrap_or_else(|| panic!("{path} has no [{name}] section"))
        .1;
    after
        .split_once("\n[")
        .map_or(after, |(section, _)| section)
        .to_owned()
}

#[test]
fn the_mcp_binary_does_not_depend_on_the_slack_client() {
    // The root package is the one that carries `src/bin/slack-mcp.rs`.
    let dependencies = section("Cargo.toml", "dependencies");
    for forbidden in ["slack-client", "slack-ingest"] {
        assert!(
            !dependencies.contains(forbidden),
            "the MCP binary must not link {forbidden}:\n{dependencies}"
        );
    }
}

#[test]
fn neither_the_core_nor_the_mcp_crate_depends_on_the_slack_client() {
    // A transitive edge would put the code back in the binary just as surely.
    for path in ["crates/core/Cargo.toml", "crates/mcp/Cargo.toml"] {
        for forbidden in ["slack-client", "slack-ingest"] {
            assert!(
                !manifest(path).contains(forbidden),
                "{path} must not depend on {forbidden}"
            );
        }
    }
}

#[test]
fn the_ingester_is_its_own_package_with_its_own_binary() {
    // The reason it is: a second `[[bin]]` on the root package would have given the
    // package that carries `slack-mcp` a dependency on `slack-client`, and the test above
    // would be asserting a property the layout no longer had.
    let ingest = manifest("crates/ingest/Cargo.toml");
    assert!(
        ingest.contains("slack-client"),
        "the ingester is the one crate that *should* link the Slack client"
    );
    assert!(
        repo_root().join("crates/ingest/src/main.rs").is_file(),
        "the ingest binary must live in the ingest package"
    );
    let root = manifest("Cargo.toml");
    assert!(
        !root.contains("slack-ingest.rs"),
        "the root package must carry no ingest binary target"
    );
}

#[test]
fn the_tools_layer_holds_no_sql_of_its_own() {
    // `AGENTS.md`: all SQL lives in the store. `set-alias` is the only write in the
    // whole read surface, and the MCP role is granted nothing else.
    let source = std::fs::read_to_string(repo_root().join("crates/mcp/src/tools.rs"))
        .expect("tools.rs")
        .to_lowercase();
    for statement in ["insert into", "update ", "delete from", "select "] {
        assert!(
            !source.contains(statement),
            "raw {statement:?} in the tools layer"
        );
    }
}

#[test]
fn the_slack_crate_reaches_no_database() {
    // The Slack layer fetches and parses, and a store call there would let a fetch decide
    // what gets written. It depends on `slack-core` for the models, which is also where
    // `store` lives, so the manifest alone cannot say this — the source has to.
    let dependencies: String = manifest("crates/slack/Cargo.toml")
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect();
    assert!(
        !dependencies.contains("sqlx"),
        "the Slack client must not depend on sqlx"
    );
    for entry in std::fs::read_dir(repo_root().join("crates/slack/src")).expect("the slack crate") {
        let path = entry.expect("a source file").path();
        let source = std::fs::read_to_string(&path).expect("a source file");
        assert!(
            !source.contains("store::") && !source.contains("store}"),
            "{} reaches into the store",
            path.display()
        );
    }
}
