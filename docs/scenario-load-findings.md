# Scenario load testing: findings and lessons

Results of scaling the market-making scenario (oracle, router, makers, uninformed
flow, arb searchers) from 17 to 33,086 participants on Surfpool and on
`solana-test-validator`, measured 2026-10-10 to 2026-10-13. How to run it is in
`bootstrap/scenario/README.md`. This document covers what the runs taught us
and what they mean for testnet.

## TL;DR

- **Surfpool is a functional simulator, not a performance target.** It caps at
  ~105 TPS with 7 pools and ~48 TPS with 39 pools, and its RPC latency rises
  to seconds. It also hides the failure modes of a real banking stage.
- **A local Agave validator (`solana-test-validator` 4.3) is the right stand-in
  for testnet.** With the same scenario it lands every offered transaction up
  to x500 (~660 TPS) with sub-2 ms RPC latency.
- **On this machine the scenario tops out at ~1.3k successful swaps/s**
  (x1000: 1,220; x2000: 1,332). That ceiling is ours, not the validator's: the
  validator uses ~4 of 16 cores and ~29M of the 100M block CU. The limits are
  pool contention (7 pools), per-taker HTTP connections exhausting ephemeral
  ports, and (until cached) router quote cost.
- **Moving off Surfpool exposed two bugs in our code that only appear on a real
  validator:**
  - mixed commitments in the PnL snapshot;
  - the semantics of failed transactions (they land and pay fees).
- **The next scale steps are client-side:**
  - batched confirmations;
  - TPU/QUIC sends;
  - more pools;
  - priority fees;
  - an open-loop load generator;
  - then more machines and IPs.

## Setup

- **Machine:** 16 cores, 125 GB RAM, tmpfs `/tmp`. A Docker stack and a VM ran
  alongside on the same host.
- **Chains:**
  - Surfpool 1.5.0: `--ci`, `--offline`, Metaplex from a snapshot.
  - `solana-test-validator` 4.3.0: tmpfs ledger, SIMD-0500 deactivated,
    Metaplex via `--account-dir`, token-owner/mint account indexes.
- **Scales** (`bootstrap/scenario/scale.py`): takers grow by orders of
  magnitude while makers stay roughly fixed (12 to 36; 92 on the asset scale).
  Maker capital grows instead.

  | Scale  | Takers | Arb bots | Pools          |
  | ------ | ------ | -------- | -------------- |
  | x1     | 4      | 1        | 7              |
  | x10    | 140    | 6        | 7              |
  | x100   | 1,650  | 14       | 7              |
  | assets | 1,650  | 14       | 39 (11 tokens) |
  | x200   | 3,300  | 20       | 7              |
  | x500   | 8,250  | 30       | 7              |
  | x1000  | 16,500 | 40       | 7              |
  | x2000  | 33,000 | 50       | 7              |

  Every taker trades at 5 swaps/min (Poisson arrivals).

- **Measurement** (`scenario.sh measure NAME 600`):
  - a frozen snapshot at the start of the window, with participants paused
    (SIGSTOP) while it reads;
  - 10 minutes of sampling, recording per-service CPU/RSS, machine CPU,
    `getTransactionCount` TPS, slot rate and `getLatestBlockhash` latency;
  - a frozen snapshot at the end;
  - a report built from the window's blocks with PnL, transactions per signer
    and Prometheus trade counters.
- **TPS is non-vote.** The test validator lands one vote per slot, and the
  report and sampler subtract them.
  - "Validator TPS" counts landed transactions, including those that fail on
    chain.
  - "Successful swaps/s" counts flow swaps that landed and succeeded.

## Results

### Surfpool vs test validator (same scenario, 10-minute windows)

| Scale  | Offered swaps/s | Surfpool TPS | TV TPS (10 s peak) | Failed on chain (S / TV) | RPC p95 (S / TV) | Validator CPU, RSS (S / TV) |
| ------ | --------------- | ------------ | ------------------ | ------------------------ | ---------------- | --------------------------- |
| x1     | 0.3             | 0.6          | 0.6 (2.0)          | 1 / 5                    | 1 ms / 0.6 ms    | 2%, 1.0 GB / 129%, 1.2 GB   |
| x10    | 12              | 13.0         | 13.8 (16)          | 0 / 592                  | 1 ms / 0.7 ms    | 21%, 0.8 GB / 145%, 1.3 GB  |
| x100   | 137             | 105          | 138.5 (162)        | 73 / 9,759               | 696 ms / 1.7 ms  | 210%, 4.8 GB / 247%, 1.7 GB |
| assets | 137             | 48           | 133 (140)          | 31 / 10,615              | 1.5 s / 1.6 ms   | 174%, 6.7 GB / 250%, 1.9 GB |

