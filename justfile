# bash rather than zsh so recipes work in containers/CI as well as dev machines
set shell := ["bash", "-cu"]
set dotenv-load := true

default:
    @just --list

check:
    cargo check --workspace --all-targets

build:
    cargo build --workspace

fmt:
    cargo fmt --all
    pnpm exec prettier --write .

fmt-check:
    cargo fmt --all --check
    pnpm exec prettier --check .

lint:
    cargo clippy --workspace --all-targets -- -D warnings
    pnpm run check

test:
    @if command -v cargo-nextest >/dev/null 2>&1; then cargo nextest run --workspace; else cargo test --workspace; fi

deny:
    cargo deny check

ci: fmt-check lint test

# --- run services (profile = local | testnet) ----------------------------------

oracle profile="local":
    MTM_PROFILE={{profile}} cargo run -p oracle

# --- token bootstrap (doppler-managed secrets) ----------------------------------

bootstrap-tokens *ARGS:
    doppler run --project testnet-testsuite --config prd -- pnpm bootstrap:tokens {{ARGS}}

bootstrap-tokens-dev *ARGS:
    doppler run --project testnet-testsuite --config dev -- pnpm bootstrap:tokens:local {{ARGS}}

# --- ops ------------------------------------------------------------------------

install-tools:
    ./scripts/install-tools.sh
