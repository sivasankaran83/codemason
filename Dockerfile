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
COPY --from=builder /build/target/release/codemason /usr/local/bin/codemason
ENTRYPOINT ["/usr/local/bin/codemason"]
