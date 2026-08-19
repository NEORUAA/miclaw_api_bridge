# syntax=docker/dockerfile:1

FROM node:22-bookworm-slim AS frontend
WORKDIR /app
RUN corepack enable && corepack prepare pnpm@10 --activate
COPY package.json pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile
COPY index.html tsconfig.json tsconfig.node.json vite.config.ts ./
COPY src ./src
COPY src-tauri/icons ./src-tauri/icons
RUN pnpm build

FROM rust:1-bookworm AS builder
WORKDIR /app
RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates libdbus-1-dev pkg-config \
  && rm -rf /var/lib/apt/lists/*
COPY src-tauri/Cargo.toml ./src-tauri/Cargo.toml
COPY src-tauri/Cargo.lock ./src-tauri/Cargo.lock
COPY src-tauri/src ./src-tauri/src
COPY --from=frontend /app/dist ./dist
WORKDIR /app/src-tauri
RUN cargo build --release --bin miclaw_api_bridge

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates libdbus-1-3 \
  && rm -rf /var/lib/apt/lists/*

ENV MICLAW_API_BRIDGE_DISABLE_KEYRING=1 \
  RUST_LOG=miclaw_api_bridge_lib=info \
  XDG_CONFIG_HOME=/data/config \
  XDG_DATA_HOME=/data/data \
  MALLOC_ARENA_MAX=2

WORKDIR /app
COPY --from=builder /app/src-tauri/target/release/miclaw_api_bridge /usr/local/bin/miclaw_api_bridge

VOLUME ["/data"]
EXPOSE 8765
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD miclaw_api_bridge status --base-url http://127.0.0.1:8765 >/dev/null || exit 1

CMD ["miclaw_api_bridge", "server", "--host", "0.0.0.0"]
