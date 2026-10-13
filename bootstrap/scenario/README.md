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
  (triangular) cycle starting from the bot's `start_tokens`. Each cycle is
  sized on pure snapshot quotes as it will execute (legs chained on the
  previous leg's slippage-adjusted minimum): log-spaced probes, then a
  ternary search around the best, capped by `max_trade_usd` and inventory.
  The best cycle above `min_profit_usd` is sent as one v1 transaction of
  top-level venue swaps; the last leg's minimum is `input + min_profit`, so
  a stale cycle reverts rather than losing money. Unlike `pool-arb` it
  never looks at the oracle to trade (only to value profit). Bots race on
  each snapshot; `priority_fee_lamports` and `latency_ms` per bot make them
  differ. A bot with a cycle confirming sits out later scans, and scans
  never wait for confirmations.

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
landed/failed transactions and TPS for the window (`network`, with
`slotsSkipped`). `byVenue` splits the window's transactions by the DEX
programs they call (`other`: none): landed, failed, failure rate and the
top error codes from every block, and compute (mean, p95, share of all
non-vote compute) from every 10th slot's full block (`--compute-sample-every`;
0 turns it off). A multi-venue route counts for each venue it touches.

RPC errors fail the snapshot or report rather than reading as "no
positions" or "empty block"; the Prometheus parts are best-effort and
anything missing is listed in `warnings`.

Taker refills mint tokens and would show up as PnL (the report warns when
any happened in the window); for a clean window run flow with
`MTM_SCENARIO__FLOW__REFILL_BELOW_RATIO=0`.

## Scaling

`bootstrap/scenario/scale.py` generates overlays layered over the profile
(`MTM_PROFILE=scenario-local,scale-<name>`; `scenario.sh` does this from
`SCENARIO_SCALE`):

| `SCENARIO_SCALE` | participants | takers | arb bots | makers | assets / pools | maker capital |
| ---------------- | ------------ | ------ | -------- | ------ | -------------- | ------------- |
| (unset)          | 17           | 4      | 1        | 12     | 3 / 7          | ×1            |
| `x10`            | 170          | 140    | 6        | 24     | 3 / 7          | ×3            |
| `x100`           | 1700         | 1650   | 14       | 36     | 3 / 7          | ×10           |
| `assets`         | 1756         | 1650   | 14       | 92     | 11 / 39        | ×10           |
| `x200`           | 3356         | 3300   | 20       | 36     | 3 / 7          | ×20           |

Takers keep the base 5 trades/min each, so offered load scales with them.
The asset scale adds BONK, WIF, PYTH, JTO, ORCA, RENDER, W, TNSR (Binance
feeds; mainnet metadata via `bootstrap/scenario/tokens-assets.json`), each
on all three venues against USDC plus an Orca pool against JUP; it needs
`KEYPAIR_TOKEN_<NAME>_MINT` / `PUBKEY_TOKEN_<NAME>_MINT` and
`SCENARIO_TOKENS_CONFIG=bootstrap/scenario/tokens-assets.json`.

`scale.py scale-x10` regenerates only the named overlays. Prices come from
Binance at generation time; where it is unreachable or geo-blocked, pass
them in: `SCALE_PRICES='{"JUP": "0.32", "RAY": "1.84"}'`.

Measure a scale (10 minutes; writes `.scenario/runs/<name>-{t0,t1,load,report}.json`):

```sh
NO_DNA=1 just scenario-surfpool-offline        # fresh chain per scale (no testnet fallback)
SCENARIO_SCALE=x10 just scenario-dev           # deploy + up
SCENARIO_SCALE=x10 bootstrap/scenario/scenario.sh measure x10 600   # or: just scenario-measure dev local x10 600 x10
python3 -I bootstrap/scenario/summarize.py .scenario/runs x1 x10 x100 assets
```

Always start Surfpool with `--disable-instruction-profiling` (both
`scenario-surfpool*` recipes do; `--ci` also implies it). With profiling on,
Surfpool pins one core and its memory grows with history: at x10 it lands
4.3 TPS instead of 13 (RPC p95 7.4 s instead of 1 ms), and the x100
bootstrap crawls at ~2 TPS.

