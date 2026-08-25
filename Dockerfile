# syntax=docker/dockerfile:1.7

FROM rust:1.88-slim-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked --package journal-mcp

FROM debian:bookworm-slim
LABEL io.modelcontextprotocol.server.name="io.github.ynishi/journal-mcp"
LABEL org.opencontainers.image.source="https://github.com/ynishi/journal-mcp"
LABEL org.opencontainers.image.licenses="MIT OR Apache-2.0"
LABEL org.opencontainers.image.description="Project canonical history MCP server — EventLog SoT (SQLite) + schema-driven chapters, stdio or streamable HTTP daemon"

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/journal-mcp /usr/local/bin/journal-mcp

# EventLog SQLite + schema dir live under the mounted volume; the server
# resolves them from JOURNAL_PROJECT_ROOT (see contrib/fly/fly.toml [env]).
WORKDIR /data
ENTRYPOINT ["journal-mcp"]
