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

router profile="local":
    MTM_PROFILE={{profile}} cargo run -p router

# Run the router under a Doppler config. Program IDs are derived from the
# program keypair secrets in a wrapper shell; the router process only sees
# public keys. PUBKEY_JUPITER_FORK enables `execution = "jupiter"`.
router-with CONFIG PROFILE:
    doppler run --project testnet-testsuite --config "{{CONFIG}}" -- bash -c ' \
        set -euo pipefail; \
        pk() { if [ -n "${!1:-}" ]; then solana-keygen pubkey <(printf %s "${!1}"); fi; }; \
        export MTM_PROFILE="$1"; \
        export MTM_ROUTER__PROGRAMS__RAYDIUM_CLMM="$(pk KEYPAIR_RAYDIUM_CLMM)"; \
        export MTM_ROUTER__PROGRAMS__ORCA_WHIRLPOOL="$(pk KEYPAIR_ORCA_WHIRLPOOL)"; \
        export MTM_ROUTER__PROGRAMS__MANIFEST="$(pk KEYPAIR_MANIFEST)"; \
        export MTM_ROUTER__JUPITER__PROGRAM_ID="${PUBKEY_JUPITER_FORK:-}"; \
        export MTM_ROUTER__BRIDGE_MINTS="[\"${PUBKEY_TOKEN_USDC_MINT:?missing in config}\"]"; \
        unset KEYPAIR_RAYDIUM_CLMM KEYPAIR_ORCA_WHIRLPOOL KEYPAIR_MANIFEST; \
        exec cargo run -p router \
    ' _ "{{PROFILE}}"

router-dev: (router-with "dev" "local")
router-testnet: (router-with "prd" "testnet")

# Local only: open wide positions and rest orders on every discovered pool so
# the router has liquidity to route through (faucet-funded, left open).
router-seed-dev *ARGS:
    doppler run --project testnet-testsuite --config dev -- bash -c ' \
        set -euo pipefail; \
        pk() { solana-keygen pubkey <(printf %s "${!1:?missing Doppler secret: $1}"); }; \
        cargo run -p router --example seed_liquidity -- \
            --rpc-url http://localhost:8899 \
            --raydium-clmm "$(pk KEYPAIR_RAYDIUM_CLMM)" \
            --orca-whirlpool "$(pk KEYPAIR_ORCA_WHIRLPOOL)" \
            --manifest "$(pk KEYPAIR_MANIFEST)" "$@" \
    ' _ {{ARGS}}

# --- market-making scenario (bootstrap/scenario/README.md) ------------------------

# Local Surfpool forking testnet (Metaplex for token metadata), funding the faucet and admin.
scenario-surfpool:
    doppler run --project testnet-testsuite --config dev -- bash -c ' \
        surfpool start --network testnet --no-deploy --disable-instruction-profiling \
            --airdrop "$PUBKEY_FAUCET" --airdrop "$PUBKEY_PROGRAM_AUTHORITY"'

# Offline Surfpool (no testnet fallback for missing accounts: no remote
# fetch latency, closer to a real validator). Metaplex is preloaded from a
# snapshot; deploy the forks with `scenario-deploy-dev` afterwards.
scenario-surfpool-offline:
    test -f .scenario/metaplex.json || python3 -I bootstrap/scenario/metaplex-snapshot.py .scenario/metaplex.json
    doppler run --project testnet-testsuite --config dev -- bash -c ' \
        surfpool start --offline --snapshot .scenario/metaplex.json --no-deploy --disable-instruction-profiling \
            --airdrop-amount 100000000000000 --airdrop "$PUBKEY_FAUCET" --airdrop "$PUBKEY_PROGRAM_AUTHORITY"'