`measure` freezes flow, arb, and mm (`SIGSTOP`) while each snapshot reads,
so every wallet, seat, and position is read in one chain state; without it,
heavy flow moves value between participants mid-read and PnL stops summing
to (minus) the protocol fees. Before each snapshot it waits
`SCENARIO_SETTLE_S` for in-flight transactions to land: 3 s locally, 90 s on
testnet, where the RPC node rebroadcasts until the blockhash expires. They
stay frozen through the report, whose block scan would otherwise compete
with flow for connections. `sample.py` records per-service CPU/RSS (from
`.scenario/pids`), machine CPU, validator TPS (`getTransactionCount`), slot
rate, and RPC latency; against a shared cluster that TPS is network-wide
(`tpsScope`), so use the report's per-participant counts there. The report
counts transactions from the window's blocks. Use the offline
Surfpool for scale runs: the forking one fetches every missing account
(uninitialized tick arrays, never-funded ATAs) from testnet, which turns
snapshot loads into multi-second stalls.

### solana-test-validator

A local Agave validator runs the same scenario with a real banking stage,
so it is the closer stand-in for testnet. It also scales much further:
Surfpool saturates near 105 TPS, while the test validator still lands
~95% of the offered load at x200 (261 TPS) with sub-millisecond RPC:

```sh
just scenario-test-validator                   # Agave >= 4.3 on PATH; fresh ledger
just scenario-test-validator-fund              # program authority (Surfpool airdrops it)
SCENARIO_SCALE=x100 just scenario-dev          # deploy + up, as with Surfpool
SCENARIO_SCALE=x100 bootstrap/scenario/scenario.sh measure tv-x100 600
```

Differences from Surfpool that matter when reading the numbers:

- **Votes.** The validator lands one vote transaction per slot. The report
  skips vote transactions (`network.voteTransactionsExcluded`) and
  `sample.py` subtracts one per slot from `getTransactionCount`, so TPS is
  non-vote on both.
- **Failures land.** Surfpool executes each transaction when it is sent, so
  anything that would fail is rejected at preflight. On Agave, preflight
  runs against the last bank: racing arb bots, slipped swaps, and LP
  re-centres that a competitor front-runs within the slot land _failed_
  and pay fees, as on testnet (~13% of landed transactions at x100).
- **Commitments differ.** Finalized lags confirmed by ~32 slots (~13 s),
  so every read in a snapshot must use one commitment (the client's,
  confirmed); Surfpool treats all commitments alike and hides mixing.
