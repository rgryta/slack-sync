# slack-sync

Slack ingester and read-only MCP server, backed by PostgreSQL.

Reads Slack with a browser session token (`xoxc` + `xoxd`), persists a scoped slice
of message history into Postgres, and serves it to an MCP client over MCP. No
Slack app, no bot token, no write path toward Slack.

## The two processes

One image, two commands, deliberately separated:

| Process | Command | Holds | Database role |
|---|---|---|---|
| Ingester | `slack-ingest` | the Slack session credential | `slack_ingest` (owns the schema) |
| MCP server | `slack-mcp` | nothing that can reach Slack | `slack_mcp` (reads; writes only aliases) |

Two Rust binaries from one workspace. `slack-mcp --health-check` is what the container
healthcheck runs, and it is the only thing that can: the image has no shell and no
interpreter.

One ingester container per Slack account. The MCP server is shared and serves every
account's corpus at once, labelling each row with its account and workspace.

The split is enforced three ways: separate Postgres roles, an `McpSettings` struct with
no token field, and the dependency graph — the package that builds `slack-mcp` does not
depend on `slack-client`, so the code that could use a Slack token is not linked into
that process at all. `tests/archive/isolation.rs` keeps the manifests honest about it.

## What is stored

Only four things, decided per message by `crates/ingest/src/filter.rs`:

1. DMs
2. group DMs
3. the channels named in `SLACK_SCOPE_CHANNELS`
4. threads containing a direct `<@me>` mention, whole thread included

Everything else is read and discarded — it is never written to the database.
Membership events (`channel_join` and friends) are dropped even inside a DM.

## Deploying

Agents build and push images. **Deployment is a human action in Dockge** — use
Update (which pulls) rather than Restart, or the new image is never fetched.

```bash
# 1. provision the database (once, as the postgres superuser)
psql -h <your-postgres-host> -U postgres \
  -v ingest_pw="<pw>" -v mcp_pw="<pw>" -f deploy/provision-slack-db.sql

# 2. build and push
docker build -f deploy/Dockerfile \
  --build-arg GIT_SHA=$(git rev-parse --short HEAD) \
  -t slack-sync:latest .
docker push <your-registry>/slack-sync:latest
```

3. Copy `.env.example` to `.env` beside the compose file and fill it in.
4. Deploy the stack in Dockge (`services/slack` in the homelab repo).
5. Register the server in MCPHub — a **manual, one-time admin action** through its
   UI, because writing into another service's database from a deploy script breaks
   silently on their next schema change:

   | Field | Value |
   |---|---|
   | name | `slack` |
   | type | `streamable-http` |
   | url | `http://slack-mcp:8000/mcp` |
   | headers | none — the server has no auth of its own |

   By container DNS name, not IP: that is how the live `waha` entry is written, and
   it survives an IP reallocation.

The MCP server has **no Traefik route**. It is reachable only inside
a private network; network isolation is the whole of its access control, so exposing
it would be a security change rather than a convenience.

## When the token dies

Session credentials rotate. The ingester records the failure on its account row and
keeps running — `slack-status` reports it and the stored corpus keeps serving,
it just stops advancing.

1. Re-extract `xoxc` and `xoxd` (the procedure is in `.env.example`).
2. Replace them in `.env`.
3. Restart only that ingester container.

Polling resumes from the stored cursor; nothing is re-read and nothing is lost.

## Limitations

- **`@here`, `@channel` and user-group pings do not bring a thread into scope.** Only
  a literal `<@Uxxxx>` mention does. A ping that reaches you through `@here` or a
  user group will be missed. `@channel` in a busy channel would drag most of that
  channel into scope, which is the opposite of the intent; user groups would need a
  separate `usergroups.users.list` sync to evaluate correctly.
- **Reaction counts are a snapshot, not a stream.** Only the last
  `SLACK_REACTION_WINDOW` days are re-read, and `conversation.reactions_fresh_since`
  records the boundary. Counts older than it are a floor, not a truth — every
  response that shows one says so.
- **No real-time push.** A session token cannot open Socket Mode, so new messages
  arrive one poll interval late at worst.
- **Token refresh is manual.** Nothing here logs into Slack or renews a session.
- **No attachments.** `files` and `blocks` are stripped before anything is stored.
- **Two accounts in one workspace keep two independent copies.** Ingest is
  account-scoped by design; the read tools collapse the duplicates and say they did.

## Development

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace   # the Postgres tests print a skip when docker is unreachable
cargo build --release --workspace --bins
```

The Postgres tests share one named, reusable `postgres:18-alpine` container and build
their own DSN, so nothing can be pointed at a real database; remove it with
`docker rm -f slack-sync-test-pg`.

## MCP tools

`conversations`, `history`, `thread`, `revisions`, `search`, `mentions`, `catch-up`,
`workspaces`, `set-alias`, `aliases`, `status`.

Names are hyphenated and declared explicitly, because MCPHub joins the server name to
the tool name with `-`: registered as `slack`, these read `slack-history`. **The names
are a contract** — MCPHub groups pin an explicit tool allow-list matched by name, so a
stale name matches nothing rather than failing loudly. `crates/mcp`'s unit tests assert
the exact set.

Register in MCPHub as `slack` / `streamable-http` / `http://slack-mcp:8000/mcp`, no
headers — the server has no auth of its own and network isolation is its access
control.

