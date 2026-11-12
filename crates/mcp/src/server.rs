//! The MCP server: streamable HTTP at `/mcp`, eleven tools, no Slack credential.
//!
//! The pool is opened before the listener binds and the shipped migrations are
//! asserted present — never applied. This process runs as a role with no DDL rights,
//! and a schema it cannot create is a schema it must not pretend to have.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::routing::get;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, ErrorData, Implementation, ServerCapabilities, ServerConfig,
};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use slack_core::config::McpSettings;
use slack_core::db::{DbError, assert_migrations, create_pool};
use sqlx::postgres::PgPool;

use crate::tools::{self, ToolError};

/// Byte-identical to the Python server's, hard wrap and trailing newline included: the
/// two are the same server to a client for as long as both are shipped, and a
/// description that drifts is a description one of them is wrong about.
pub const INSTRUCTIONS: &str = "\
A read-only view of a scoped slice of Slack, persisted in PostgreSQL by a separate
ingester. Only DMs, group DMs, explicitly named channels and threads the user was
mentioned in are stored; everything else was discarded at ingest and cannot be
retrieved here. Nothing in this server can send a Slack message or read anything
Slack has not already been asked for.
";

/// Tool names are hyphenated and declared explicitly: MCPHub joins them to the server
/// name with `-`, so these read `slack-history` rather than `slack-slack_history`,
/// which would stutter and mix both separators. The Rust method names below are
/// incidental.
#[derive(Clone)]
pub struct SlackMcp {
    pool: PgPool,
}

/// Turn a tool's answer into a protocol response.
///
/// A usage error is the caller's and goes back as `invalid_params` with its own text. A
/// SQL error is ours: it is logged in full and the caller is told only that the read
/// failed, because a Postgres error string can carry the DSN and the data.
fn finish(result: Result<String, ToolError>) -> Result<CallToolResult, ErrorData> {
    match result {
        Ok(body) => Ok(CallToolResult::success(vec![ContentBlock::text(body)])),
        Err(ToolError::Usage(message)) => Err(ErrorData::invalid_params(message, None)),
        Err(ToolError::Sql(error)) => {
            tracing::error!(%error, "the archive could not be read");
            Err(ErrorData::internal_error(
                "the archive could not be read".to_string(),
                None,
            ))
        }
    }
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ConversationsArgs {
    pub account: Option<String>,
    pub workspace: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HistoryArgs {
    pub conversation: String,
    pub account: Option<String>,
    pub after: Option<String>,
    pub before: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ThreadArgs {
    pub conversation: String,
    pub thread_ts: String,
    pub account: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RevisionsArgs {
    pub conversation: String,
    pub ts: String,
    pub account: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    pub query: String,
    pub account: Option<String>,
    pub workspace: Option<String>,
    pub conversation: Option<String>,
    pub author: Option<String>,
    pub after: Option<String>,
    pub before: Option<String>,
    pub has_reaction: Option<bool>,
    pub limit: Option<i64>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct MentionsArgs {
    pub account: Option<String>,
    pub workspace: Option<String>,
    pub after: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CatchUpArgs {
    pub since: String,
    pub account: Option<String>,
    pub workspace: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct NoArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetAliasArgs {
    pub alias: String,
    pub members: Vec<String>,
    pub note: Option<String>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct AliasesArgs {
    pub alias: Option<String>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct StatusArgs {
    pub account: Option<String>,
}

// Declared so a client can tell a read from a write without parsing a description. A
// tool with no hints is assumed able to change things, so an unannotated read looks no
// safer than a write — and ChatGPT blocked read calls before they left.
#[tool_router]
impl SlackMcp {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    #[tool(
        name = "conversations",
        description = "List the Slack conversations held in the local corpus.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn conversations(
        &self,
        Parameters(args): Parameters<ConversationsArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(
            tools::list_conversations(
                &self.pool,
                args.account.as_deref(),
                args.workspace.as_deref(),
                args.kind.as_deref(),
            )
            .await,
        )
    }

    #[tool(
        name = "history",
        description = "Read one conversation's stored messages, oldest first. With `after` you get \
                       the oldest `limit` messages after it, so you read a period forward from its \
                       start; without `after` you get the newest `limit`, and `before` walks back \
                       from there. `after`/`before` are ISO 8601 (a plain date is fine). When the \
                       period holds more than `limit`, the reply ends with how many more there are \
                       and the exact `after` (or `before`) to pass for the next page.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn history(
        &self,
        Parameters(args): Parameters<HistoryArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(
            tools::history(
                &self.pool,
                &args.conversation,
                args.account.as_deref(),
                args.after.as_deref(),
                args.before.as_deref(),
                args.limit,
            )
            .await,
        )
    }

    #[tool(
        name = "thread",
        description = "Read one whole thread, parent message included.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn thread(
        &self,
        Parameters(args): Parameters<ThreadArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(
            tools::thread(
                &self.pool,
                &args.conversation,
                &args.thread_ts,
                args.account.as_deref(),
            )
            .await,
        )
    }

    #[tool(
        name = "revisions",
        description = "The edit history of one message, oldest first, and the deleted text if it \
                       was deleted. Other tools mark a message (edited) or [deleted] and withhold \
                       the earlier and deleted text; this is where to read it. Revision 1 is the \
                       earliest text this archive saw, not necessarily what was originally sent. \
                       `conversation` is required because a Slack ts is only unique within one \
                       conversation.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn revisions(
        &self,
        Parameters(args): Parameters<RevisionsArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(
            tools::revisions(
                &self.pool,
                &args.conversation,
                &args.ts,
                args.account.as_deref(),
            )
            .await,
        )
    }

    #[tool(
        name = "search",
        description = "Full-text search the stored corpus. `author` accepts an alias or a raw \
                       Slack user ID, and `after`/`before` (ISO 8601, a plain date is fine) narrow \
                       it to a period.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(
            tools::search(
                &self.pool,
                &args.query,
                args.account.as_deref(),
                args.workspace.as_deref(),
                args.conversation.as_deref(),
                args.author.as_deref(),
                args.after.as_deref(),
                args.before.as_deref(),
                args.has_reaction,
                args.limit,
            )
            .await,
        )
    }

    #[tool(
        name = "mentions",
        description = "Messages that mention the account owner directly, newest first, across \
                       every stored conversation.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn mentions(
        &self,
        Parameters(args): Parameters<MentionsArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(
            tools::my_mentions(
                &self.pool,
                args.account.as_deref(),
                args.workspace.as_deref(),
                args.after.as_deref(),
                args.limit,
            )
            .await,
        )
    }

    #[tool(
        name = "catch-up",
        description = "Everything stored after a timestamp, grouped by conversation. This is not \
                       an unread count: no read state is tracked anywhere.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn catch_up(
        &self,
        Parameters(args): Parameters<CatchUpArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(
            tools::catch_up(
                &self.pool,
                &args.since,
                args.account.as_deref(),
                args.workspace.as_deref(),
                args.limit,
            )
            .await,
        )
    }

    #[tool(
        name = "workspaces",
        description = "Slack workspaces represented in the corpus.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn workspaces(
        &self,
        Parameters(_): Parameters<NoArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(tools::list_workspaces(&self.pool).await)
    }

    #[tool(
        name = "set-alias",
        description = "Name a set of Slack identities so they can be searched as one person. \
                       Members are written `account:user_id`; an empty list deletes the alias.",
        annotations(
            read_only_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn set_alias(
        &self,
        Parameters(args): Parameters<SetAliasArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(tools::set_alias(&self.pool, &args.alias, &args.members, args.note.as_deref()).await)
    }

    #[tool(
        name = "aliases",
        description = "Show the configured user aliases and their members.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn aliases(
        &self,
        Parameters(args): Parameters<AliasesArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(tools::list_aliases(&self.pool, args.alias.as_deref()).await)
    }

    #[tool(
        name = "status",
        description = "How fresh the corpus is, per ingesting account, and whether it is stuck.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn status(
        &self,
        Parameters(args): Parameters<StatusArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(tools::sync_status(&self.pool, args.account.as_deref()).await)
    }
}

