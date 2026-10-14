# Market-making scenario

A replayable scenario that keeps all three DEX forks trading: market makers
quote Manifest books, provide Raydium CLMM and Orca liquidity, and arbitrage
the AMMs back to external prices, while uninformed takers send random (and
occasionally large) swaps through the router, and a cross-venue arbitrage
bot closes the price gaps that flow opens. Everything is observable in
Grafana, and `snapshot`/`report` produce per-participant PnL.

```text
Binance ──▶ oracle ──▶ mm (12 makers) ──▶ Manifest / Orca / Raydium CLMM ◀── router ◀── flow (takers)
                         │                          ▲  │ ws account updates                 │
                         │                          └──┴── arb (cycle searcher)             │
                         └── prometheus ◀── metrics ────────────────────────────────────────┘
```

| Piece                                                  | What it does                                                                                                                      |
| ------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------- |
| `config/scenario-local.toml` / `scenario-testnet.toml` | The whole scenario: tokens → oracle symbols, pools per venue, makers (strategy, venue, inventory), flow parameters, service ports |
| `services/scenario`                                    | Idempotent bootstrap: venue configs, pools created **at the live oracle price** if missing, wallet funding                        |
| `services/mm`                                          | One task per maker; strategies below                                                                                              |
| `services/flow`                                        | Poisson takers routing through the router, measuring landed vs quoted vs oracle                                                   |
| `services/arb`                                         | Websocket-triggered 2- and 3-leg cycle searcher across venues, one v1 transaction per cycle                                       |
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

- **cross-venue arbitrage** (`services/arb`, `[scenario.arb]`): subscribes
  to every pool and market account; each update (debounced) reloads the
  changed pools and searches every 2-leg (same pair, two venues) and 3-leg
  (triangular) cycle starting from the bot's `start_tokens`. Cycle output is
  concave in its input, so the size comes from a ternary search on pure
  snapshot quotes, capped by `max_trade_usd` and inventory. The best cycle
  above `min_profit_usd` is sent as one v1 transaction of top-level venue
  swaps; legs chain on the previous leg's minimum and the last leg's
  minimum is `input + min_profit`, so a stale cycle reverts rather than
  losing money. Unlike `pool-arb` it never looks at the oracle to trade
  (only to value profit).

Uninformed flow: Poisson arrivals at `rate_per_min`, lognormal notionals
around `median_usd`, a `large_probability` share of blocks at
`large_multiplier`×, random pairs (JUP/RAY has no direct Manifest market, so
large JUP↔RAY flow also exercises bridging), `exact_out_probability` exact-out
trades. The RNG is seeded (`rng_seed`), so the trade sequence replays.

## Replay

All state is derived or idempotent, so every command can be re-run:

- wallets derive from `SCENARIO_SEED` (`maker/<name>`, `arb/<name>`, `taker/<i>`), so a
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
≥ 4.x `solana` CLI (Manifest is SBPF v3), `curl` and `jq`, and `pnpm install`.

```sh
NO_DNA=1 just scenario-surfpool      # terminal 1: Surfpool forking testnet (Metaplex for token metadata)
NO_DNA=1 just scenario-dev           # deploy forks, run everything, start observability
NO_DNA=1 just scenario-status-dev    # pools, wallets and balances, processes
just scenario-down                   # stop services (Surfpool state is in memory)
```

