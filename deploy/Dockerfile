# Built from the repo root:
#   docker build -f deploy/Dockerfile -t slack-sync:latest .
#
# One image, two binaries: the services differ only by `command`. There is no interpreter
# left in it — both processes are Rust, so the runtime stage carries nothing but glibc and
# the two binaries.

# Trixie to match the runtime below: a binary built against a newer glibc than the
# image it runs in will not start. This one needs no more than GLIBC_2.34 and trixie
# ships 2.41, so there is margin either way.
#
# 1.99 rather than the manifest's `rust-version = "1.85"`: sqlx 0.9 requires rustc
# 1.94, so that is the real floor. A too-old toolchain fails here with the required
# version named, which is a better error than anything a comment could say.
FROM rust:1.99-slim-trixie@sha256:f6c4a35d21dbb3c89d7a8d056c193e94fc09037f2b58dd858b09c39a47dd02d7 AS rust
WORKDIR /build
# No dependency pre-build stage. The usual trick needs stub sources for every workspace
# member, kept in step with the members by hand, to save about two minutes on an image
# that is built occasionally and by hand. `db-migrations/` is here because
# `crates/core/src/db.rs` embeds the migrations with `include_str!`, so the build needs
# them even though the binary then carries its own copy.
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
COPY src/ ./src/
COPY db-migrations/ ./db-migrations/
# `tests/` is excluded by .dockerignore and is not needed: the declared `[[test]]`
# target's absence does not stop the manifest parsing, and `--bins` does not build it.
# Both binaries in one `cargo build`, so the shared dependencies compile once.
# `--workspace --bins` rather than two `--bin` flags: `--bin` resolves inside the
# default-run package, so naming `slack-ingest` that way fails with "no bin target named
# slack-ingest in default-run packages". The workspace has exactly these two binaries,
# and `.github/workflows/ci.yml` runs this same line so the two cannot drift.
RUN cargo build --release --workspace --bins

# glibc, no shell, no package manager, ~28 MB. The alternatives and why not:
#
#   debian:trixie-slim         ~80 MB for a shell this workflow never uses — the
#                              interfaces are the logs and the MCP tools, not `docker exec`.
#   distroless/base-debian13   ~8 MB smaller, but a Rust binary normally carries a
#                              DT_NEEDED on libgcc_s.so.1 and `base` has no libgcc.
#                              `panic = "abort"` drops the unwinding tables, not
#                              necessarily that entry. Check with `ldd` before trying it.
#   static musl on scratch     ~13 MB total, and it swaps the allocator under a port whose
#                              entire justification is a resident-memory number: musl's
#                              malloc fragments differently, so the measured 7.18 MiB would
#                              have to be re-earned. It also drops zoneinfo, which `TZ`
#                              needs. Not worth 15 MB that is pulled once.
#
# The `nonroot` tag runs as uid 65532 — there is no `useradd` here to make one, and none
# is needed. **Pin this by digest on the first real build**, as every other base here is;
# it is a tag only because the machine that wrote this could not pull it.
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:cc66d222f0a7ec996b78a99fe76a969bc80b539a5016ad866c14d1949bc6d0ab
# Root-owned and merely executable by `nonroot`, so neither service can rewrite its own
# binary. `/usr/local/bin` is on PATH ahead of nothing that shadows it. No
# `db-migrations/` here on purpose: the binaries embed the migrations, and a scratch image
# has no shell to read them with anyway.
COPY --from=rust /build/target/release/slack-mcp /usr/local/bin/slack-mcp
COPY --from=rust /build/target/release/slack-ingest /usr/local/bin/slack-ingest
USER nonroot:nonroot
EXPOSE 8000
# There is no shell and no interpreter in this image, so `slack-mcp --health-check` is the
# only thing that can probe the server — which is why that subcommand exists.
# `deploy/compose.yaml` already runs it. For a one-off look inside, rebuild against the
# `:debug` tag, which adds busybox; do not add a shell here for it.
#
# GIT_SHA LAST, on purpose: above the COPYs it would enter their layer cache key and every
# commit would rebuild them.
ARG GIT_SHA=unknown
ENV SLACK_SYNC_GIT_SHA=$GIT_SHA
# The ingester service overrides this with `slack-ingest`.
CMD ["slack-mcp"]