#[tool_handler]
impl ServerHandler for SlackMcp {
    /// Written out rather than left to `#[tool_handler(instructions = "…")]`: that
    /// argument takes a literal, and a literal folded to fit this file would drop the
    /// hard wrap the Python server sends. This is what keeps the two byte-identical.
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("slack-sync", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS.to_string())
    }
}

/// Own the pool and refuse an unmigrated database.
///
/// Separate from [`run`] so a test can call it without a listener.
pub async fn open_pool(settings: &McpSettings) -> Result<PgPool, DbError> {
    let pool = create_pool(settings.dsn.expose()).await?;
    if let Err(error) = assert_migrations(&pool).await {
        // This process runs as a role with no DDL rights. A schema it cannot create is
        // one it must not pretend to have.
        pool.close().await;
        return Err(error);
    }
    Ok(pool)
}

/// `/mcp` plus `/healthz`.
///
/// `/healthz` answers only once the pool exists, because the pool is opened before the
/// listener binds: until then the probe gets a refused connection, which Docker reads
/// as unhealthy just the same. It is a plain route on this router and only `/mcp` is
/// nested onto the MCP service, so the `Host` allowlist below does not apply to it and
/// `--health-check` needs no entry of its own.
/// `tests/archive/server.rs::healthz_answers_a_host_the_mcp_endpoint_rejects` holds
/// that: if it ever stops being true the healthcheck starts failing for a reason
/// nothing about the container would explain.
pub fn build_router(pool: PgPool, allowed_hosts: Vec<String>) -> Router {
    let service = StreamableHttpService::new(
        move || Ok(SlackMcp::new(pool.clone())),
        Arc::new(NeverSessionManager::default()),
        stateless_config(allowed_hosts),
    );
    Router::new()
        .route("/healthz", get(async || "ok"))
        .nest_service("/mcp", service)
}

