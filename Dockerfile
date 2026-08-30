# ---- build stage ----
FROM rust:1-slim AS builder
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
# optional: pi coding agent for the subprocess review engine
RUN npm install -g @mariozechner/pi-coding-agent || echo "pi install failed (engine disabled)"

COPY --from=builder /build/target/release/server /usr/local/bin/server

ENV XERO_DATA_DIR=/data
VOLUME /data
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/server"]
