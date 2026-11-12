# Working in this repo

## Gates — all clean before every commit

```bash
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release --workspace --bins
```

Postgres tests run against a throwaway testcontainer and print a skip when Docker is
unreachable. **Never point tests at a real database.**

```
crates/core      models, db, config, store (every SQL statement)
crates/slack     the Slack client: fetch and parse, no database in reach
crates/ingest    the loops and `slack-ingest`; the only crate that links crates/slack
crates/mcp       the read surface
src/bin/         `slack-mcp`, in the root package, which must never gain a
                 `slack-client` or `slack-ingest` dependency
```

The Rust tests share one named, reusable `postgres:18-alpine` container
(`slack-sync-test-pg`) and build their own DSN, as `conftest.py` does. Never set
`DATABASE_URL`: the queries are runtime-checked on purpose, so the build is hermetic
and nothing can point at a real database. Remove the container with
`docker rm -f slack-sync-test-pg`.

## Commits

- Two lines maximum. No co-author trailers. No emojis.
- Subject line says what changed; the second line says why, if it is not obvious.

## Deployment

**Agents do not deploy.** Agents build and push images; a human deploys in Dockge.
When telling the user to deploy, say **Update** (which pulls the new image), never
Restart — Restart reuses the image already on disk and the change silently does not
land.

Releases are built on GitHub: bump `version` under `[workspace.package]`, merge to main, and
`tag.yml` tags it and starts `image.yml`, which publishes `ghcr.io/rgryta/slack-sync:X.Y.Z`
and `:latest` and cuts a release with a changelog. `ci.yml` (fmt, clippy, tests, release
build) runs on every pull request.

The same gate runs on the NAS when Actions is unavailable — see the Gates section.

**The Dockge stack directory is not a git checkout and must not be treated as one.** Never
copy `deploy/compose.yaml` into it: that file is an example for other people, and the live
stack is maintained by hand. The repo's job ends at the image.

## Secrets

- `.env` files committed to this repo carry variable **names with empty values**.
  Never copy a real secret into the repo.
- Credentials are typed `slack_core::config::Secret`, whose `Debug` prints a redaction, so
  a stray log line cannot print them. Keep it that way.

## Invariants worth not breaking

- The root Cargo package must not depend on `slack-client` or `slack-ingest`: that absent
  edge is what makes the MCP process unable to reach Slack.
- All SQL lives in `crates/core/src/store/`.
- `crates/ingest/src/filter.rs` is the only place that decides what is stored.