### Test-validator ceiling

| Scale                   | Offered swaps/s | Validator TPS mean (max) | Successful swaps/s | Tx per block (mean, max) | RPC p95 | Validator CPU, RSS |
| ----------------------- | --------------- | ------------------------ | ------------------ | ------------------------ | ------- | ------------------ |
| x200                    | 275             | 269 (288)                | 259                | —                        | 1.6 ms  | 276%, 1.7 GB       |
| x500                    | 690             | 661 (1,051)              | 633                | —                        | 1.9 ms  | 339%, 1.8 GB       |
| x1000 (no router cache) | 1,375           | 750 (893)                | 733                | —                        | 7.4 ms  | 380%, 2.2 GB       |
| x1000 + router cache    | 1,375           | 1,284 (1,545)            | 1,220              | 531, 665                 | 2.5 ms  | 380%, 2.0 GB       |
| x2000 + router cache    | 2,750           | 1,357 (2,204)            | 1,332              | 554, 860                 | 785 ms  | 403%, 2.3 GB       |

Other observations:

- **PnL conservation held at every scale.** The total trading PnL equals minus
  the protocol fees, within ~1–3% of taker losses.
- **Takers lose; LPs, CLOB makers and arbitrageurs gain**, as designed.
- **Fees:** at ~1.3k TPS the participants spent ~4.3 SOL per 10 minutes, about
  25 SOL/hour, at 5,000 lamports per signature and no priority fees.
- **Observability barely moved.** Grafana stayed at 224→227 MiB and Prometheus
  at 80→~99 MiB (peaks of 139 MiB during compaction), with 51 MB on disk and
  <0.4% CPU. Series grow with pools and makers, not with takers.

## What moving off Surfpool taught us

### Surfpool hides real-validator behaviour

| Behaviour        | Surfpool                                                                    | Agave (test validator, testnet)                                                                                                  |
| ---------------- | --------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| Execution        | Each transaction executes synchronously when sent, so preflight = execution | Preflight runs against the RPC's last bank; execution happens later in a leader's block                                          |
| Losing a race    | Rejected at preflight, never lands, pays nothing                            | Lands **failed** and pays the fee: racing arb bots, slipped swaps, front-run LP re-centres (~13% of landed transactions at x100) |
| Commitments      | processed = confirmed = finalized                                           | Finalized lags confirmed by ~32 slots (~13 s)                                                                                    |
| Votes            | None                                                                        | One per slot from the local validator (thousands per slot on a real cluster)                                                     |
| Throughput cap   | Internal serialization at ~2 cores; RPC latency 0.2–2 s                     | Not reached by this harness; RPC stays at a few ms until the client side saturates                                               |
| Memory           | Grows with history (5–7 GB per 10 minutes at x100)                          | Flat at ~1.7–2.3 GB                                                                                                              |
| Idle cost        | ~0                                                                          | ~1.3 cores (block production, votes, PoH)                                                                                        |
| Genesis features | Permissive                                                                  | Current Agave defaults (e.g. SIMD-0500 blocks new SBPF v0–v2 deploys)                                                            |

### Bugs only a real validator exposed

1. **Mixed commitments in the PnL snapshot.** Position NFTs were discovered
   with a raw `getTokenAccountsByOwner` and no commitment, so the request
   defaulted to finalized. Every other read used confirmed.
   - On Agave, positions opened in the last ~13 s were invisible and
     recently closed ones still listed.
   - One run showed a bogus +$303k total PnL.
   - Fix: every snapshot read uses the client's commitment.
2. **Failure accounting.** On Surfpool "arb reverted" meant a preflight
   rejection. On Agave most reverts are on-chain failures that cost fees. The
   reporting counts both, but strategy economics (expected value per attempt)
   now has to include the fees of lost races.

## How this translates to testnet

What carries over:

- **Contention on hot pools.** Write locks on a pool serialize swaps, and a
  quote goes stale within a slot when a pool takes dozens of swaps per slot.
  The same thing happens on testnet. Testnet adds other users' traffic and
  the per-account CU limit (40% of the block with SIMD-0306), which a single
  hot pool can hit.
- **Failed transactions land and pay fees.** Budget SOL for failures, not just
  successes.
- **Commitment discipline.** Read everything at one commitment (confirmed),
  and account for confirmed→finalized lag when reconciling.