fn stateless_config(allowed_hosts: Vec<String>) -> StreamableHttpServerConfig {
    let mut config = StreamableHttpServerConfig::default();
    // There is no per-client state to keep, and statelessness takes session affinity
    // off the table behind MCPHub — what `stateless_http=True` bought the Python server.
    config.legacy_session_mode = false;
    // rmcp's guard against DNS rebinding. An empty list would mean "answer any `Host`",
    // so the list handed in here is never empty — see `DEFAULT_ALLOWED_HOSTS`. MCPHub
    // addresses this server by container name, so production sets `slack-mcp`.
    config.allowed_hosts = allowed_hosts;
    config
}

pub async fn run(settings: &McpSettings) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let pool = open_pool(settings).await?;
    let address: SocketAddr = format!("{}:{}", settings.host, settings.port).parse()?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    // Logged because the failure mode is a server that boots clean and then refuses
    // every tool call with a 403: one line here turns a baffling outage into an obvious
    // one. `/healthz` is outside this list, so a container can look healthy while every
    // call is rejected.
    tracing::info!(
        allowed_hosts = %settings.allowed_hosts.join(", "),
        "answering /mcp only for these Host headers"
    );
    tracing::info!(%address, "serving the slack archive over MCP");
    axum::serve(listener, build_router(pool, settings.allowed_hosts.clone()))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

/// Probe our own `/healthz`, for the container healthcheck.
///
/// `deploy/compose.yaml` currently shells out to `python -c "import urllib.request;
/// ..."`. A Rust image has neither Python nor, in a distroless or scratch base, a
/// shell — so at cutover that healthcheck silently becomes an unhealthy container.
/// This subcommand is what replaces it.
///
/// Hand-rolled over TCP rather than through an HTTP client crate: it reads one status
/// line from a loopback socket, and a dependency for that is a dependency to keep
/// patched for no gain.
pub async fn health_check(host: &str, port: u16) -> Result<(), String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    // The bind address may be the wildcard, which is not a destination.
    let host = match host {
        "0.0.0.0" | "::" | "" => "127.0.0.1",
        host => host,
    };
    let target = format!("{host}:{port}");
    let mut stream = tokio::net::TcpStream::connect(&target)
        .await
        .map_err(|error| format!("could not connect to {target}: {error}"))?;
    stream
        .write_all(
            format!("GET /healthz HTTP/1.1\r\nHost: {target}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .map_err(|error| format!("could not send the probe: {error}"))?;

    let mut status = String::new();
    BufReader::new(stream)
        .read_line(&mut status)
        .await
        .map_err(|error| format!("no answer from {target}: {error}"))?;
    if status.contains(" 200 ") {
        return Ok(());
    }
    Err(format!("/healthz answered {}", status.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_names() -> Vec<String> {
        SlackMcp::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect()
    }

    #[test]
    fn every_tool_is_registered() {
        let mut names = tool_names();
        names.sort();
        assert_eq!(
            names,
            vec![
                "aliases",
                "catch-up",
                "conversations",
                "history",
                "mentions",
                "revisions",
                "search",
                "set-alias",
                "status",
                "thread",
                "workspaces",
            ]
        );
    }

    #[test]
    fn names_are_hyphenated_not_mixed() {
        // MCPHub joins server and tool with `-`; an underscore would read as
        // `slack-slack_history`, mixing both separators in one identifier.
        for name in tool_names() {
            assert!(!name.contains('_'), "{name}");
        }
    }

    #[test]
    fn tools_describe_themselves() {
        // MCPHub shows these descriptions; an unlabelled tool is an unusable one.
        for tool in SlackMcp::tool_router().list_all() {
            assert!(tool.description.is_some(), "{}", tool.name);
        }
    }

    #[test]
    fn a_client_can_tell_which_tools_only_read() {
        // With no hints every tool looks able to write, so a read was no safer on its
        // face than `set-alias` beside it.
        for tool in SlackMcp::tool_router().list_all() {
            let annotations = tool
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("{} has no annotations", tool.name));
            assert_eq!(
                annotations.read_only_hint,
                Some(tool.name != "set-alias"),
                "{}",
                tool.name
            );
        }
    }

    #[tokio::test]
    async fn the_health_check_target_is_never_the_wildcard_address() {
        // Connecting to 0.0.0.0 is not a thing the healthcheck can do; port 1 is
        // closed, so the error names the address it actually tried.
        let error = health_check("0.0.0.0", 1).await.unwrap_err();
        assert!(error.contains("127.0.0.1:1"), "{error}");
    }
}
