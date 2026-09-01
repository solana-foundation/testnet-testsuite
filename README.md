# Testnet test suite

Solana testnet traffic/testing stack: an oracle service (Pyth pull → HTTP/WSS API →
on-chain pusher), an arbitrage bot, a market maker, and shared infrastructure crates.
Primary target cluster is **testnet** — the point is generating realistic traffic
against it.

## Layout

| Path                                 | What                                                                   |
| ------------------------------------ | ---------------------------------------------------------------------- |
| `crates/oracle-client`               | Typed client for the oracle service; owns the API wire types           |
| `crates/raydium-clmm-client`         | Async Raydium CLMM pool, position, collection, close, and swap actions |
| `crates/raydium-clmm-orchestrator`   | Reusable funding and full CLMM lifecycle sequencing                    |
| `services/oracle`                    | Polls Hermes → serves `/v1/prices` + `/v1/ws` → pushes on-chain (WIP)  |
| `services/raydium-clmm-orchestrator` | CLI with `admin` setup and complete `flow` lifecycle commands          |
| `programs/`                          | On-chain programs (empty for now — Anchor workspace lands here later)  |
| `config/`                            | Layered config: `default.toml` ← `{profile}.toml` ← `MTM_*` env vars   |

## Quickstart

```sh
just install-tools        # nextest, cargo-deny (one-time)
just check                # compile the workspace
just test

just oracle               # run oracle with the local profile
just oracle testnet       # ... against testnet
```

Profiles: `MTM_PROFILE` selects `config/{profile}.toml` (default `local`).
Any value can be overridden via env: `MTM_RPC__HTTP_URL=... just arb testnet`.

## Token bootstrap

`bootstrap/tokens/config.json` declares each mainnet mint and the names of the
environment variables that contain the required Solana keypairs. The deployment script
reads the mainnet mint and Metaplex metadata, then creates a classic SPL Token
mint and Metaplex metadata PDA on Solana testnet.

It never reads local keypair files and never prints secret material. The fee
payer defaults to `KEYPAIR_FAUCET`; mint and metadata authorities sign only the
operations that require them. All configured keypairs must be JSON-encoded
Solana secret-key byte arrays in Doppler.

The production recipe defaults to a read-only dry run:

```sh
just bootstrap-tokens
```

The Justfile injects those variables using Doppler project `testnet-testsuite`
and configuration `prd`. To submit the simulated deployment, pass `--apply`:

```sh
just bootstrap-tokens --apply
```

Optional flags are `--config <path>`, `--mainnet-rpc <url>`, and
`--testnet-rpc <url>`. `--payer-keypair-env <name>` selects the injected fee
payer and defaults to `KEYPAIR_FAUCET`. Both RPC defaults are Solana public
endpoints.

The dev recipe always adds `--apply`. With a local Surfpool running, the exact
command below creates the configured JUP, RAY, and USDC mints and pays rent from
the airdropped faucet instead of the mint authority:

```sh
just bootstrap-tokens-dev \
  --mainnet-rpc https://api.mainnet-beta.solana.com \
  --testnet-rpc http://localhost:8899
```

The script deliberately creates a zero-supply mint. Recreating a mainnet supply
requires an explicit recipient and distribution policy, neither of which belongs
in this bootstrap configuration. The dry-run report includes the original supply
for audit purposes.

## Raydium CLMM client

`raydium-clmm-client` accepts an existing `ChainClient` and an injected signer. It
never reads or writes keypair files. Amounts and prices use `rust_decimal::Decimal`;
pool addresses use Agave 4.x `solana_pubkey::Pubkey` values.
Replace the uppercase address placeholders below with funded testnet accounts.

```rust,no_run
use std::{str::FromStr, sync::Arc};

use mtm_chain::ChainClient;
use raydium_clmm_client::{
    CreatePoolParams, RaydiumClmmClient, SwapExactInParams,
};
use rust_decimal::Decimal;
use solana_commitment_config::CommitmentConfig;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let chain = ChainClient::new("https://api.testnet.solana.com", "confirmed")?;
let payer = Arc::new(Keypair::new()); // inject a funded signer from your application
let program_id = Pubkey::from_str("DRayAUgENGQBKVaX8owNhgzkEDyoHTGVEGHVJT1E9pfH")?;
let client = RaydiumClmmClient::new(
    chain,
    program_id,
    payer,
    50,        // 0.50% slippage
    1_400_000, // compute-unit limit
    1_000,     // micro-lamports per compute unit
    CommitmentConfig::confirmed(),
)?;

let created = client
    .create_pool(CreatePoolParams {
        amm_config: Pubkey::from_str("AMM_CONFIG_ADDRESS")?,
        mint_a: Pubkey::from_str("FIRST_MINT_ADDRESS")?,
        mint_b: Pubkey::from_str("SECOND_MINT_ADDRESS")?,
        initial_price: Decimal::new(125, 2), // 1.25 token B per token A
        open_time: None,                     // defaults to 0
    })
    .await?;

let swap = client
    .swap_exact_in(SwapExactInParams {
        pool: created.accounts.pool,
        input_mint: created.quote.mint_0,
        output_mint: created.quote.mint_1,
        amount_in: Decimal::new(10, 0),
    })
    .await?;

tracing::info!(
    signature = %swap.transaction.signature,
    slot = swap.transaction.confirmation_slot,
    compute_units = swap.transaction.simulated_compute_units,
    "swap confirmed"
);
# Ok(())
# }
```

