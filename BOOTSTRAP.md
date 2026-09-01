# Network Bootstrap

These are instructions for bootstrapping a local surfnet or testnet with all tokens, programs, and configurations created by the testnet-testsuite.

### Prerequisites

Ensure you have doppler installed and logged in with access to the `testnet-testsuite` project.
Clone the following repos in a sibling directory to this project:

- https://github.com/solana-foundation/raydium-clmm

### Env Vars

First, export environment variables that will be use across the subsequent commands.
If bootstrapping a local surfnet, run

```sh
export DOPPLER_PROJ=testnet-testsuite
export DOPPLER_CONFIG=dev
```

Then, start surfpool:

```sh
surfpool start \
  --airdrop $(doppler secrets get --project $DOPPLER_PROJ --config $DOPPLER_CONFIG --plain PUBKEY_FAUCET) \
  --airdrop $(doppler secrets get --project $DOPPLER_PROJ --config $DOPPLER_CONFIG --plain PUBKEY_PROGRAM_AUTHORITY) \
  --no-deploy \
  --disable-instruction-profiling \
  --network testnet
```

If bootstrapping testnet, run

```sh
export DOPPLER_PROJ=testnet-testsuite
export DOPPLER_CONFIG=prd
```

### Token Bootstrap

The [token bootstrap config](./bootstrap/tokens/config.json) defines token configuration and metadata to pull from mainnet for the local deployment.
Bootstrap the token deployment on surfpool by running:

```sh
just bootstrap-tokens-dev \
  --mainnet-rpc https://api.mainnet-beta.solana.com \
  --testnet-rpc http://localhost:8899
```

or, for testnet run:

```sh
just bootstrap-tokens
```

### Program Deployments

Each program will need to be built then deployed to the network.

#### Raydium CLMM

Navigate to the `raydium-clmm` program and ensure that all program and admin IDs are pulled appropriately from doppler. Then run `anchor build`

For surfnet deployment, run:

```sh
just deploy-program-dev "KEYPAIR_RAYDIUM_CLMM" "../raydium-clmm/target/deploy/raydium_clmm.so"
just configure-raydium-clmm-dev
```

then set up the pools with:

```sh
just raydium-clmm-create-pools-dev --amm-config <config-pubkey-from-last-command>
```

for testnet, run

```sh
just deploy-program "KEYPAIR_RAYDIUM_CLMM" "../raydium-clmm/target/deploy/raydium_clmm.so"
just configure-raydium-clmm
```

```sh
just raydium-clmm-create-pools --amm-config <config-pubkey-from-last-command>
```
