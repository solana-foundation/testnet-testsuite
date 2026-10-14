# Code review: all applications and crates (2026-10-14)

A senior-engineer review of every hand-written crate and service, covering
coding standards, boundaries, correctness, performance and testability. It
was done read-only at `bc96670` (`feat/test-validator`). There is one report
per application or cohesive group:

| Report                                                 | Covers                                                                                                              |
| ------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------- |
| [oracle-and-core.md](oracle-and-core.md)               | `services/oracle`, `oracle-client`, `mtm-feeds`, `mtm-common`, `mtm-telemetry`, `mtm-math`                          |
| [router-and-venues.md](router-and-venues.md)           | `services/router`, `mtm-venues`, `router-client`                                                                    |
| [mm-and-mtm-scenario.md](mm-and-mtm-scenario.md)       | `services/mm`, `mtm-scenario`                                                                                       |
| [flow-and-arb.md](flow-and-arb.md)                     | `services/flow`, `services/arb`                                                                                     |
| [scenario-and-tooling.md](scenario-and-tooling.md)     | `services/scenario`, `bootstrap/scenario`, `bootstrap/tokens`, `ops/`                                               |
| [raydium-clmm.md](raydium-clmm.md)                     | `raydium-clmm-client`, the Raydium orchestrator (library and CLI), `mtm-math::clmm`                                 |
| [orca-whirlpool.md](orca-whirlpool.md)                 | `orca-whirlpool-client`, the Orca orchestrator (library and CLI)                                                    |
| [manifest-chain-harness.md](manifest-chain-harness.md) | `manifest-dex-client`, the Manifest orchestrator (library and CLI), `mtm-chain`, `mtm-dex-harness`, `sdks/manifest` |

Every finding cites `file:line`. Severity is Critical/High/Medium/Low.
Priority runs from P0 (now), P1 (next) and P2 (later) to P3 (nice to have).
Effort is S, M or L.

The headline findings (both Criticals plus the top P0s) were re-checked
against the code while compiling this index. The rest come from the reviewers'
reading and were spot-checked, not all re-run. Profiling-dependent claims
(router CPU and RSS) are inferred from code paths.

## Status after the fix rounds (2026-10-14)

The findings below were then worked branch by branch up the stack
(`feat/raydium` … `feat/test-validator`). Each report still describes the
code as reviewed; this table says where the P0s stand now.

| P0  | Status      | Where / what remains                                                                                                                                                              |
| --- | ----------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1   | Partly done | `orca_whirlpools_macros` pinned to `=1.0.4` on `feat/orca`. The license-only `cargo deny` gate is not restored.                                                                   |
| 2   | Done        | `feat/scenario`: re-centres resume (zero-liquidity decrease skipped, a missing NFT counts as closed), atomic maker state.                                                         |
| 3   | Done        | `feat/scenario`: balances are re-read after a close.                                                                                                                              |
| 4   | Partly done | `feat/scenario`: `PriceBook` ages prices by publish time. The oracle-side fixes (restamped `received_at_us`, half-open websocket) are in the oracle on `main`, outside the stack. |
| 5   | Partly done | `ChainClient::send_versioned`, slot-pinned fills and batched balance reads (`feat/scenario`, `feat/arb`). No shared batched confirmer or global RPC bound yet.                    |
| 6   | Partly done | The quote-to-send gap is gone (`feat/scenario`). Arrivals are still closed-loop.                                                                                                  |
| 7   | Done        | `feat/router`: `Arc` snapshots, single-flight loads, 400 ms TTL default.                                                                                                          |
| 8   | Done        | `feat/arb`: executions run beside the scan loop; per-bot priority fee and latency (profiles generated on `feat/scale`).                                                           |
| 9   | Done        | `feat/arb`: typed `getBlock` skips, RPC errors fail the report, Prometheus parts best-effort with `warnings`.                                                                     |

Notable items still open, by report:

