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
RUN rustup target add x86_64-unknown-linux-musl \
    && apt-get update -qq \
    && apt-get install -y -qq musl-tools \
    && rm -rf /var/lib/apt/lists/*
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
RUN cargo build --release --workspace --bins --target x86_64-unknown-linux-musl

# Nothing: 14.7 MB, of which 14.0 MB is the two binaries. No libc, no shell, no package
# manager, no certificate store. Each of those absences was checked, not assumed:
#
#   * TLS trust is bundled. `reqwest`, `sqlx-core` and `hyper-rustls` use `webpki-roots`;
#     `rustls-native-certs` is in Cargo.lock only through `bollard`, testcontainers'
#     Docker client, which no release binary links. `ci.yml` fails the build if that ever
#     stops being true, because the symptom here is every Slack call failing at runtime
#     with nothing earlier to notice.
#   * No timezone database, and `TZ` does not need one: every rendered time is UTC and
#     nothing uses `chrono::Local`. Set `DISPLAY_TZ` to anything else and this base has
#     to change.
#   * Nothing writes to a temporary directory or reads /etc. DNS resolves from the
#     /etc/resolv.conf Docker mounts in, which musl reads directly.
#
# An earlier revision of this file argued against musl on the grounds that it "swaps the
# allocator under a port whose entire justification is a resident-memory number". Fair
# concern, so it was measured rather than argued: serving an identical 3.2 MB of rendered
# output, glibc on distroless/cc settled at 6.30 MiB resident and musl here at 5.59 MiB.
# musl idles ~0.7 MiB higher and ends ~0.7 MiB lower, because glibc's malloc grows arenas
# and keeps them. The number was not lost.
#
# `gcr.io/distroless/static-debian13:nonroot` is the step up if any of the above changes:
# a certificate store, tzdata and an /etc/passwd for 2.4 MB.
FROM scratch
# Root-owned and merely executable by `nonroot`, so neither service can rewrite its own
# binary. `/usr/local/bin` is on PATH ahead of nothing that shadows it. No
# `db-migrations/` here on purpose: the binaries embed the migrations, and a scratch image
# has no shell to read them with anyway.
COPY --from=rust /build/target/x86_64-unknown-linux-musl/release/slack-mcp /usr/local/bin/slack-mcp
COPY --from=rust /build/target/x86_64-unknown-linux-musl/release/slack-ingest /usr/local/bin/slack-ingest
# Numeric, because there is no /etc/passwd here for a name to resolve against. 65532 is
# the uid distroless calls `nonroot`, kept the same so the two bases are interchangeable.
USER 65532:65532
EXPOSE 8000
# There is no shell and no interpreter in this image, so `slack-mcp --health-check` is the
# only thing that can probe the server — which is why that subcommand exists.
# `deploy/compose.yaml` already runs it. For a one-off look inside, copy the binaries into
# a `distroless/static:debug` stage locally; do not add a shell here for it.
#
# GIT_SHA LAST, on purpose: above the COPYs it would enter their layer cache key and every
# commit would rebuild them.
ARG GIT_SHA=unknown
ENV SLACK_SYNC_GIT_SHA=$GIT_SHA
# The ingester service overrides this with `slack-ingest`.
CMD ["slack-mcp"]
