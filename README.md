# Testnet test suite

Solana testnet traffic/testing stack. This bootstrap contains an oracle service
(Pyth pull → HTTP/WSS API → on-chain pusher, WIP) and a token bootstrap script;
trading bots (arb, market maker) come next. Primary target cluster is **testnet** —
the point is generating realistic traffic against it.

## Layout

| Path | What |
|---|---|
| `crates/mtm-common` | Shared leaf types (`Symbol`) + layered config loading |
| `crates/mtm-math` | Canonical fixed-point `Price` (i128 mantissa × 10^expo) + rust_decimal bridge |
| `crates/mtm-chain` | Off-chain Solana toolkit: RPC clients, keypairs, (soon) tx pipeline |
| `crates/oracle-client` | Typed client for the oracle service; owns the API wire types |
| `services/oracle` | Polls Hermes → serves `/v1/prices` + `/v1/ws` → pushes on-chain (WIP) |
| `programs/` | On-chain programs (empty for now — Anchor workspace lands here later) |
| `config/` | Layered config: `default.toml` ← `{profile}.toml` ← `MTM_*` env vars |

## Quickstart

```sh
just install-tools        # nextest, cargo-deny (one-time)
just check                # compile the workspace
just test

just oracle               # run oracle with the local profile
just oracle testnet       # ... against testnet
```

Profiles: `MTM_PROFILE` selects `config/{profile}.toml` (default `local`).
Any value can be overridden via env: `MTM_RPC__HTTP_URL=... just oracle testnet`.

## Running the stack (containerized)

Sustained runs use Docker Compose: the oracle plus Prometheus (`:9090`) and
Grafana (`:3000`, admin/admin unless `GRAFANA_PASSWORD` is set), all bound to
host loopback only.

```sh
just up testnet           # build image + start against testnet
pnpm surfpool:mainnet &   # local profile needs surfpool on the HOST first
just up local             # ... then start against it
just logs oracle
just down
```

The image is a cargo-chef multi-stage build (dependency layer cached — source
rebuilds are fast) on a distroless runtime; TLS roots are compiled into the
binaries so no OS cert store is needed. Config is mounted from `config/`, so
instrument edits only need a container restart, not a rebuild. When the bots
need secrets: `doppler run -- just up testnet`.

## Token bootstrap

`bootstrap/tokens/config.json` declares each mainnet mint and the names of the
environment variables that contain the required Solana keypairs. The deployment script
reads the mainnet mint and Metaplex metadata, then creates a classic SPL Token
mint and Metaplex metadata PDA on Solana testnet.

It never reads local keypair files and never prints secret material. The
`mintAuthority` keypair is the testnet transaction payer. The four configured
keypairs must be JSON-encoded Solana secret-key byte arrays in Doppler.

The default is a read-only dry run:

```sh
just bootstrap-tokens
```

The Justfile injects those variables using Doppler project `testnet-testsuite`
and configuration `prd`. To submit the simulated deployment, pass `--apply`:

```sh
just bootstrap-tokens --apply
```

Optional flags are `--config <path>`, `--mainnet-rpc <url>`, and
`--testnet-rpc <url>`. Both RPC defaults are Solana public endpoints.

The script deliberately creates a zero-supply mint. Recreating a mainnet supply
requires an explicit recipient and distribution policy, neither of which belongs
in this bootstrap configuration. The dry-run report includes the original supply
for audit purposes.

## Conventions

- **Interface/client pattern.** Services never depend on each other — only on
  `*-client` crates. When on-chain programs land, each gets a `<name>-interface`
  crate shared by program and clients.
- **Overflow checks stay on in release.** See root `Cargo.toml`.
- New shared code goes in `crates/`, binaries stay thin (`main.rs` = config + telemetry + `lib::run`).

