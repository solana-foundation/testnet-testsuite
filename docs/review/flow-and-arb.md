# Review: flow and arb services

Scope: `services/flow`, `services/arb`.

Reviewed 2026-10-14 at `bc96670`, read-only. Severity is
Critical/High/Medium/Low. Priority runs from P0 (now), P1 (next) and P2
(later) to P3 (nice to have).

## flow

**Purpose and boundary.** Generates uninformed taker load:

- each taker trades on its own seeded Poisson clock, with lognormal notionals;
- it gets a routed transaction from the router over HTTP, signs and sends it;
- it measures slippage and cost against the oracle, and refills test tokens.

Dependencies are clean (`router-client`, `oracle-client`, `mtm-chain`,
`mtm-scenario`). The one smell is `mtm-dex-harness[cli]` (clap etc.), pulled in
only for `signer_from_environment` (`main.rs:20`).

| #   | Severity | Category    | Finding                                                                                                                                                                                                                                                                                                                              | Recommendation                                                                                                                                                                       | Priority | Impact                                                          |
| --- | -------- | ----------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | -------- | --------------------------------------------------------------- |
| 1   | High     | perf        | **Closed loop.** The next arrival is sampled only after `trade()` and `refill()` finish (`main.rs:178-211`). At 5 trades/min per taker (12 s mean think time), a 3-10 s trade cuts offered load by about 20-45%, and nothing reports it.                                                                                             | Open-loop scheduler: arrival events feed a bounded worker queue, and each taker stays serial. At minimum emit `flow_offered_rate` and `flow_lag_seconds`.                            | P0       | The configured load is not what gets tested.                    |
| 2   | High     | perf        | **About 15-20 RPCs per trade:**<br>- 1-2 affordability reads (`223-225`);<br>- 2 balances-before (`262-263`);<br>- `send_and_confirm`, which polls status and `is_blockhash_valid` every 500 ms;<br>- 2 balances-after (`270-272`);<br>- N inventory reads for refill (`312-316`).<br>The input balance is read twice (223 and 262). | Get fills from the transaction's pre/post token balances (one `getTransaction`), reuse the affordability read, and check refill every K trades with the batched `funding::balances`. | P0       | RPC load and socket count are several times higher than needed. |
| 3   | High     | perf        | **No concurrency bound, and one reqwest pool per shard** (`main.rs:112-114`). Every taker can have an HTTP/1.1 request in flight, and every pending confirmation polls on its own. That gives about 24k sockets at x2000 and exhausts the ephemeral ports.                                                                           | A global RPC semaphore, plus a shared confirmation tracker that batches `getSignatureStatuses` (256 signatures per call). Cap the pool.                                              | P0       | Connection collapse at x2000.                                   |
| 4   | High     | correctness | **Quotes go stale before sending.** Two balance RPCs sit between the router quote (`241-253`) and the send (`264-268`), adding at least 2 round trips. This feeds the ~25% preflight slippage rejections at x2000.                                                                                                                   | Read balances before the quote, or not at all (#2); send right after signing.                                                                                                        | P0       | Fewer preflight rejections.                                     |
| 5   | Medium   | correctness | **No error classification.** Every error becomes `status="failed"` (`203-206`): unaffordable, quote errors, preflight rejections, landed-failed, blockhash expiry and transport errors. A status RPC error after a successful send counts as a failure even when the transaction landed.                                             | A typed outcome (`unaffordable`, `quote_failed`, `preflight_rejected`, `landed_failed`, `expired`, `rpc_ambiguous`) used as a metric label.                                          | P1       | Can't tell market effects from infrastructure faults.           |
| 6   | Medium   | correctness | **Balance-after reads aren't pinned to the confirmation slot** (`270-272`). A lagging node reports spent=0 / received=0, which records `slippage_bps=10000` and `cost_bps=0` (`278-287`, `337-342`).                                                                                                                                 | Use transaction meta, or `min_context_slot`.                                                                                                                                         | P1       | Corrupt slippage and cost histograms under load.                |
| 7   | Medium   | correctness | **A taker can die silently.** `join_all` (`165-167`) only surfaces errors after every loop ends, and loops never end. A taker that exits via `?` (e.g. `190`, `174-176`) stops for good with no log.                                                                                                                                 | `try_join_all`, or log and restart; add a `flow_takers_alive` gauge.                                                                                                                 | P1       | Load silently drops.                                            |
| 8   | Medium   | perf        | **Refills contend on shared accounts.** All refills use the same faucet fee payer and write-lock the same mints (`funding.rs:139-168,185-188`). Each refill blocks its taker for a full confirmation (up to 45 s).                                                                                                                   | A separate batching refill worker; mint well above the threshold.                                                                                                                    | P1       | Taker stalls and lower load.                                    |
| 9   | Low      | correctness | **SOL is never checked for refill** (`315`). A taker that runs out of fee SOL fails every trade without ever refilling.                                                                                                                                                                                                              | Include lamports in the check.                                                                                                                                                       | P2       | Long runs with priority fees would stall.                       |
| 10  | Low      | standards   | **f64 notional.** The notional is sampled in f64 (`150,184-187`); NaN or inf becomes a "failed trade". `flow_notional_usd_total` truncates to whole USD (`293-294`).                                                                                                                                                                 | Round to micro-USD right after sampling; document the exception; use a micro-USD counter.                                                                                            | P2       | Rule hygiene; undercounted notional.                            |
| 11  | Low      | correctness | **Arrival and metric details.** `wait.min(600.0)` (`180`) cuts off the exponential tail. Exact-out slippage divides by actual `spent` instead of the quote (`285`).                                                                                                                                                                  | Document the cap; normalize both modes by the quoted amount.                                                                                                                         | P3       | Small metric bias.                                              |
| 12  | Low      | standards   | **Comment doesn't match the cause.** The comment at `86-89` blames "signer types not Send". The real cause is `&dyn Signer` (not `Sync`) held across awaits in `top_up` and `ChainClient::submit`.                                                                                                                                   | Fix those APIs (see the cross-cutting notes) and spawn takers on the multi-thread runtime.                                                                                           | P2       | Removes the bespoke thread sharding.                            |
| 13  | Low      | testing     | No tests at all.                                                                                                                                                                                                                                                                                                                     | Pull out a pure trade planner and test it.                                                                                                                                           | P2       |                                                                 |
| 14  | Low      | ops         | A `warn!` on every failed trade (`204`) floods logs at x2000.                                                                                                                                                                                                                                                                        | Rate-limit, or debug plus metrics.                                                                                                                                                   | P3       |                                                                 |

**Design recommendations**

- **Split the module (M):** `arrivals` (a pure seeded planner), `executor`
  (returns a typed outcome), `accounting` (metrics from transaction meta), and
  `refill` (a separate worker). Today one ~340-line file mixes all of these.
- **Open-loop scheduler with bounded concurrency (M).** Offered load then stays
  independent of latency, and saturation shows up in metrics.
- **Shared chain I/O (M/L):** one confirmation tracker and batched reads for the
  whole process, under one connection pool and semaphore. Consider sending
  without waiting (fire, then track).
- **Fix the Signer API at the source (S).**

## arb

**Purpose and boundary.** A cross-venue cycle searcher:

- websocket subscriptions per pool, debounced into a snapshot cache;
- enumerates 2- and 3-hop cycles and sizes each with a ternary search;
- each of up to 50 in-process bots sends one v1 transaction per cycle.

It quotes the venues directly, not through the router, which is appropriate
for latency. Dependencies are clean.

| #   | Severity | Category         | Finding                                                                                                                                                                                                                                                                                                                    | Recommendation                                                                                                            | Priority | Impact                                                    |
| --- | -------- | ---------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------- | -------- | --------------------------------------------------------- |
| 1   | High     | perf/correctness | **Blind while confirming.** The main loop awaits `scan_and_execute` (`main.rs:113`), which waits on every bot's `send_and_confirm` (`search.rs:307`). Nothing is rescanned until the slowest of the 50 bots confirms or times out.                                                                                         | A pipeline: the scanner produces candidates; executors submit and track confirmations in their own tasks.                 | P0       | Most opportunities are missed in busy periods.            |
| 2   | High     | correctness      | **Bots herd.** All bots share the same snapshot, algorithm and price book (`search.rs:148-158`), so they mostly pick the same cycle and pass simulation. On a real validator the losers land failed and pay fees, which nothing tracks. No priority fee is set (`294-296`), so landing order is arbitrary.                 | Per-bot strategy profiles (latency jitter, priority fee, thresholds, venue subsets); an `arb_fees_lamports_total` metric. | P0       | Wasted failed transactions; unrealistic searcher traffic. |
| 3   | Medium   | correctness      | **Stale cache can stick.** On a failed reload the old snapshot is kept, but that pool's change marker has already been used (`search.rs:129-141`). The full-reload poll only fires after `poll_ms` of no updates, because the sleep is re-created every iteration (`main.rs:109`), so under steady traffic it never fires. | A per-pool dirty flag cleared only on a successful load; a `tokio::time::interval` independent of updates.                | P1       | Phantom or missed cycles.                                 |
| 4   | Medium   | correctness      | **No resync after a websocket reconnect** (`main.rs:121-131`); updates during the gap are lost. Backoff is a fixed 2 s, with one `PubsubClient` per pool (`139`), so all reconnect at once.                                                                                                                                | Mark the pool dirty after resubscribing; jittered backoff; one multiplexed client.                                        | P1       | Stale state after RPC restarts.                           |
| 5   | Medium   | correctness      | **Sizing ignores slippage.** `optimize` maximizes the no-slippage chain (`search.rs:362-407`), but execution chains each leg's `min_out` (`256`) and checks the floor against that. Candidates that pass on paper fail as `below_floor`, and each leg's slippage residue stays in the wallet unvalued.                     | Optimize what actually executes; value the residue.                                                                       | P1       | Wasted scans; misreported profit.                         |
| 6   | Medium   | correctness      | **Losses are dropped from accounting.** A negative `realized_usd` becomes 0 (`search.rs:322-326`). SOL fees are not counted, profit is measured only in the start token, and the before/after reads are unpinned.                                                                                                          | Separate profit and loss counters (micro-USD), fees included, from transaction meta.                                      | P1       | P&L dashboards are always too optimistic.                 |
| 7   | Medium   | correctness      | **Every send error counts as "reverted"** (`search.rs:307-315`). The transaction is also simulated twice: explicitly (`301`), then again by preflight.                                                                                                                                                                     | `skip_preflight` after our own simulation; classify outcomes.                                                             | P1       | Can't tell lost races from faults; extra RPC.             |
| 8   | Medium   | perf             | **Repeated work on one task.** Each bot reads its balance with a sequential RPC on every scan (`search.rs:178`). The search runs separately per bot (`148`). The snapshot `Vec` is cloned twice per scan (`110,141`).                                                                                                      | Cache balances; search once per scan and filter per bot; use `Arc<PoolSnapshot>`.                                         | P2       | Scan latency grows with the number of bots.               |
| 9   | Low      | correctness      | **Ternary search edge cases.** It assumes concavity. The `cap/1000` probe gate (`384-386`) rejects optima below that size, and rounding at small sizes can be non-concave.                                                                                                                                                 | Probe log-spaced points, then ternary-search the best bracket; property tests.                                            | P2       | Small edges are missed.                                   |
| 10  | Low      | correctness      | **Inconsistent quote-failure handling.** The final requote uses `?` (`248-254`) and fails the whole scan, while per-leg failures are soft.                                                                                                                                                                                 | Handle both the same way.                                                                                                 | P3       | Spurious `arb_errors_total`.                              |
| 11  | Low      | correctness      | **A fixed 250 ms debounce** is added to every reaction (`main.rs:99`). The `None` channel branch would busy-loop if all senders were dropped (`107`).                                                                                                                                                                      | Make it configurable per bot; exit on a closed channel.                                                                   | P3       |                                                           |
| 12  | Low      | testing          | The only test covers cycle enumeration.                                                                                                                                                                                                                                                                                    | Test `optimize`, the headroom/floor math and the ATA dedup via a pure planner.                                            | P2       |                                                           |

**Design recommendations**

- **Three-stage pipeline (M):**
  - `StateCache`: subscriptions plus per-pool dirty and slot tracking;
  - `Searcher`: pure, turns snapshots plus a bot policy into ranked
    candidates;
  - `Executor`: build, sign, send, and track confirmations asynchronously per
    bot.

  This removes #1 and makes #5 and #9 unit-testable.

- **Per-bot strategy profiles (S),** so the bots behave like a competitive
  population rather than 50 copies of one bot.
- **Accounting from transaction meta (S):** all tokens plus fees.
- **Shared subscription layer (M):** the account-subscription trait from the
  TODO in mtm-chain (pubsub-ws, later Geyser), multiplexed, with
  resync-on-reconnect. mm and the router can reuse it.

## Cross-cutting notes

- **Both services bypass `ChainClient`.** They call the raw
  `rpc().send_and_confirm_transaction` (flow `264-268`, arb `search.rs:307`),
  which skips `ChainClient::submit`'s typed `SubmissionError` and its
  expiry-aware confirmation, because `submit` doesn't accept a
  `VersionedTransaction`. Add `ChainClient::submit_versioned` with batched
  confirmation tracking: one place for classification, retries, preflight
  policy and metrics.
- **Balance-diff accounting** is duplicated in both services, costs RPCs, races
  across slots and misses fees. Move it to a shared helper built on
  transaction meta (e.g. `mtm-chain::fills`).
- **`&dyn Signer` without `Sync`** in `top_up` and `submit` is why services
  can't use a multi-thread runtime. Fixing it is an S-sized change.
- **Load-fidelity metrics:** offered vs achieved rate, quote-to-send latency,
  and in-flight RPC gauges.
- **Testing:** pure planners (flow trade sampling, arb search and leg
  chaining) are the cheapest route to coverage.