The action methods are `create_pool`, `open_position`, `increase_liquidity`,
`decrease_liquidity`, `collect_position`, `close_position`, `swap_exact_in`, and
`swap_exact_out`. Position lookups use the position-NFT mint. Pool lookups use the
pool address.

Every transaction is signed once, simulated, submitted, and confirmed through
`mtm-chain`. A failed simulation is never submitted. Transport retries reuse the
same signed transaction; an expired blockhash with no observed status returns an
ambiguous-confirmation error instead of rebuilding a transaction that may already
have executed.

The client creates missing associated token accounts idempotently and supports
classic SPL Token plus Raydium-supported Token-2022 mint extensions. It does not
wrap native SOL, initialize rewards, route across pools, or read keypairs from
disk. Dynamic-fee pools and swaps that encounter Raydium limit orders return a
quote error; dynamic-fee configuration and limit-order execution are outside this
client.

## Raydium CLMM admin setup

Raydium's program admin is compiled into the SBF binary; it cannot be assigned by
an instruction after deployment. Build Raydium manually with the desired program
ID and localnet admin, then deploy the resulting SBF artifact. The orchestrator
does not modify or build the Raydium source tree.

The default local workflow uses `KEYPAIR_PROGRAM_AUTHORITY` as both upgrade
authority and Raydium admin because the supplied Surfpool command funds it. After
manually building `../raydium-clmm`, deploy the artifact and create config index
0:

```sh
NO_DNA=1 just deploy-program-dev \
  "KEYPAIR_RAYDIUM_CLMM" \
  "../raydium-clmm/target/deploy/raydium_clmm.so"

NO_DNA=1 just configure-raydium-clmm-dev
```

The admin command signs `create_amm_config` with the injected admin. Subsequent
runs validate every requested field and return `created: false` without
submitting another transaction.

The default config is tick spacing `10`, trade fee rate `500` (0.05%), protocol
share `120000` (12% of trade fees), and fund share `0`; all rates use Raydium's
1,000,000 denominator. Override them after the recipe name:

```sh
NO_DNA=1 just configure-raydium-clmm-dev \
  --config-index 1 \
  --tick-spacing 60 \
  --trade-fee-rate 3000 \
  --protocol-fee-rate 120000 \
  --fund-fee-rate 0
```

For a dedicated admin secret, compile its public key into Raydium, ensure it is
funded by Surfpool, deploy the manually built artifact, and select the same
variable during configuration:

```sh
NO_DNA=1 just configure-raydium-clmm-dev-with KEYPAIR_RAYDIUM_CLMM_ADMIN
```

Changing the compiled admin requires manually rebuilding and redeploying the
program.

## Raydium CLMM full-flow orchestrator

`raydium-clmm-orchestrator` is the application layer above the action client. It
accepts injected payer and mint-authority signers plus decimal mints, prices, and
amounts. `run_full_flow` then funds the payer and executes:

```text
create pool → open position → increase liquidity → exact-in swap →
exact-out swap → withdraw all liquidity → collect → close position
```

The typed `FullFlowOutcome` retains every action quote, derived/created address,
signature, confirmation slot, and simulated compute-unit count. The library does
not read environment variables; only the thin `raydium-clmm-orchestrator` CLI
resolves JSON-encoded keypairs from named environment variables.

Start and provision the local Surfpool with:

```sh
NO_DNA=1 surfpool start \
  --airdrop 6Ac9P7p499uvu6UJcNwqnyDhqJG7GxhE7nLNkgWKYUob \
  --airdrop JA6aXK9AqCYTTypE8bag3kViKX813GoVf3KWcj3ToCg1 \
  --no-deploy \
  --disable-instruction-profiling

NO_DNA=1 just deploy-program-dev \
  "KEYPAIR_RAYDIUM_CLMM" \
  "../raydium-clmm/target/deploy/raydium_clmm.so"

NO_DNA=1 just configure-raydium-clmm-dev

NO_DNA=1 just bootstrap-tokens-dev \
  --mainnet-rpc https://api.mainnet-beta.solana.com \
  --testnet-rpc http://localhost:8899
```

The default configuration command creates config index 0 at
`7qcaBohkMCzE9xQPzNXKbWyMdtqsmvSJmhqmgNs9tuCK` for the current dev program key.
The flow CLI derives the program, USDC, RAY, payer, and mint-authority public keys
from the corresponding Doppler keypairs unless explicit `--program-id`,
`--mint-a`, or `--mint-b` values are supplied:

```sh
NO_DNA=1 just raydium-clmm-flow-dev \
  --amm-config 7qcaBohkMCzE9xQPzNXKbWyMdtqsmvSJmhqmgNs9tuCK

# On a mainnet-backed Surfpool, use Raydium's forked canonical program/config.
NO_DNA=1 just raydium-clmm-flow-dev \
  --program-id CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK \
  --amm-config 4BLNHtVe942GSs4teSZqGX24xwKNkqU7bGgNn3iUiUpw
```

Run `cargo run -p raydium-clmm-orchestrator-service -- flow --help` for funding
amounts, position range, swap amounts, slippage, compute budget, priority fee,
commitment, and keypair-variable overrides. Run the same command with
`admin --help` for AMM-config options. Pool creation remains intentionally
non-idempotent; use fresh mints/configuration or a fresh Surfpool for each
complete run.

## Conventions

- **Interface/client pattern.** Services never depend on each other — only on
  `*-client` crates. When on-chain programs land, each gets a `<name>-interface`
  crate shared by program and clients.
- **Overflow checks stay on in release.** See root `Cargo.toml`.
- New shared code goes in `crates/`, binaries stay thin (`main.rs` = config + telemetry + `lib::run`).
