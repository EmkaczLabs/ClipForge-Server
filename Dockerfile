# syntax=docker/dockerfile:1

# ==============================================================================
# Stage 1: Builder
# ==============================================================================
FROM rust:1-slim-bookworm AS builder

WORKDIR /app

# C toolchain needed for bundled SQLite (rusqlite) and ring
RUN apt-get update \
    && apt-get install -y --no-install-recommends gcc libc6-dev \
    && rm -rf /var/lib/apt/lists/*

# Ensure optional runtime asset directories exist even if empty
RUN mkdir -p /app/web /app/migrations

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY . .

RUN cargo build --release --locked --bin clipforge-server

# ==============================================================================
# Stage 2: Minimal Non-Root Runtime
# ==============================================================================
FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user and persistent /data directory
RUN groupadd --gid 10001 clipforge \
    && useradd --uid 10001 --gid 10001 --home-dir /data --shell /usr/sbin/nologin clipforge \
    && mkdir -p /data/clips /app/web /app/migrations \
    && chown -R clipforge:clipforge /data /app

WORKDIR /app

COPY --from=builder --chown=clipforge:clipforge /app/target/release/clipforge-server /usr/local/bin/clipforge-server
COPY --from=builder --chown=clipforge:clipforge /app/web /app/web
COPY --from=builder --chown=clipforge:clipforge /app/migrations /app/migrations

ENV CLIPFORGE_BIND_ADDR=0.0.0.0:3000 \
    CLIPFORGE_DATABASE_PATH=/data/clipforge-server.db \
    CLIPFORGE_STORAGE_BACKEND=local \
    CLIPFORGE_STORAGE_PATH=/data/clips \
    RUST_LOG=clipforge_server=info,tower_http=info

VOLUME ["/data"]
EXPOSE 3000

USER clipforge
STOPSIGNAL SIGTERM

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["curl", "-fsS", "http://127.0.0.1:3000/ready"]

ENTRYPOINT ["/usr/local/bin/clipforge-server"]
CMD ["serve"]