Grafana: <http://127.0.0.1:3100> (admin/admin), dashboard **mtm — market-making
scenario**. Prometheus: <http://127.0.0.1:9190>. Logs: `.scenario/logs/`.
Scenario ports (oracle 8180, router 8181, metrics 9180–9184, Prometheus 9190,
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

## PnL

```sh
scenario.sh snapshot .scenario/t0.json   # holdings + oracle prices + slot
# ... let it run ...
scenario.sh snapshot .scenario/t1.json
scenario.sh report .scenario/t0.json .scenario/t1.json   # needs Prometheus on :9190
```

A snapshot values everything a participant owns: wallet tokens and SOL,
Manifest seat balances plus tokens locked in resting orders, and LP
positions (token amounts the position would withdraw at the current price,
plus uncollected fees). The report gives, per participant:

- `tradingPnlUsd`: both inventories at end prices (the market move removed);
- `markToMarketPnlUsd`: end value at end prices minus start value at start prices;
- `solSpent` (fees), token deltas, landed/failed transactions it signed in
  the slot window (scanned block by block with `getBlock`, votes excluded),
  and trade counts from service metrics.

It also sums PnL per participant kind (`byKind`) and reports network-wide
landed/failed transactions and TPS for the window (`network`).

Taker refills mint tokens and would show up as PnL; for a clean window run
flow with `MTM_SCENARIO__FLOW__REFILL_BELOW_RATIO=0`.

## Doppler variables

| Variable                                                                                                                                                     | Used by                                                                                                            |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------ |
| `SCENARIO_SEED` **(new)**                                                                                                                                    | secret all maker/arb/taker wallets derive from (`prd` required; `dev` optional — the local profile has a dev seed) |
| `KEYPAIR_ORCA_WHIRLPOOL`, `KEYPAIR_ORCA_WHIRLPOOLS_CONFIG`, `KEYPAIR_MANIFEST`                                                                               | from the DEX branches                                                                                              |
| `KEYPAIR_RAYDIUM_CLMM`, `KEYPAIR_FAUCET`, `KEYPAIR_PROGRAM_AUTHORITY`, `KEYPAIR_TOKEN_*`, `PUBKEY_TOKEN_*_MINT`, `PUBKEY_FAUCET`, `PUBKEY_PROGRAM_AUTHORITY` | existing                                                                                                           |

## Metrics

| Metric                                                                                         | Labels                      | Meaning                                                 |
| ---------------------------------------------------------------------------------------------- | --------------------------- | ------------------------------------------------------- |
| `mm_oracle_price`                                                                              | pair                        | oracle pair price the makers use                        |
| `mm_venue_price`, `mm_deviation_bps`                                                           | maker, venue                | venue price and its gap to the oracle                   |
| `mm_inventory`, `mm_inventory_value_usd`                                                       | maker, token                | holdings (wallet + seat) and USD value                  |
| `mm_actions_total`                                                                             | maker, action               | requote / open / close / arb_buy / arb_sell             |
| `mm_errors_total`, `mm_panics_total`, `venue_quote_panics_total`                               | maker, stage / venue        | failures (ticks retry)                                  |
| `mm_skew_bps`, `mm_arb_volume_micro_usd_total`, `mm_tick_seconds`                              | maker                       | skew, arbitrage volume (µUSD), tick latency             |
| `flow_swaps_total`                                                                             | pair, status, outcome, size | routed swaps landed/failed, why, normal/large           |
| `flow_fee_lamports_total`, `flow_fill_read_errors_total`                                       | —                           | fees paid by takers; landed swaps with unread fills     |
| `flow_takers_alive`, `flow_taker_restarts_total`                                               | —                           | taker loops running / restarted after an error          |
| `flow_cost_vs_oracle_bps`, `flow_slippage_bps`                                                 | pair                        | cost to uninformed flow vs oracle; landed vs quoted     |
| `flow_route_legs_total`, `flow_notional_micro_usd_total`, `flow_refills_total`                 | venue / pair                | routing mix, volume (µUSD), refills                     |
| `router_quote_seconds`, `router_quote_errors_total`, `router_quotes_total`                     | execution, status           | router latency and failures                             |
| `oracle_price_updates_total`, `{orca_whirlpool,raydium_clmm,manifest}_actions_total`           | —                           | feed health and per-venue transactions                  |
| `arb_trades_total`                                                                             | bot, status, legs           | landed / below_floor / simulation_failed / quote_failed |
| `arb_opportunities_total`, `arb_profit_micro_usd_total`, `arb_last_profit_usd`                 | bot, legs, trigger          | cycles found, realized profit                           |
| `arb_scans_total`, `arb_scan_seconds`, `arb_pool_updates_total`, `arb_snapshot_failures_total` | trigger / reason            | search cadence and latency, ws updates, slow reloads    |
