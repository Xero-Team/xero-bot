# ---- build stage ----
# pinned to bookworm to match the runtime stage's glibc (the floating
# `rust:1-slim` tag now tracks Debian trixie / glibc 2.39, which fails to
# run on bookworm with "GLIBC_2.39 not found")
FROM rust:1-slim-bookworm AS builder
WORKDIR /build
# cache deps
COPY Cargo.toml Cargo.lock ./
COPY vendor ./vendor
COPY src ./src
COPY api ./api
RUN cargo build --release --bin server

# ---- runtime stage ----
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates git curl nodejs npm \
    && rm -rf /var/lib/apt/lists/*
# subprocess review engines: pi + codex CLIs (both degrade gracefully —
# engine selection just falls through to agent/builtin if an install fails)
RUN npm install -g @mariozechner/pi-coding-agent @openai/codex \
    || echo "WARN: engine CLI install failed; auto engine falls back to agent/builtin"

COPY --from=builder /build/target/release/server /usr/local/bin/server

ENV XERO_DATA_DIR=/data
VOLUME /data
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/server"]
