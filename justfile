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

# Create every unordered pair of mints supplied with repeated --mint flags.
raydium-clmm-create-pools *ARGS:
    cargo run -p raydium-clmm-orchestrator-service -- create-pools \
        --mint $(echo $PUBKEY_TOKEN_JUP_MINT) \
        --mint $(echo $PUBKEY_TOKEN_RAY_MINT) \
        --mint $(echo $PUBKEY_TOKEN_USDC_MINT) \
        {{ARGS}}

raydium-clmm-create-pools-dev *ARGS:
    doppler run --project testnet-testsuite --config dev -- \
        just raydium-clmm-create-pools --rpc-url http://localhost:8899 {{ARGS}}

raydium-clmm-create-pools-testnet *ARGS:
    doppler run --project testnet-testsuite --config prd -- \
        just raydium-clmm-create-pools --rpc-url https://api.testnet.solana.com {{ARGS}}

# Exercise every existing unordered pair supplied with --mint using a new faucet-funded user.
# Pass --amm-config plus any user-flow overrides.
raydium-clmm-user-flow *ARGS:
    cargo run -p raydium-clmm-orchestrator-service -- user-flow \
        --mint $(echo $PUBKEY_TOKEN_JUP_MINT) \
        --mint $(echo $PUBKEY_TOKEN_RAY_MINT) \
        --mint $(echo $PUBKEY_TOKEN_USDC_MINT) \
        {{ARGS}}

raydium-clmm-user-flow-dev *ARGS:
    doppler run --project testnet-testsuite --config dev -- \
        just raydium-clmm-user-flow --rpc-url http://localhost:8899 {{ARGS}}

raydium-clmm-user-flow-testnet *ARGS:
    doppler run --project testnet-testsuite --config prd -- \
        just raydium-clmm-user-flow --rpc-url https://api.testnet.solana.com {{ARGS}}

# Create or validate the canonical AMM config using a named admin signer.
configure-raydium-clmm-with CONFIG ADMIN_KEYPAIR_VAR RPC_URL *ARGS:
    doppler run --project testnet-testsuite --config "{{CONFIG}}" -- \
        cargo run -p raydium-clmm-orchestrator-service -- admin \
            --rpc-url "{{RPC_URL}}" \
            --admin-keypair-env "{{ADMIN_KEYPAIR_VAR}}" \
            {{ARGS}}

# Use the funded program authority as the default local admin.
configure-raydium-clmm-dev *ARGS:
    just configure-raydium-clmm-with dev "KEYPAIR_PROGRAM_AUTHORITY" http://localhost:8899 {{ARGS}}

# Use the funded program authority as the default testnet admin.
configure-raydium-clmm-prd *ARGS:
    just configure-raydium-clmm-with prd "KEYPAIR_PROGRAM_AUTHORITY" https://api.testnet.solana.com {{ARGS}}

# --- orca whirlpool (doppler-managed secrets) ---------------------------------------

# Create and seed every unordered pair of mints supplied with repeated --mint flags.
orca-whirlpool-create-pools *ARGS:
    cargo run -p orca-whirlpool-orchestrator-service -- create-pools \
        --mint $(echo $PUBKEY_TOKEN_JUP_MINT) \
        --mint $(echo $PUBKEY_TOKEN_RAY_MINT) \
        --mint $(echo $PUBKEY_TOKEN_USDC_MINT) \
        {{ARGS}}

orca-whirlpool-create-pools-dev *ARGS:
    doppler run --project testnet-testsuite --config dev -- \
        just orca-whirlpool-create-pools --rpc-url http://localhost:8899 {{ARGS}}

orca-whirlpool-create-pools-testnet *ARGS:
    doppler run --project testnet-testsuite --config prd -- \
        just orca-whirlpool-create-pools --rpc-url https://api.testnet.solana.com {{ARGS}}

# Exercise every existing unordered pair supplied with --mint using a new faucet-funded user.
orca-whirlpool-user-flow *ARGS:
    cargo run -p orca-whirlpool-orchestrator-service -- user-flow \
        --mint $(echo $PUBKEY_TOKEN_JUP_MINT) \
        --mint $(echo $PUBKEY_TOKEN_RAY_MINT) \
        --mint $(echo $PUBKEY_TOKEN_USDC_MINT) \
        {{ARGS}}

orca-whirlpool-user-flow-dev *ARGS:
    doppler run --project testnet-testsuite --config dev -- \
        just orca-whirlpool-user-flow --rpc-url http://localhost:8899 {{ARGS}}

orca-whirlpool-user-flow-testnet *ARGS:
    doppler run --project testnet-testsuite --config prd -- \
        just orca-whirlpool-user-flow --rpc-url https://api.testnet.solana.com {{ARGS}}

