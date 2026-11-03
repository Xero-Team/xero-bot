FROM rust:1.99-bookworm

# actions/checkout is a Node action. Keep the compiler and every CI component
# already present so each job starts without a toolchain installation step.
COPY --from=node:20-bookworm /usr/local/bin/node /usr/local/bin/node
RUN rustup component add clippy rustfmt
