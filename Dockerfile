# syntax=docker/dockerfile:1
FROM rust:1.97-slim-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY LICENSE-ENGINE ./
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Run as an unprivileged user. The runner deliberately has no command
# allowlist — `run_command` executes whatever the model asks for, and the
# container is the isolation boundary that makes that acceptable. A container
# running as root is not that boundary, so this is load-bearing, not hygiene.
RUN useradd --create-home --uid 10001 --shell /usr/sbin/nologin codemason

COPY --from=builder /build/target/release/codemason /usr/local/bin/codemason

# `git` refuses to operate on a repository owned by a different user unless
# the path is marked safe. Mounted target repositories will not be owned by
# uid 10001, and the runner shells out to `git` for every repository
# operation, so without this every containerised run fails preflight.
RUN git config --system --add safe.directory '*'

USER codemason
WORKDIR /repo
ENTRYPOINT ["/usr/local/bin/codemason"]