# Create or validate the WhirlpoolsConfig + fee tier using a named admin signer.
configure-orca-whirlpool-with CONFIG ADMIN_KEYPAIR_VAR RPC_URL *ARGS:
    doppler run --project testnet-testsuite --config "{{CONFIG}}" -- \
        cargo run -p orca-whirlpool-orchestrator-service -- admin \
            --rpc-url "{{RPC_URL}}" \
            --admin-keypair-env "{{ADMIN_KEYPAIR_VAR}}" \
            {{ARGS}}

configure-orca-whirlpool-dev *ARGS:
    just configure-orca-whirlpool-with dev "KEYPAIR_PROGRAM_AUTHORITY" http://localhost:8899 {{ARGS}}

configure-orca-whirlpool-prd *ARGS:
    just configure-orca-whirlpool-with prd "KEYPAIR_PROGRAM_AUTHORITY" https://api.testnet.solana.com {{ARGS}}

# --- manifest (doppler-managed secrets) ---------------------------------------------

# Ensure a market exists for every unordered pair of mints supplied with repeated --mint flags.
manifest-create-markets *ARGS:
    cargo run -p manifest-dex-orchestrator-service -- create-markets \
        --mint $(echo $PUBKEY_TOKEN_JUP_MINT) \
        --mint $(echo $PUBKEY_TOKEN_RAY_MINT) \
        --mint $(echo $PUBKEY_TOKEN_USDC_MINT) \
        {{ARGS}}

manifest-create-markets-dev *ARGS:
    doppler run --project testnet-testsuite --config dev -- \
        just manifest-create-markets --rpc-url http://localhost:8899 {{ARGS}}

manifest-create-markets-testnet *ARGS:
    doppler run --project testnet-testsuite --config prd -- \
        just manifest-create-markets --rpc-url https://api.testnet.solana.com {{ARGS}}

# Make and take on every pair's existing market using a new faucet-funded user.
manifest-user-flow *ARGS:
    cargo run -p manifest-dex-orchestrator-service -- user-flow \
        --mint $(echo $PUBKEY_TOKEN_JUP_MINT) \
        --mint $(echo $PUBKEY_TOKEN_RAY_MINT) \
        --mint $(echo $PUBKEY_TOKEN_USDC_MINT) \
        {{ARGS}}

manifest-user-flow-dev *ARGS:
    doppler run --project testnet-testsuite --config dev -- \
        just manifest-user-flow --rpc-url http://localhost:8899 {{ARGS}}

manifest-user-flow-testnet *ARGS:
    doppler run --project testnet-testsuite --config prd -- \
        just manifest-user-flow --rpc-url https://api.testnet.solana.com {{ARGS}}

# --- program deployments (doppler-managed secrets) ----------------------------------

# Deploy a program using a Doppler-injected keypair.
# PROGRAM_KEYPAIR_VAR is the *name* of the Doppler secret, not its value.
deploy-program CONFIG PROGRAM_KEYPAIR_VAR SO_PATH URL="localhost" FLAGS="":
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
                $4 \
                "$2" \
        ' _ "{{PROGRAM_KEYPAIR_VAR}}" "{{SO_PATH}}" "{{URL}}" "{{FLAGS}}"

deploy-program-testnet PROGRAM_KEYPAIR_VAR SO_PATH: (deploy-program "prd" PROGRAM_KEYPAIR_VAR SO_PATH "testnet")
# Surfpool does not serve TPU/QUIC; send buffer writes over RPC.
deploy-program-dev PROGRAM_KEYPAIR_VAR SO_PATH: (deploy-program "dev" PROGRAM_KEYPAIR_VAR SO_PATH "localhost" "--use-rpc")


# --- containerized stack (oracle + prometheus + grafana) -------------------------
# MTM_COMPOSE selects the engine: "docker compose" (default) or "nerdctl compose"
# for containerd-native environments (e.g. Kata sandboxes).

compose := env("MTM_COMPOSE", "docker compose")

# bring the stack up: `just up testnet` or `just up local` (start surfpool first)
up profile="testnet":
    @if [ "{{profile}}" = "local" ]; then \
        {{compose}} -f ops/compose.yaml -f ops/compose.local.yaml up -d --build; \
    else \
        MTM_PROFILE={{profile}} {{compose}} -f ops/compose.yaml up -d --build; \
    fi

down:
    {{compose}} -f ops/compose.yaml down

logs service="oracle":
    {{compose}} -f ops/compose.yaml logs -f {{service}}

# --- codegen ----------------------------------------------------------------------

# regenerate program SDKs from the IDLs in crates/sdks/*/idl (codama)
gen-sdks:
    cd codegen && pnpm install && pnpm run generate
    cargo fmt --all

# --- ops ------------------------------------------------------------------------

install-tools:
    ./scripts/install-tools.sh