- **Block cost model.** The scheduler packs by _requested_ CU plus signature,
  write-lock and loaded-data costs. Our swaps request 150k–280k CU and use
  ~55–110k (p99 76k for Orca, 102k for Raydium). On a busy cluster,
  over-requesting lowers our priority and packing.

What will be different, or worse:

- **Transaction delivery.** Locally RPC and leader are the same process. On
  testnet:
  - `sendTransaction` forwards to the current and next leaders over QUIC;
  - transactions can be dropped, and there is no mempool;
  - blockhashes expire after ~150 slots (~60 s).
    So clients need resend logic, and above a few hundred TPS direct TPU/QUIC
    sends.
- **Stake-weighted QoS.** Leaders give unstaked connections a small share of
  TPU bandwidth. One unstaked IP will be throttled well below what we pushed
  locally. Sustained high rates need a staked RPC or validator connection
  (SWQoS) or several sending hosts.
- **Public RPC limits.** `api.testnet.solana.com` rate-limits per IP, far
  below what flow sends (several reads per swap plus confirmation polling).
  Use a dedicated RPC (`SCENARIO_RPC_URL`), which also has to handle our
  `getBlock` / `getTokenAccountsByOwner` reads (token-owner index).
- **Preflight against a lagging bank.** The RPC node simulates against its own
  view, which may trail the leader by a slot or more, so expect more on-chain
  failures than the local run.
- **Latency.** Confirmation takes ~1–2 s rather than one local slot. With
  closed-loop takers that alone cuts the achieved rate (see the footguns).
- **Block space is shared.** Votes from the whole validator set and other
  users' traffic compete with ours, and makers without priority fees starve
  under load. That starvation already happened locally at x1000.
- **Cost and visibility.** ~25 SOL/hour at 1.3k TPS before priority fees and
  failures, and hundreds of TPS from one stack is visible load on shared
  infrastructure. Coordinate before load runs.
- **Features differ.** Check which cost-model and block-limit features are
  active on testnet (`solana feature status -ut`) before comparing numbers.
  Locally SIMD-0286 (100M block) and SIMD-0306 (account limit at 40%) were
  active at genesis.

## Bottlenecks, in the order we hit them

1. **Surfpool instruction profiling.** With profiling on it pins one core and
   lands 4.3 TPS at x10 (13 with it off). `--ci` implies the disable flag.
2. **Surfpool forking mode.** Every missing account (uninitialized tick
   arrays, unfunded ATAs) is fetched from testnet, which stalls snapshot loads
   by 0.3–10 s. Use `--offline` with a snapshot.
3. **Surfpool's internal ceiling:** ~105 TPS with 7 pools, ~48 TPS with
   39 pools, RPC p95 up to 1.5 s.
4. **`getSignaturesForAddress` is linear in history** (17 s per call at ~90k
   transactions). The report scans blocks with `getBlock` instead (~40 ms per
   block).
5. **Snapshot reads.** Sequential reads took >20 minutes under load. Batched
   balances (chunks of 100, 16 concurrent) and concurrent maker reads brought
   that to ~30 s at x100 and tens of seconds at 33k wallets.
6. **Flow on one core.** `join_all` over thousands of taker loops runs them on
   one task. Fix: shard takers across OS threads, one current-thread runtime
   each (the signer types are not `Send`).
7. **Router quote cost.** With `snapshot_ttl_ms = 0` every quote reloads every
   pool. At x1000 that meant a 5 s mean quote and 750 TPS. A 400 ms cache
   gave 25 ms quotes and 1,284 TPS.
8. **Pool contention.** At ~190 swaps per pool per second (x2000), a quarter
   of swaps fail preflight on slippage. LP re-centres fail on chain, so books
   thin and failures snowball: a chain kept under x1000 load for an hour
   landed 40% failed transactions.
9. **Client sockets.** Each waiting taker holds an HTTP/1.1 connection, about
   24k sockets to one RPC port at x2000. That exhausts the ephemeral port
   range (32768–60999), causing `EADDRNOTAVAIL` and RPC p95 of ~0.8 s.
10. **Ledger growth.** ~0.5 GB/min at ~1k TPS. A 20M-shred cap let the ledger
    reach 26 GB and hit the tmpfs quota after ~55 minutes, which killed the
    validator. 5M shreds keeps ~40 minutes at x2000.

Not a bottleneck yet: validator CPU (≤4.5 cores), block CU (~30M of 100M),
machine RAM, Prometheus and Grafana.

## How to scale further

Prioritized by expected gain per effort.

