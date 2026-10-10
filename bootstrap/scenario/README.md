# Market-making scenario

A replayable scenario that keeps all three DEX forks trading: market makers
quote Manifest books, provide Raydium CLMM and Orca liquidity, and arbitrage
the AMMs back to external prices, while uninformed takers send random (and
occasionally large) swaps through the router. Everything is observable in
Grafana.

```text
Binance ──▶ oracle ──▶ mm (12 makers) ──▶ Manifest / Orca / Raydium CLMM ◀── router ◀── flow (takers)
                         │                          ▲                                    │
                         └── prometheus ◀── metrics ┴────────────────────────────────────┘
```

| Piece                                                  | What it does                                                                                                                      |
| ------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------- |
| `config/scenario-local.toml` / `scenario-testnet.toml` | The whole scenario: tokens → oracle symbols, pools per venue, makers (strategy, venue, inventory), flow parameters, service ports |
| `services/scenario`                                    | Idempotent bootstrap: venue configs, pools created **at the live oracle price** if missing, wallet funding                        |
| `services/mm`                                          | One task per maker; strategies below                                                                                              |
| `services/flow`                                        | Poisson takers routing through the router, measuring landed vs quoted vs oracle                                                   |
| `services/oracle`, `services/router`                   | Existing services, run with the scenario profile                                                                                  |
| `bootstrap/scenario/scenario.sh`                       | Runs everything (`deploy`, `up`, `bootstrap`, `observe`, `status`, `down`)                                                        |

## Strategies

- **`clob-ladder`** (Manifest): `levels` bids and asks starting `spread_bps`
  from the oracle mid and `step_bps` apart. The ladder is replaced in one
  atomic `BatchUpdate` when the mid moves `refresh_bps` or an order fills.
  `skew_bps` shifts the mid against inventory (long base → quote lower).
- **`clmm-range`** (Raydium CLMM, Orca): one position `width_bps` wide around
  the pool price, re-centred when the price drifts `recenter_bps`. The open
  position is persisted in `.scenario/state/<maker>.json`.
- **`pool-arb`** (any venue): when the pool strays `threshold_bps` from the
  oracle, trades the size that moves its marginal price back to the oracle
  (capped at `max_trade_usd`). This is what makes AMM prices follow Binance.

Uninformed flow: Poisson arrivals at `rate_per_min`, lognormal notionals
around `median_usd`, a `large_probability` share of blocks at
`large_multiplier`×, random pairs (JUP/RAY has no direct Manifest market, so
large JUP↔RAY flow also exercises bridging), `exact_out_probability` exact-out
trades. The RNG is seeded (`rng_seed`), so the trade sequence replays.

## Replay

All state is derived or idempotent, so every command can be re-run:

- wallets derive from `SCENARIO_SEED` (`maker/<name>`, `taker/<i>`), so a
  replay drives the same accounts;
- the token bootstrap skips mints that already exist (`--skip-existing`);
- pools and markets are created only when missing;
- makers receive their inventory once (an endowment: funds move into seats
  and positions, so a low wallet is not refunded); takers are topped up to
  their targets, and the flow service refills them when they run low.

### Local (Surfpool)

Prerequisites: the three fork artifacts (see [docs/dex-forks.md](../../docs/dex-forks.md))
at `../raydium-clmm`, `../whirlpools`, `../manifest` (or set
`SCENARIO_RAYDIUM_SO`, `SCENARIO_ORCA_SO`, `SCENARIO_MANIFEST_SO`), an Agave
≥ 4.x `solana` CLI (Manifest is SBPF v3), and `pnpm install`.

```sh
NO_DNA=1 just scenario-surfpool      # terminal 1: Surfpool forking testnet (Metaplex for token metadata)
NO_DNA=1 just scenario-dev           # deploy forks, run everything, start observability
NO_DNA=1 just scenario-status-dev    # pools, wallets and balances, processes
just scenario-down                   # stop services (Surfpool state is in memory)
```

Grafana: <http://127.0.0.1:3100> (admin/admin), dashboard **mtm — market-making
scenario**. Prometheus: <http://127.0.0.1:9190>. Logs: `.scenario/logs/`.
Scenario ports (oracle 8180, router 8181, metrics 9180–9183, Prometheus 9190,
Grafana 3100) are chosen so the default `just up` stack can run alongside.

`just scenario-observe` uses `ops/compose.scenario.yaml` when Docker is usable
and otherwise runs `prometheus` and `grafana` binaries from `PATH` as the
current user. Debug builds: `SCENARIO_BUILD=debug`.

### Testnet

```sh
NO_DNA=1 just scenario-deploy-testnet   # once per program release
NO_DNA=1 just scenario-testnet          # bootstrap + run (prd Doppler config)
NO_DNA=1 just scenario-status-testnet
```

The testnet profile slows every maker 5× and flow to 4 trades/min for the
public RPC; set `SCENARIO_RPC_URL` (and `SCENARIO_WS_URL`) to a dedicated
endpoint for sustained runs. `SCENARIO_SEED` is mandatory there.

## Doppler variables

| Variable                                                                                                                                                     | Used by                                                                                                        |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------- |
| `SCENARIO_SEED` **(new)**                                                                                                                                    | secret all maker/taker wallets derive from (`prd` required; `dev` optional — the local profile has a dev seed) |
| `KEYPAIR_ORCA_WHIRLPOOL`, `KEYPAIR_ORCA_WHIRLPOOLS_CONFIG`, `KEYPAIR_MANIFEST`                                                                               | from the DEX branches                                                                                          |
| `KEYPAIR_RAYDIUM_CLMM`, `KEYPAIR_FAUCET`, `KEYPAIR_PROGRAM_AUTHORITY`, `KEYPAIR_TOKEN_*`, `PUBKEY_TOKEN_*_MINT`, `PUBKEY_FAUCET`, `PUBKEY_PROGRAM_AUTHORITY` | existing                                                                                                       |

## Metrics

| Metric                                                                               | Labels               | Meaning                                             |
| ------------------------------------------------------------------------------------ | -------------------- | --------------------------------------------------- |
| `mm_oracle_price`                                                                    | pair                 | oracle pair price the makers use                    |
| `mm_venue_price`, `mm_deviation_bps`                                                 | maker, venue         | venue price and its gap to the oracle               |
| `mm_inventory`, `mm_inventory_value_usd`                                             | maker, token         | holdings (wallet + seat) and USD value              |
| `mm_actions_total`                                                                   | maker, action        | requote / open / close / arb_buy / arb_sell         |
| `mm_errors_total`, `mm_panics_total`, `venue_quote_panics_total`                     | maker, stage / venue | failures (ticks retry)                              |
| `mm_skew_bps`, `mm_arb_volume_usd`, `mm_tick_seconds`                                | maker                | skew, arbitrage volume, tick latency                |
| `flow_swaps_total`                                                                   | pair, status, size   | routed swaps landed/failed, normal/large            |
| `flow_cost_vs_oracle_bps`, `flow_slippage_bps`                                       | pair                 | cost to uninformed flow vs oracle; landed vs quoted |
| `flow_route_legs_total`, `flow_notional_usd_total`, `flow_refills_total`             | venue / pair         | routing mix, volume, refills                        |
| `router_quote_seconds`, `router_quote_errors_total`, `router_quotes_total`           | execution, status    | router latency and failures                         |
| `oracle_price_updates_total`, `{orca_whirlpool,raydium_clmm,manifest}_actions_total` | —                    | feed health and per-venue transactions              |