# SIMD-0500 (no new SBPF v0-v2 deploys) is active at test-validator genesis
# but not on testnet; the Raydium and Orca forks are v0 builds, so it is
# deactivated. The faucet is the genesis mint; run scenario-test-validator-fund next.
# Local Agave validator (real banking stage) to compare against Surfpool; ledger on tmpfs.
# The ledger lives in RAM when /dev/shm is writable: the default 5M-shred cap is
# ~20 GB (~40 min at x2000); SCENARIO_TV_SHREDS lowers or raises it.
# Short epochs: the SIMD-0525 slot-time features are active at genesis but only
# apply from the first epoch boundary (default 432k slots, ~2 days at 400 ms).
# 1024 slots puts the chain on 200 ms slots ~7 min in, before bootstrap ends.
# SCENARIO_TV_CLONE_FEATURES=<rpc url> copies that cluster's feature set into genesis
# instead, e.g. https://api.testnet.solana.com (Alpenglow on, so no vote transactions;
# SIMD-0500 already off there). Match the cluster's Agave version (testnet: 4.4).
scenario-test-validator:
    test -f .scenario/metaplex.json || python3 -I bootstrap/scenario/metaplex-snapshot.py .scenario/metaplex.json
    python3 -I bootstrap/scenario/metaplex-snapshot.py --account-dir .scenario/metaplex-accounts .scenario/metaplex.json
    doppler run --project testnet-testsuite --config dev -- bash -c ' \
        ledger="$([[ -w /dev/shm ]] && echo /dev/shm/mtm-test-ledger || echo .scenario/test-ledger)"; \
        solana-test-validator --ledger "$ledger" --reset --quiet --mint "$PUBKEY_FAUCET" \
            --account-dir .scenario/metaplex-accounts \
            --account-index program-id spl-token-owner spl-token-mint --limit-blockstore-size "${SCENARIO_TV_SHREDS:-5000000}" \
            --slots-per-epoch "${SCENARIO_TV_SLOTS_PER_EPOCH:-1024}" \
            $([[ -n "${SCENARIO_TV_CLONE_FEATURES:-}" ]] \
                && echo --clone-feature-set --url "$SCENARIO_TV_CLONE_FEATURES" \
                || echo --deactivate-feature B8JJXCy5amZyWG9r7EnUYLwzXSXTxG7GZ1qZ1qggo83g)'

# Regenerate every preset overlay (config/scale-*.toml) at current prices
# (SCALE_PRICES='{"JUP": "0.32", ...}' where Binance is unreachable).
scenario-overlays:
    cargo run -q -p scenario -- overlay

# Estimate a preset's load: just scenario-estimate x1500 [--chain local-400ms] [--target-tps 3000]
scenario-estimate PRESET *ARGS:
    cargo run -q -p scenario -- estimate --preset "{{PRESET}}" {{ARGS}}

# Fund the program authority on a fresh test validator (Surfpool airdrops it instead).
scenario-test-validator-fund:
    doppler run --project testnet-testsuite --config dev -- bash -c ' \
        solana transfer -u http://127.0.0.1:8899 --keypair <(printf %s "$KEYPAIR_FAUCET") \
            --allow-unfunded-recipient "$PUBKEY_PROGRAM_AUTHORITY" 100000'

# Run a scenario.sh command under a Doppler config: CMD = deploy|up|bootstrap|observe|status,
# or snapshot|report|measure with their operands in ARGS (e.g. `measure x10 600`).
scenario-with CONFIG TARGET CMD *ARGS:
    doppler run --project testnet-testsuite --config "{{CONFIG}}" -- \
        bootstrap/scenario/scenario.sh "{{CMD}}" {{ARGS}} "{{TARGET}}"

# Measure a running scenario: snapshot → SECONDS of load sampling → snapshot → report,
# into .scenario/runs/NAME-*.json. SCALE (x10|x100|assets) must match the one it was started with.
scenario-measure CONFIG TARGET NAME SECONDS SCALE="":
    SCENARIO_SCALE="{{SCALE}}" doppler run --project testnet-testsuite --config "{{CONFIG}}" -- \
        bootstrap/scenario/scenario.sh measure "{{NAME}}" "{{SECONDS}}" "{{TARGET}}"