1. **Batch confirmations.** Replace per-transaction `send_and_confirm` polling
   with a shared confirmer per process: one `getSignatureStatuses` per slot
   for up to 256 signatures, or `signatureSubscribe`. This cuts sockets and
   RPC calls by an order of magnitude and is required to find the validator's
   own ceiling.
2. **Send over TPU/QUIC** (the connection cache / TPU client) rather than RPC
   `sendTransaction`. That is mandatory for high rates on testnet, ideally
   through a staked identity (SWQoS).
3. **Trim per-swap RPC.** Today each swap does an affordability balance read,
   two balances before, the send, the confirmation polls and two balances
   after. Derive fills from transaction metadata (`postTokenBalances`) and
   keep balances in memory.
4. **Spread the flow.** Add pools and assets, and weight pairs, so no single
   pool takes more than tens of swaps per slot. That is closer to real markets
   and avoids the stale-quote spiral.
5. **Priority fees and tight CU limits.** Set CU limits from simulation
   (p99 + 10%, not estimate + 50k) and pay makers a priority fee, so quotes
   and re-centres land in full blocks. Consider dynamic fees per route.
6. **Open-loop arrivals.** Takers currently wait for each trade before the next
   arrival (closed loop), so rising latency silently lowers offered load.
   Generate arrivals on a schedule and record offered, attempted and landed
   separately.
7. **Router at scale.** Keep the snapshot cache on (≥400 ms) for load. For
   more, stream pool accounts (`accountSubscribe`, or Geyser) instead of
   polling, and run several router replicas behind a load balancer.
8. **Horizontal load.** Several flow processes, hosts and source IPs, sharded
   by taker index, against several RPC nodes. One host's ephemeral ports cap
   it at ~28k concurrent connections per destination.
9. **Fresh state per measurement.** Reset the chain (or at least the pools and
   makers) for each measurement window, or measure degradation explicitly.

## Footguns and roadblocks

Chain and tooling:

- **Surfpool `--disable-instruction-profiling`** (or `--ci`) is mandatory for
  any long or loaded run.
- **`surfnet_exportSnapshot` omits program accounts.** Resetting means a fresh
  chain plus redeploy.
- **Manifest is SBPF v3**: deploy it with an Agave ≥ 4.x CLI.
- **SIMD-0500** is active at test-validator genesis, which blocks deploying the
  Raydium and Orca forks (SBPF v0). Deactivate it with `--deactivate-feature`.
- **A ProgramData account copied from testnet** keeps its deployment slot. On a
  fresh chain that slot is in the future, so the program never loads, and the
  error surfaces as `ProgramCacheHitMaxLimit` in token bootstrap. Zero the
  slot.
- **Votes count toward `getTransactionCount` and `getBlock`** on a real
  validator. Subtract them before comparing.
- **Raw JSON-RPC calls without `commitment`** default to finalized. Always set
  it explicitly.
- **v1 transactions carry the CU limit in the message config**, not in a
  ComputeBudget instruction. Tooling that parses ComputeBudget instructions
  silently reads the 200k default; our first block-cost analysis made this
  mistake.
- **The ledger needs a cap** (`--limit-blockstore-size`), especially on tmpfs
  with a quota.

Harness:

- **Closed-loop load hides saturation.** Compare offered against attempted
  rates.
- **Long runs degrade.** Pools drift and LPs fail to re-centre, so compare
  fresh chains.
- **Restarting the router re-vets pools.** Manifest books that are empty
  because their makers starved stay quarantined.
- **Low-priced tokens** (BONK at ~$3e-6, 5 decimals) need an escalating
  discovery probe, or the router quarantines their pools.
- **Thin Binance books** (RENDER) can exceed a 30 s oracle staleness limit; the
  asset scale uses 120 s.
- **Funding transactions with many tokens exceed the transaction size limit.**
  Chunk them (9 instructions per transaction).
- **PnL snapshots must freeze participants** (SIGSTOP plus a settle period)
  and read each maker optimistically (wallet, venue, wallet again).
  Otherwise mid-recenter state shows up as six-figure fake PnL.
- **LP valuation must include accrued fees**
  (`fee_growth_inside − checkpoint`), or LP PnL is understated (x1 total went
  from −$346 to −$9 once fees were counted).
- **A pool can become impossible to LP.** The BONK/JUP pool drifted far from
  the oracle before its LP opened, and the range then needed 2.5× the LP's
  JUP.

Operations:

- **`pkill -f`** matches the invoking shell. Kill by PID from `.scenario/pids`.
- **Background shells die with the session.** Start long measurements with
  `setsid nohup`.