- **flow-and-arb:** open-loop arrivals (flow #1), a global RPC bound with
  batched confirmations (flow #3), a separate refill worker (flow #8); the
  full arb cache/search/execute split and one multiplexed websocket client.
- **scenario-and-tooling:** slot-consistency checks across snapshot reads
  (#10); `leftovers` still scans with `getProgramAccounts`, which public
  testnet RPCs limit; TypeScript, Python and shell checks in CI (no
  `ruff`/`shellcheck` installed on the dev machine); host and socket metrics.
- **raydium-clmm:** the `base_flag` change was declined (a trade-off, not a
  fix); the tick-array clamp was a no-op (the program filters out-of-range
  arrays).
- **manifest-chain-harness:** cancel hints need an on-chain experiment (the
  GPL source stays unread).
- **router-and-venues:** R15 checked and is not a bug.

## Where the workspace stands

- **Gates are clean.** `cargo clippy --workspace --all-targets -D warnings`
  passes; `cargo test --workspace` passes all **67 tests**. No duplicate
  Solana or Tokio versions (only several `rand` generations, transitively).
- **Rules are mostly respected.**
  - Fixed-point money math: `f64` appears mainly in metrics, plus the
    exceptions listed in the reports (flow notional sampling, `FlowConfig`,
    `scale.py`, one oracle config field).
  - Granular Solana crates, with the 4.2.1 floor kept.
  - No GPL code is linked.
  - No service depends on another service crate.
- **The quality is uneven.** The pure cores are good and well guarded: the
  router engine, the Manifest book decoder, Orca's use of `orca_whirlpools_core`,
  and `mtm-math::clmm`. The I/O edges around them are where the bugs and the
  scaling limits are: submission, confirmation, snapshot loading, error
  handling and state persistence.

Test distribution (`#[test]` count):

| Tests | Crates                                                                                                                                                                                                                 |
| ----- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 10–11 | router (11), oracle (10)                                                                                                                                                                                               |
| 7–9   | manifest-dex-client (9), mtm-math (8), mtm-dex-harness (7)                                                                                                                                                             |
| 1–5   | mtm-venues (5), mtm-scenario (4), orca-whirlpool-client (4), mtm-common (2), scenario (2), manifest-dex-orchestrator (2), arb (1), oracle-client (1), orca-whirlpool-orchestrator (1)                                  |
| **0** | **raydium-clmm-client** (2.6k lines of swap math), **mtm-chain** (the submission state machine), **flow**, **mm**, mtm-feeds, mtm-telemetry, raydium-clmm-orchestrator, router-client, and the three orchestrator CLIs |

Dependency graph observations (from the `Cargo.toml`s):

- **`services/scenario` breaks the literal service rule.** It depends on the
  three `*-orchestrator` libraries and `mtm-dex-harness`, not only on
  `*-client` crates.
- **Dependencies point the wrong way.** `orca-whirlpool-client` and
  `manifest-dex-client` depend on `mtm-dex-harness` (a CLI/test crate) for SPL
  helpers. `mtm-scenario`, `flow` and `arb` pull it in too, `flow` only for env
  keypairs.
- **`mtm-scenario` is a hub.** It depends on every venue client plus the
  harness and holds every service's config types, so any change rebuilds every
  service.
- **Orphans.** `crates/mtm-feeds` is not a workspace member (and wouldn't
  resolve), and `mtm/Cargo.toml` is a tracked stale manifest.
- **An unused dependency.** `services/oracle` depends on `mtm-chain` and never
  uses it.

## P0: fix now

| #   | Where                               | What                                                                                                                                                                                                                                                                                                                  | Why now                                                          |
| --- | ----------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------- |
| 1   | orca-whirlpool-client               | **Licensing (Critical).** `Cargo.lock` resolves `orca_whirlpools_macros 1.0.5`, published under the Orca License. Pin it to `=1.0.4`, list it in `docs/dex-forks.md`, and restore a license-only `cargo deny` gate (the deny job was removed in `5bfabb0`).                                                           | Breaks the documented Apache-2.0-only policy.                    |
| 2   | mm                                  | **A partial re-centre leaves an LP stuck forever (Critical).** If decrease lands and collect or close fails, every later tick retries `decrease_liquidity(None)` on a zero-liquidity position, which errors. Make close idempotent, or one transaction.                                                               | LPs silently stop under exactly the congestion we test.          |
| 3   | mm                                  | **Reopens are sized from pre-close balances.** LP depth roughly halves on every re-centre.                                                                                                                                                                                                                            | Explains part of the books thinning during long runs.            |
| 4   | oracle, oracle-client, mtm-scenario | **Staleness is broken end to end.** Halted feeds look fresh (`received_at_us` is restamped on every poll); thin Binance books look stale while live; a half-open websocket hangs forever; consumers age-check the wrong field. Fix them together, then restore the 30 s limit (the 120 s workaround hides all of it). | Participants trade against frozen prices without noticing.       |
| 5   | mtm-chain, flow                     | **Per-transaction confirmation polling and unbounded RPC concurrency** (about 15–20 RPCs per swap, ~24k sockets at x2000). Build a shared confirmer with batched `getSignatureStatuses`, a semaphore, and fills from transaction meta.                                                                                | The x2000 ceiling and `EADDRNOTAVAIL`.                           |
| 6   | flow                                | **Closed-loop arrivals** cut offered load 20–45% invisibly; the quote-to-send gap adds stale-quote rejections. Use an open-loop scheduler and send immediately after signing.                                                                                                                                         | Load numbers don't mean what they say.                           |
| 7   | router, mtm-venues                  | **Deep `PoolSnapshot` clones on every quote, and a cache that is off by default (TTL 0, no single-flight).** Use `Arc` snapshots, always coalesce loads, and default the TTL to about one slot.                                                                                                                       | The likely main cause of 5 s quotes and ~4.5 cores.              |
| 8   | arb                                 | **The scanner is blind while all 50 bots confirm**, and the bots herd onto the same cycle (on Agave the losers land failed and pay fees). Pipeline scanning and execution; give bots their own profiles.                                                                                                              | Missed opportunities and unrealistic failed-transaction traffic. |
| 9   | scenario                            | **Report and snapshot swallow RPC errors.** `getBlock` errors count as empty blocks, and `getTokenAccountsByOwner` errors count as "no positions".                                                                                                                                                                    | Published TPS and PnL can be silently wrong.                     |

## Cross-cutting themes

1. **The transaction layer isn't one layer.** `mtm-chain::submit` is
   legacy-only, so the v1 users (flow, arb, router) bypass it. It also:
   - polls per transaction;
   - signs before simulating, so it always requests 1.4M CU;
   - never rebroadcasts;
   - turns a single poll error into an ambiguous outcome;
   - takes non-`Sync` signers, which is why mm runs a `LocalSet` and flow
     shards over OS threads.

   Nobody reconciles ambiguous outcomes, so every higher-level retry is a
   double-execution risk, and `tx.rs` still has a re-sign-on-timeout helper.
   _Target:_ one `mtm-chain` with v1 and legacy submit, simulate-then-size CU,
   a confirmer actor (batched statuses, rebroadcast until expiry), a typed
   `Pending` outcome and `Send` signers.

2. **Reads are chatty, unbatched and not slot-consistent.**
   - Orca and Raydium snapshot loads take 4–6 serial RPCs, including
     `getEpochInfo` and the ATAs of a `NullSigner(Pubkey::default())`.
   - Raydium reads pool and tick arrays in separate calls, a race Orca guards
     against.
   - `position_value` mixes slots, so wrapping fee growth saturates to
     `u64::MAX`.
   - Post-confirmation reads aren't pinned with `min_context_slot`.

   _Target:_ cached static pool data plus one `getMultipleAccounts` (with the
   Clock sysvar) per refresh, slot-tagged `Arc` snapshots, and eventually a
   push cache (websocket or Geyser) shared by the router, arb and mm.

3. **Silent fallbacks instead of typed errors.** `VenueError::Venue{String}`,
   errors that become 0 or empty in pnl and teardown, losses clamped to 0 in
   the arb profit counter, a 422 for an RPC failure in the router, and
   router-client discarding response bodies. _Target:_ typed error enums per
   layer (`Rpc`, `Malformed`, `NoLiquidity`, `Unsupported`, `Ambiguous`), and
   measurement tools that fail or emit `warnings` instead of guessing.
4. **Duplication that already diverges.**
   - The Raydium and Orca clients share about 40% of their plumbing.
   - The Raydium CLI re-implements the harness.
   - Participant enumeration exists four times.
   - Seat-locked valuation exists twice.
   - ATA dedup and CU summing exist twice, and the 50k CU margin is hard-coded
     in three places.

   _Target:_ see the shared CLMM layer proposal in `raydium-clmm.md`, plus a
   `holdings` valuation module and an `mtm_venues::build` budget helper.

5. **Contention handling.** LP operations use single-point slippage bounds
   (Orca core 1.0.4) or client-fixed liquidity (Raydium, `base_flag: None`).
   With a 4–6 RPC gap between quote and landing, that produces the 6017/6018
   failures, and makers have no priority fees and 1.4M CU requests. _Target:_
   price-band bounds (Orca, implemented independently), `base_flag` liquidity
   (Raydium), one retry, and per-participant execution profiles (CU margin,
   priority fee, slippage).
6. **Ops baseline is missing everywhere.**
   - No HTTP client sets timeouts.
   - No service handles SIGTERM.
   - The oracle's `/health` always says ok, and its JoinHandles are dropped.
   - Histograms export as summaries, which can't be aggregated across flow
     shards.
   - The router has no load shedding.
   - Shutdowns are ungraceful mid-re-centre.
7. **CI covers only Rust.** No TypeScript, Python or shell checks, no
   codegen drift check (and `just gen-sdks` is broken by a stale
   `raydium-cpmm` target), and no license gate.
8. **Bash owns too much.** `scenario.sh` has become a process supervisor and
   measurement protocol: non-waiting stops, keys left in `/dev/shm` on Ctrl-C,
   a `deploy` that upgrades a live program when one RPC call fails, and a
   report that runs after thawing. The safety-critical parts belong in Rust.

## Target layering

```
L0 pure        mtm-math (fixed-point, rounding modes, CLMM math), mtm-common (config + validation)
L1 chain I/O   mtm-chain: submit (legacy + v1), confirmer actor, typed JSON-RPC
               (getBlock / token accounts / gPA), AccountBatch loader, spl helpers, keys
L2 venues      <venue>-client: pure quoting (no RPC) + signer-free loaders + actions,
               implementing ClmmVenue / ClobVenue traits
L3 aggregation mtm-venues: Arc<PoolSnapshot>, snapshot store (poll → push), CU budgets,
               route building; jupiter-fork-client split out
L4 domain      mtm-scenario (config, wallets, participants), holdings (valuation),
               funding, oracle-client (types + freshness API)
L5 services    oracle · router · mm · flow · arb · scenario (operator CLI, incl. measure)
tools          mtm-dex-harness (CLI/test only) · *-orchestrator libs + CLIs (bootstrap only;
               scenario should call them through a bootstrap facade, not link all three)
```

Rules to add to CLAUDE.md once it's adopted:

- Client crates never depend on the harness.
- Anything that signs goes through `mtm-chain`.
- Anything taking a `PoolSnapshot` lives in L2/L3.
- Each service owns its own config types.

## Suggested roadmap

| Phase    | Scope                                                                                                                                                                                                                                                                                                                  | Effort |
| -------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------ |
| 0 (days) | P0 items 1–4 and 9; router TTL default and single-flight (item 7, first half); delete `mtm/Cargo.toml`, `mtm-chain/src/tx.rs` and the unused oracle dependency; fix the `gen-sdks` target; timeouts on every HTTP client.                                                                                              | S each |
| 1        | `mtm-chain` v1 submit + confirmer + simulate-then-size CU + `Send` signers (unblocks dropping the flow sharding and mm `LocalSet`); flow open-loop with transaction-meta accounting; arb pipeline; mm plan/execute/persist phase machine with on-chain reconciliation.                                                 | M–L    |
| 2        | `Arc` snapshot store with static/dynamic split and batched, slot-tagged loads; shared CLMM layer and the Raydium CLI on the harness; split `pnl.rs` and `mtm-scenario`; typed errors end to end; reference-vector tests for Raydium; golden fixtures for Manifest; CI for TS/Python/shell, codegen drift and licenses. | M      |
| 3        | Push cache (websocket or Geyser) shared by router, arb and mm; testnet readiness (TPU/QUIC sends via SWQoS, per-participant transaction filtering, settle based on blockhash expiry); `measure` in Rust; multi-host load.                                                                                              | L      |

See also `docs/scenario-load-findings.md` for the load-test evidence behind
several of these items.
