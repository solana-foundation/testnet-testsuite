# Multi-stage build for the mtm Rust workspace.
# cargo-chef caches the (large, solana-heavy) dependency layer so source-only
# rebuilds take seconds instead of minutes.

FROM rust:1.97-slim-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
# add `-p arb -p mm` when the bots rejoin the workspace
RUN cargo build --release -p oracle

# Distroless works because TLS roots are compiled into the binaries
# (webpki-roots on both reqwest and tungstenite) — no OS cert store needed.
FROM gcr.io/distroless/cc-debian12 AS runtime
WORKDIR /app
COPY --from=builder /app/target/release/oracle /usr/local/bin/oracle
# baked-in config as fallback; compose mounts ./config over it for live edits
COPY config /app/config
ENV MTM_CONFIG_DIR=/app/config
ENTRYPOINT ["/usr/local/bin/oracle"]