- **Port exhaustion affects every process on the host**, including the report
  run after a window. Stop flow, or run the report from another host.
- **Keep scenario ports off the always-on Docker stack** (8080/9090/3000). The
  scenario uses 8180–8184, 9190 and 3100.

## Caveats

- Everything ran on one shared host: services, validator, Prometheus and
  Grafana competed for the same CPUs and ports, alongside other workloads.
- The test validator is a single node: no turbine, no leader rotation, no
  network latency, no QUIC QoS. Its numbers are an upper bound on what one
  validator's banking stage did _for this workload_, not a testnet forecast.
- We never saturated the validator itself. Its real ceiling for this workload
  is above ~1.4k TPS sustained (2.2k in 5-second samples) and unknown.
- All runs were 10-minute windows, one sample per configuration, so expect
  run-to-run variation of roughly ±10%.

## Long runs: 30 min, 1 h, 2 h (2026-10-15)

Base scenario (17 participants) on solana-test-validator, each run on a
fresh chain: deploy → bootstrap → `measure` → janitor teardown with the
programs closed. Raw outputs: `.scenario/runs/long-*`.

|                                             | 30 min            | 1 h               | 2 h               |
| ------------------------------------------- | ----------------- | ----------------- | ----------------- |
| transactions landed / failed                | 976 / 7           | 2 325 / 17        | 4 361 / 34        |
| landed TPS mean / peak                      | 0.54 / 1.7        | 0.65 / 3.2        | 0.61 / 2.9        |
| flow swaps landed / failed (of which large) | 546 / 10 (9)      | 1 067 / 28 (26)   | 2 192 / 75 (70)   |
| taker PnL excluding refills                 | −$2 278           | −$3 056           | −$8 790           |
| refills minted inside the window            | $18 182           | $84 577           | $111 803          |
| transaction fees, whole lifecycle           | 0.0384 SOL        | 0.0454 SOL        | 0.0560 SOL        |
| rent locked after teardown                  | 0.0547 SOL        | 0.0547 SOL        | 0.0547 SOL        |
| mm / router RSS, first → last sample        | 24→31 / 19→27 MiB | 24→37 / 18→28 MiB | 26→41 / 20→28 MiB |

What the length showed that short runs do not:

- **Liquidity drifts one-sided.** Takers keep selling base into the makers,
  and refills keep minting them more of it ($112 k in 2 h). Nothing
  rebalances the makers: after 2 h both CLOB makers and all five oracle
  arbitrageurs held ~$0 of USDC, and two LPs (`lp-orca-jup`,
  `lp-raydium-jup`) had ~$0.1–0.5 k left in their venues (one-sided
  wallets cap the next position). Large-trade quote failures tripled
  (~5 → 16–19 per 30 min). Runs longer than ~1 h need maker rebalancing
  (or inventory-neutral flow) to measure a steady state.
- **The ledger needs sizing for the window, not the load.** At ~1 user TPS
  the test validator writes ~6.5 GB/h (votes and ticks dominate): a
  2M-shred cap pruned the start of a 30-minute window, and the report
  correctly refused to count from missing blocks. 12M shreds on disk held
  2 h (13 GB).
- **Refills distort PnL.** One refill reads as tens of thousands of dollars
  of taker "profit"; the report now warns reliably (the refill counter is
  registered at zero), but long runs should either disable refills or
  subtract them.
- **SOL is spent by Raydium LPs, not by trading.** Re-centres pay tick-array
  and position rent (up to ~0.22 SOL/h for one LP); every other participant
  spent < 0.005 SOL/h in fees. The janitor recovers that rent at teardown.
- **Fees are small and linear:** ~0.03 SOL fixed (deploys, bootstrap,
  teardown) plus ~0.012 SOL per hour of running.
- **Teardown is boring, as it should be:** every run ended with 14 vaults and
  32–36 program accounts closed by the janitor, every wallet at 0 lamports,
  and exactly the 0.0547 SOL floor locked (mints, metadata, program stubs).
- **Memory is flat enough:** mm grows ~7 MiB/h and slowing, the router
  levels at ~28 MiB, the validator at ~1.1 GB; no leak at this horizon.
- **Transient on-chain races, all retried:** Orca `AmountOutBelowMinimum`
  (the pool-arb floor) and `TickArraySequenceInvalidIndex` (a swap crossing
  past its tick arrays), and a Manifest `Custom(10)` (a stale cancel index
  hint, the open Manifest item).