- **Genesis.** SIMD-0500 (no new SBPF v0–v2 deploys) is active at
  test-validator genesis but not on testnet, and the Raydium and Orca forks
  are v0 builds, so the recipe deactivates it. Metaplex comes from the same
  snapshot via `--account-dir` (`metaplex-snapshot.py --account-dir`, which
  zeroes the ProgramData deployment slot: a testnet slot is in a fresh
  chain's future and keeps the program from loading).

Measured 2026-10-13 on a 16-core / 125 GB machine, 10-minute windows,
Surfpool 1.5.0 (`--ci`, offline) vs solana-test-validator 4.3.0 (tmpfs
ledger). TPS is non-vote, landed (ok + failed on chain):

| scale  | offered swaps/s | Surfpool TPS | test-validator TPS (peak 10 s) | failed on chain (S / TV) | RPC p95 (S / TV) | validator CPU / RSS (S / TV)  |
| ------ | --------------- | ------------ | ------------------------------ | ------------------------ | ---------------- | ----------------------------- |
| x1     | 0.3             | 0.6          | 0.6 (2.0)                      | 1 / 5                    | 1 ms / 0.6 ms    | 2% · 1.0 GB / 129% · 1.2 GB   |
| x10    | 12              | 13.0         | 13.8 (16.0)                    | 0 / 592                  | 1 ms / 0.7 ms    | 21% · 0.8 GB / 145% · 1.3 GB  |
| x100   | 137             | 105.3        | 138.5 (162.5)                  | 73 / 9 759               | 696 ms / 1.7 ms  | 210% · 4.8 GB / 247% · 1.7 GB |
| assets | 137             | 48.4         | 133.0 (140.2)                  | 31 / 10 615              | 1.5 s / 1.6 ms   | 174% · 6.7 GB / 250% · 1.9 GB |
| x200   | 275             | —            | 268.8 (321.1)                  | — / 4 776                | — / 1.6 ms       | — / 276% · 1.7 GB             |

The test validator idles at ~1.3 cores (block production, votes) where
Surfpool idles near zero, but its cost barely grows with load and its
memory stays flat (Surfpool keeps history in RSS). x200 did not saturate
it; the next bottlenecks on this machine are the router (~1 core at x200)
and flow's per-swap RPC round trips.

#### Test-validator ceiling (x200 → x2000)

Same machine, 2026-10-13, fresh chain per row. From x1000 up the router
runs with `MTM_ROUTER__SNAPSHOT_TTL_MS=400` (without it quotes average 5 s
at x1000 and the router caps the run at 750 TPS):

| scale       | offered swaps/s | validator TPS mean (max) | successful swaps/s | tx / block (mean, max) | RPC p95 | validator CPU · RSS |
| ----------- | --------------- | ------------------------ | ------------------ | ---------------------- | ------- | ------------------- |
| x200        | 275             | 269 (288)                | 259                | —                      | 1.6 ms  | 276% · 1.7 GB       |
| x500        | 690             | 661 (1051)               | 633                | —                      | 1.9 ms  | 339% · 1.8 GB       |
| x1000 + TTL | 1375            | 1284 (1545)              | 1220               | 531, 665               | 2.5 ms  | 380% · 2.0 GB       |
| x2000 + TTL | 2750            | 1357 (2204)              | 1332               | 554, 860               | 785 ms  | 403% · 2.3 GB       |

The scenario tops out at ~1.3k successful swaps/s on this machine: doubling
the offered load from x1000 to x2000 adds 9%. The validator is not what
saturates (4 of 16 cores, ~29M of the 100M block CU limit, 1% of landed
transactions failed). The limits, in order:

- **Pool contention.** All flow goes through 7 pools (~190 swaps per pool
  per second at x2000), so a quote is stale within a slot: a quarter of
  x2000's swaps are rejected at preflight on slippage, and LP re-centres
  fail on chain, so books thin over a long run (a chain kept under x1000
  load for an hour lands 40% of its transactions failed).
- **Client transport.** Each taker holds an HTTP/1.1 connection while it
  polls for confirmation: flow opens ~24k sockets to the RPC port at x2000,
  which exhausts the ephemeral port range (`EADDRNOTAVAIL`) and pushes RPC
  p95 to ~0.8 s. Batched `getSignatureStatuses` (or TPU/QUIC sends) would
  be needed to find the validator's own ceiling.
- **Ledger.** At ~1k TPS the blockstore grows ~0.5 GB/min; keep it capped
  (`--limit-blockstore-size 5000000` retains ~40 min at x2000, enough for a
  report window) or a tmpfs quota stops the validator.

## Teardown

`scenario.sh teardown` (`just scenario-teardown-dev` / `-testnet`) recovers
the scenario's SOL into the faucet:

1. stops oracle, router, makers, flow, and arb (Prometheus/Grafana stay);
2. unwinds every maker: CLMM positions are emptied, their fees collected,
   and closed (position, NFT mint, and NFT account rent come back);
   Manifest makers cancel their orders and withdraw their seat balances;
3. sweeps every participant wallet (makers, arb bots, takers): scenario
   tokens are burned, every token account is closed with its rent sent to
   the faucet, and the remaining SOL follows, leaving each wallet at 0.
   Token accounts holding anything else (e.g. a position NFT whose close
   failed) are left open and listed instead of burned;
4. with `SCENARIO_CLOSE_PROGRAMS=1`, closes the three DEX programs
   (programdata rent to the faucet; the pools stop working);
5. sweeps the operator wallets (program / mint / freeze / metadata
   authorities) into the faucet: by default on `local`, opt-in on testnet
   (`SCENARIO_SWEEP_OPERATORS=1`), where those keys are shared;
6. writes `.scenario/teardown.json` (what moved) and
   `.scenario/leftovers.json` (`scenario leftovers`: the rent still locked,
   by owner program and account type).

Every step is idempotent, so a teardown can be re-run after a partial
failure; `scenario leftovers` can also be run on its own at any time.

What stays locked has no close instruction in the forked programs (or is a
mint): pools, tick arrays, observation and bitmap accounts, venue configs,
Manifest markets, pool/market vaults, the mints and their Metaplex
metadata, and the 36-byte stubs of closed programs. Measured on the x2000
chain (33 086 participants, 3 tokens, 7 pools) on 2026-10-13:

| recovered into the faucet                   | SOL       |
| ------------------------------------------- | --------- |
| participant wallets (33 086, all left at 0) | 16 715.19 |
| token-account rent (99 222 accounts closed) | 202.34    |
| operator wallets                            | 200 000   |
| programdata (`SCENARIO_CLOSE_PROGRAMS=1`)   | 20.29     |

| still locked                                               | accounts | SOL   |
| ---------------------------------------------------------- | -------- | ----- |
| Orca tick arrays                                           | 14       | 0.986 |
| Raydium tick arrays                                        | 10       | 0.722 |
| Raydium observation + tick-array bitmap accounts           | 4        | 0.091 |
| pools (Orca Whirlpool, Raydium PoolState)                  | 5        | 0.040 |
| Manifest markets                                           | 2        | 0.038 |
| pool / market vaults                                       | 14       | 0.029 |
| mints + Metaplex metadata                                  | 6        | 0.051 |
| venue configs (Orca config + fee tier, Raydium AMM config) | 3        | 0.005 |
| closed-program stubs                                       | 3        | 0.003 |
| **total** (20.30 more if the programs are kept)            |          | 1.964 |

Tick arrays dominate (~0.07 SOL each); they grow with the ranges LPs
open, so the asset scale (39 pools) locks proportionally more.

## Doppler variables

| Variable                                                                                                                                                     | Used by                                                                                                            |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------ |
| `KEYPAIR_TOKEN_{BONK,WIF,PYTH,JTO,ORCA,RENDER,W,TNSR}_MINT`, `PUBKEY_TOKEN_*_MINT` **(new, asset scale only)**                                               | mint keypairs for the added assets                                                                                 |
| `SCENARIO_SEED` **(new)**                                                                                                                                    | secret all maker/arb/taker wallets derive from (`prd` required; `dev` optional — the local profile has a dev seed) |
| `KEYPAIR_ORCA_WHIRLPOOL`, `KEYPAIR_ORCA_WHIRLPOOLS_CONFIG`, `KEYPAIR_MANIFEST`                                                                               | from the DEX branches                                                                                              |
| `KEYPAIR_RAYDIUM_CLMM`, `KEYPAIR_FAUCET`, `KEYPAIR_PROGRAM_AUTHORITY`, `KEYPAIR_TOKEN_*`, `PUBKEY_TOKEN_*_MINT`, `PUBKEY_FAUCET`, `PUBKEY_PROGRAM_AUTHORITY` | existing                                                                                                           |

## Metrics

| Metric                                                                                         | Labels                      | Meaning                                                                                                        |
| ---------------------------------------------------------------------------------------------- | --------------------------- | -------------------------------------------------------------------------------------------------------------- |
| `mm_oracle_price`                                                                              | pair                        | oracle pair price the makers use                                                                               |
| `mm_venue_price`, `mm_deviation_bps`                                                           | maker, venue                | venue price and its gap to the oracle                                                                          |
| `mm_inventory`, `mm_inventory_value_usd`                                                       | maker, token                | holdings (wallet + seat) and USD value                                                                         |
| `mm_actions_total`                                                                             | maker, action               | requote / open / close / arb_buy / arb_sell                                                                    |
| `mm_errors_total`, `mm_panics_total`, `venue_quote_panics_total`                               | maker, stage / venue        | failures (ticks retry)                                                                                         |
| `mm_skew_bps`, `mm_arb_volume_micro_usd_total`, `mm_tick_seconds`                              | maker                       | skew, arbitrage volume (µUSD), tick latency                                                                    |
| `flow_swaps_total`                                                                             | pair, status, outcome, size | routed swaps landed/failed, why, normal/large                                                                  |
| `flow_venue_swaps_total`                                                                       | venue, status, outcome      | quoted swaps' send outcomes, once per venue the route touches                                                  |
| `flow_fee_lamports_total`, `flow_fill_read_errors_total`                                       | —                           | fees paid by takers; landed swaps with unread fills                                                            |
| `flow_takers_alive`, `flow_taker_restarts_total`                                               | —                           | taker loops running / restarted after an error                                                                 |
| `flow_cost_vs_oracle_bps`, `flow_slippage_bps`                                                 | pair                        | cost to uninformed flow vs oracle; landed vs quoted                                                            |
| `flow_route_legs_total`, `flow_notional_micro_usd_total`, `flow_refills_total`                 | venue / pair                | routing mix, volume (µUSD), refills                                                                            |
| `router_quote_seconds`, `router_quote_errors_total`, `router_quotes_total`                     | execution, status           | router latency and failures                                                                                    |
| `oracle_price_updates_total`, `{orca_whirlpool,raydium_clmm,manifest}_actions_total`           | —                           | feed health and per-venue transactions                                                                         |
| `arb_trades_total`                                                                             | bot, status, legs           | landed / reverted / expired / ambiguous / send_failed / below_floor / simulation_failed / quote_failed / error |
| `arb_opportunities_total`, `arb_last_profit_usd`                                               | bot, legs, trigger          | cycles found, last realized PnL                                                                                |
| `arb_profit_micro_usd_total`, `arb_loss_micro_usd_total`, `arb_fees_lamports_total`            | bot                         | realized gains and losses (all touched tokens), fees                                                           |
| `arb_in_flight`                                                                                | —                           | cycles sent and not yet resolved                                                                               |
| `arb_scans_total`, `arb_scan_seconds`, `arb_pool_updates_total`, `arb_snapshot_failures_total` | trigger / reason            | search cadence and latency, ws updates, slow reloads                                                           |