scenario-deploy-dev: (scenario-with "dev" "local" "deploy")
scenario-up-dev: (scenario-with "dev" "local" "up")
scenario-status-dev: (scenario-with "dev" "local" "status")
scenario-deploy-testnet: (scenario-with "prd" "testnet" "deploy")
scenario-teardown-dev: (scenario-with "dev" "local" "teardown")
scenario-teardown-testnet: (scenario-with "prd" "testnet" "teardown")
scenario-testnet: (scenario-with "prd" "testnet" "up")
scenario-status-testnet: (scenario-with "prd" "testnet" "status")
# Dashboard (docs/dashboard-plan.md): control serves the built UI on
# http://127.0.0.1:8182 and runs scenario.sh jobs against the target that
# matches its Doppler config. Build the UI first (or use dashboard-dev).
dashboard-build:
    pnpm --filter dashboard run build

# Vite dev server on :5173, proxying /api to a running control.
dashboard-dev:
    pnpm --filter dashboard run dev

control-dev:
    doppler run --project testnet-testsuite --config dev -- \
        env MTM_CONTROL__TARGET=local cargo run --release -p control

control-testnet:
    doppler run --project testnet-testsuite --config prd -- \
        env MTM_CONTROL__TARGET=testnet cargo run --release -p control

control-devnet:
    doppler run --project testnet-testsuite --config "{{DEVNET_CONFIG}}" -- \
        env MTM_CONTROL__TARGET=devnet cargo run --release -p control

# chain-watch alone (`up` starts it too): slots, blocks, versions, feature gates
# into .scenario/runs/<run id>/run.db; API :8183, metrics :9185.
scenario-watch-dev: (scenario-with "dev" "local" "watch")
scenario-watch-testnet: (scenario-with "prd" "testnet" "watch")

# Devnet rehearsal of the testnet run, under the Doppler config DEVNET_CONFIG
# (`devnet` unless overridden: `just DEVNET_CONFIG=stg scenario-devnet`).
DEVNET_CONFIG := env_var_or_default("DEVNET_CONFIG", "devnet")
scenario-deploy-devnet: (scenario-with DEVNET_CONFIG "devnet" "deploy")
scenario-devnet: (scenario-with DEVNET_CONFIG "devnet" "up")
scenario-status-devnet: (scenario-with DEVNET_CONFIG "devnet" "status")
scenario-teardown-devnet: (scenario-with DEVNET_CONFIG "devnet" "teardown")

# Deploy the forks, bootstrap and run every service, then start observability.
scenario-dev: scenario-deploy-dev scenario-up-dev scenario-observe

scenario-observe:
    bootstrap/scenario/scenario.sh observe

scenario-down:
    bootstrap/scenario/scenario.sh down

# --- janitor program (programs/janitor) ----------------------------------------------

# Build the janitor for a Doppler config, with that config's program authority
# and faucet compiled in as its authorities: target/janitor/CONFIG/janitor.so.
build-janitor CONFIG="dev":
    doppler run --project testnet-testsuite --config "{{CONFIG}}" -- bash -c ' \
        set -euo pipefail; \
        JANITOR_AUTHORITIES="${PUBKEY_PROGRAM_AUTHORITY:?}, ${PUBKEY_FAUCET:?}" \
            cargo-build-sbf --manifest-path programs/janitor/Cargo.toml --arch v3 \
            --sbf-out-dir "target/janitor/{{CONFIG}}"; \
        rm -f "target/janitor/{{CONFIG}}/janitor-keypair.json"'

# Lint and test the janitor: an SBF build embedding the test-only authorities in
# programs/janitor/tests/fixtures, then LiteSVM tests against it.
test-janitor:
    #!/usr/bin/env bash
    set -euo pipefail
    fixtures=programs/janitor/tests/fixtures
    export JANITOR_AUTHORITIES="$(solana-keygen pubkey $fixtures/authority-a.json),$(solana-keygen pubkey $fixtures/authority-b.json)"
    cargo-build-sbf --manifest-path programs/janitor/Cargo.toml --arch v3 --sbf-out-dir target/janitor/test
    rm -f target/janitor/test/janitor-keypair.json
    export JANITOR_SO="$PWD/target/janitor/test/janitor.so"
    cargo clippy --manifest-path programs/janitor/Cargo.toml --all-targets -- -D warnings
    cargo test --manifest-path programs/janitor/Cargo.toml

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
