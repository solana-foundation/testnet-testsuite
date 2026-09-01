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
    doppler run --project testnet-testsuite --config dev -- pnpm bootstrap:tokens --apply {{ARGS}}

# Execute the complete Raydium CLMM lifecycle with Doppler-injected signers.
# Pass --amm-config plus any flow overrides after the recipe name.
raydium-clmm-flow-dev *ARGS:
    doppler run --project testnet-testsuite --config dev -- \
        cargo run -p raydium-clmm-orchestrator-service -- flow \
            --rpc-url http://localhost:8899 {{ARGS}}

# Create or validate the canonical AMM config using a named admin signer.
configure-raydium-clmm-dev-with ADMIN_KEYPAIR_VAR *ARGS:
    doppler run --project testnet-testsuite --config dev -- \
        cargo run -p raydium-clmm-orchestrator-service -- admin \
            --rpc-url http://localhost:8899 \
            --admin-keypair-env "{{ADMIN_KEYPAIR_VAR}}" \
            {{ARGS}}

# Use the funded program authority as the default local admin.
configure-raydium-clmm-dev *ARGS:
    just configure-raydium-clmm-dev-with "KEYPAIR_PROGRAM_AUTHORITY" {{ARGS}}

# --- program deployments (doppler-managed secrets) ----------------------------------

# Deploy a program using a Doppler-injected keypair.
# PROGRAM_KEYPAIR_VAR is the *name* of the Doppler secret, not its value.
deploy-program CONFIG PROGRAM_KEYPAIR_VAR SO_PATH URL="localhost":
    doppler run --project testnet-testsuite --config "{{CONFIG}}" -- \
        bash -c ' \
            set -euo pipefail; \
            : "${!1:?missing Doppler secret: $1}"; \
            : "${KEYPAIR_PROGRAM_AUTHORITY:?missing in config}"; \
            : "${KEYPAIR_FAUCET:?missing in config}"; \
            umask 077; \
            if [ -d /dev/shm ] && [ -w /dev/shm ]; then \
                d=$(mktemp -d /dev/shm/kp.XXXXXX); \
            else \
                d=$(mktemp -d); \
            fi; \
            trap "rm -rf $d" EXIT INT TERM; \
            printf %s "${!1}" > "$d/program.json"; \
            printf %s "$KEYPAIR_PROGRAM_AUTHORITY" > "$d/authority.json"; \
            printf %s "$KEYPAIR_FAUCET" > "$d/faucet.json"; \
            solana program deploy \
                --url "$3" \
                --program-id "$d/program.json" \
                --upgrade-authority "$d/authority.json" \
                --fee-payer "$d/faucet.json" \
                "$2" \
        ' _ "{{PROGRAM_KEYPAIR_VAR}}" "{{SO_PATH}}" "{{URL}}"

deploy-program-testnet PROGRAM_KEYPAIR_VAR SO_PATH: (deploy-program "prd" PROGRAM_KEYPAIR_VAR SO_PATH "testnet")
deploy-program-dev PROGRAM_KEYPAIR_VAR SO_PATH: (deploy-program "dev" PROGRAM_KEYPAIR_VAR SO_PATH "localhost")


# --- ops ------------------------------------------------------------------------

install-tools:
    ./scripts/install-tools.sh
