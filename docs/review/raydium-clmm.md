# Review: Raydium CLMM client and orchestrators

Scope: `crates/raydium-clmm-client`, `crates/raydium-clmm-orchestrator`,
`services/raydium-clmm-orchestrator`, and `crates/mtm-math/src/clmm.rs` as used
here.

Reviewed 2026-10-14 at `bc96670`, read-only. The program comparisons are
against the pinned `raydium-clmm` rev `ed7c84a`; whether the deployed testnet
binary matches that rev was not verified. Severity is Critical/High/Medium/Low.
Priority runs from P0 (now), P1 (next) and P2 (later) to P3 (nice to have).

## raydium-clmm-client

**Purpose and boundary.** The async Raydium CLMM SDK: account decoding and
validation, PDAs, the swap quote loop (`quote.rs`), the position lifecycle,
`SwapSnapshot`, and `position_value`. The pinned program crate (Solana 2.x) is
wrapped behind `adapter.rs`. There are no service dependencies, but roughly 40%
of the 2.4k lines duplicates `orca-whirlpool-client`. **The crate has no
tests.**

| #   | Severity | Category        | Finding                                                                                                                                                                                                                                                                                                                                                                                                                                 | Recommendation                                                                                                                                                                                         | Priority | Impact                                                                                     |
| --- | -------- | --------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | -------- | ------------------------------------------------------------------------------------------ |
| 1   | High     | correctness     | **The quote ignores `is_fee_on_input`.** `quote.rs:126-150` always consumes `amount_in + fee_amount`, while the program's `apply_swap_amounts` adds the fee only when it is on input. On `Token0Only/Token1Only` pools, exact-in over-deducts (under-quotes, or errors with "swap input underflow") and exact-out overstates the input. Pools this client creates default to FromInput, so only customizable or external pools are hit. | `consumed = if fee_on_input { in + fee } else { in }`.                                                                                                                                                 | P1       | Wrong quotes on fee-on-output pools.                                                       |
| 2   | Medium   | correctness     | **The exact-in threshold uses gross output.** `lib.rs:1801` applies slippage to `calculated`, but the program compares `other_amount_threshold` to the output delta _net_ of the Token-2022 transfer fee. The net `expected_output_raw` (`:1803`) is computed but not used.                                                                                                                                                             | `apply_slippage(expected_output_raw, …)`.                                                                                                                                                              | P1       | Every exact-in swap into a mint whose transfer fee exceeds the slippage reverts with 6018. |
| 3   | Medium   | correctness/ops | **LP slippage under contention.** Liquidity is fixed client-side from a stale snapshot (`lib.rs:1296-1311`, `base_flag: None` at `:360,:419`). Any price move pushes the other side past `amount_max`: 6017 PriceSlippageCheck. Decrease has the same exposure via `amount_*_min` (`:1375-1382`). At this rev, LP instructions never raise 6018; that comes from swap legs.                                                             | Open/increase with `liquidity = 0`, `base_flag = Some(input_is_mint_0)` and `amount_X_max = input`, so the program derives liquidity at execution price. Allow `min = 0` on decrease for traffic runs. | P1       | Removes most 6017s under load.                                                             |
| 4   | Low      | correctness     | **Partial fills are accepted.** The quote returns `Ok` on a partial fill at the price limit or tick bounds (`quote.rs:57-62`), but the swap requires a full fill.                                                                                                                                                                                                                                                                       | Error when `remaining != 0` on exit.                                                                                                                                                                   | P2       | Quotes that then revert.                                                                   |
| 5   | Low      | correctness     | **Liquidity is derived from the gross input,** and the transfer fee added on top (`lib.rs:1290,1331`), the opposite of the program's `base_flag` path.                                                                                                                                                                                                                                                                                  | Pick one semantic (#3 fixes it).                                                                                                                                                                       | P3       | Small over-spend on fee mints.                                                             |
| 6   | Low      | correctness     | **`10_u128.pow(decimals)` overflows** for decimals > 38 (`lib.rs:2275,2291`).                                                                                                                                                                                                                                                                                                                                                           | `checked_pow`.                                                                                                                                                                                         | P3       | Panic on a hostile mint.                                                                   |
| 7   | Low      | correctness     | **`position_value` gaps.** No rewards; pool-order mints labelled `mint_a/b` (`lib.rs:574,2093`); it bypasses `run()`, so no metrics.                                                                                                                                                                                                                                                                                                    | `mint_0/1`; wrap in `run`.                                                                                                                                                                             | P3       | Mislabelling risk in PnL.                                                                  |
| 8   | Medium   | perf            | **`load_pool` makes 4 sequential round trips** (`lib.rs:922,941,1004-1010` getEpochInfo, `1013` ATAs). That makes `position_value` 6 RTTs, open 5 plus submit, and decrease 6-7. `close_position` loads the pool only to discard it (`:598`).                                                                                                                                                                                           | Clock sysvar and both ATA candidates in the second batch (2 RTTs); drop the load in `close_position`; a lean 2-RTT `load_position_value`.                                                              | P1       | Roughly halves RPC per operation.                                                          |
| 9   | Medium   | perf            | **Snapshots are heavy.** Both directions are fetched sequentially, up to 24 arrays each (`lib.rs:39,658-665`), about 480 KB per snapshot. `quote()` deep-clones the direction's `VecDeque` on every call (`:1770`). Payer ATAs are fetched even though the router doesn't need them.                                                                                                                                                    | One `getMultipleAccounts`; limit to `MAX_SWAP_TICK_ARRAYS` (6), since more is rejected anyway (`:1781`); quote over `&[TickArrayState]`; split out payer state.                                        | P1       | About 4× less bandwidth; no ~240 KB clone per quote.                                       |
| 10  | Low      | perf            | **Transfer-fee config is reparsed on every quote.** `transfer_fee` and `inverse_transfer_fee` re-unpack the mint TLV 2-4 times per quote (`lib.rs:2338,2356`).                                                                                                                                                                                                                                                                          | Parse once in `decode_mint`.                                                                                                                                                                           | P3       |                                                                                            |
| 11  | Low      | correctness     | **Fee saturation differs from the program.** `fees_accrued` saturates to `u64::MAX` and `lib.rs:577-581` uses `saturating_add`; the program returns 0 or errors.                                                                                                                                                                                                                                                                        | Match or document.                                                                                                                                                                                     | P3       | Degenerate states only.                                                                    |
| 12  | Medium   | testing         | **No tests:** none for `quote_swap`, `price_to_sqrt_x64`, `apply_slippage`, tick rounding, PDAs or `position_value`. Orca has a reference-vector test against core (`orca lib.rs:2444-2476`).                                                                                                                                                                                                                                           | Fixture-based diff of `quote_swap` against a step port of `swap_internal` (fee-on-output and transfer-fee cases), PDA vectors, and a test-validator round trip.                                        | P1       | #1, #2 and #4 would have been caught.                                                      |
| 13  | Low      | standards       | **Hygiene.** A pointless `std::mem::take` (`lib.rs:393,421`); `too_many_arguments` on `new`; errors erased to `String` in `SwapSnapshot` and `quote.rs`.                                                                                                                                                                                                                                                                                | A config struct; typed errors.                                                                                                                                                                         | P3       |                                                                                            |
| 14  | Low      | correctness     | **`ensure_position_balances` checks the inflated `amount_max`** (`lib.rs:1116`), rejecting deposits that would succeed.                                                                                                                                                                                                                                                                                                                 | Check expected plus fee; let the program enforce the max.                                                                                                                                              | P3       | False InsufficientBalance.                                                                 |

**Verified correct against the program:**

- all PDA seeds;
- the tick-array start index;
- the Token-2022 extension allow-list;
- the exact-out input threshold;
- the fee-growth-inside formula, including its boundaries;
- `mul_shr_64` (an exact floor);
- the swap loop for static-fee pools without limit orders. Dynamic-fee pools
  and limit-order ticks are explicitly rejected (`lib.rs:653`, `quote.rs:97`).

**Design recommendations**

- **`base_flag` liquidity for open/increase,** with configurable minimum
  amounts on decrease (0 allowed for traffic runs). The root fix for 6017 under
  contention. (S)
- **Split `PoolBundle` into a pool snapshot and payer state,** and add a
  `SnapshotLoader` that batches N pools into one or two `getMultipleAccounts`
  calls. (M)
- **Crossing metrics from the quote loop** (ticks crossed, arrays touched), to
  replace `100k + 15k × arrays` (observed p50 53k / p99 102k). (S)
- **Reference-vector test harness (#12).** (M)

## raydium-clmm-orchestrator (library)

**Purpose and boundary.** Sequences admin config, pool creation and a scripted
user lifecycle (fund, open, increase, swap ×2, decrease, collect, close). It
still hand-rolls SPL mint decoding, ATA derivation and minting that
`mtm-dex-harness` already provides; the Orca orchestrator uses
`TestTokenFunder`.

| #   | Severity | Category      | Finding                                                                                                                                                                                        | Recommendation                                        | Priority | Impact                                   |
| --- | -------- | ------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------- | -------- | ---------------------------------------- |
| 1   | Medium   | standards/ops | **`span.enter()` held across `.await`** (`lib.rs:241,270`): spans are misattributed, and the `!Send` guard blocks `tokio::spawn`. (Orca has the same bug.)                                     | `.instrument(span)`.                                  | P1       | Broken tracing; blocks concurrent flows. |
| 2   | Medium   | boundary      | **Hand-rolled SPL code:** offsets (`:27-31`), `decode_mint` (`:476-530`), ATA, create-ATA and mint-to (`:532-583`), program IDs parsed from strings, and a third copy of `ui_to_raw` (`:647`). | Use `TestTokenFunder` / `spl`, as Orca does.          | P2       | About 250 lines removed.                 |
| 3   | Low      | correctness   | **Incomplete pool check.** The flow checks that `amm_config` exists but not that the pool belongs to it (`:272`).                                                                              | Compare against the loaded pool.                      | P3       | Misleading validation.                   |
| 4   | Low      | ops           | **`collect` after a full decrease moves nothing** (`:337`), since the decrease already transfers the fees.                                                                                     | Swap before decreasing to really test fee collection. | P3       | A coverage illusion.                     |
| 5   | Low      | ops           | **Failures aren't resumable;** a mid-flow failure leaves an open position.                                                                                                                     | A `teardown_positions(owner)`.                        | P3       | Manual cleanup.                          |
| 6   | Low      | testing       | No tests.                                                                                                                                                                                      | Unit tests.                                           | P2       |                                          |
| 7   | Low      | ops           | **Every transaction requests 1.4M CU** (`:110`).                                                                                                                                               | Simulation plus a margin.                             | P3       |                                          |

**Design recommendations:**

- Adopt the harness `TestTokenFunder` (S).
- Express the flow as `enum Step` with per-step results, so partial progress is
  reported and teardown is possible (M).

## services/raydium-clmm-orchestrator (CLI)

**Purpose and boundary.** `admin`, `create-pools` and `user-flow`. At 746
lines against Orca's 356, it re-implements all of `mtm-dex-harness::cli`.

| #   | Severity | Category  | Finding                                                                                                                                                                                                                                                                                                           | Recommendation                                                                                     | Priority | Impact                            |
| --- | -------- | --------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------- | -------- | --------------------------------- |
| 1   | Medium   | boundary  | **Near-verbatim copies of harness code:** `run_ephemeral_users`, `reclaim_user_funds`, `transfer_lamports`, `TransactionArgs`, `connected_chain`, the keypair helpers, `resolve_pubkey`, `unique_mints`, `unordered_pairs`, `transaction_json`, plus raw SPL/system builders (`main.rs:146-156,273-334,399-723`). | Use `mtm_dex_harness::cli`, as Orca and Manifest do.                                               | P1       | About 400 lines; fixes land once. |
| 2   | Medium   | ops       | **Funded users are stranded on the first failure.** `flow.context(...)?` (`main.rs:377`) returns early, so later users who were already funded (1 SOL each by default) are never reclaimed. The same bug exists in `mtm-dex-harness/src/cli.rs` `run_ephemeral_users`.                                            | Reclaim all users before returning, or collect results and fail at the end. Fix it in the harness. | P1       | SOL leaks on every failed run.    |
| 3   | Low      | ops       | **Recovery fails for fee mints.** It calls `close_token_account` (`main.rs:503`), which Token-2022 refuses while withheld fees remain.                                                                                                                                                                            | Skip the close, or harvest first, when the extension is present.                                   | P3       | Recovery fails on fee mints.      |
| 4   | Low      | ops       | **`create-pools` isn't idempotent:** it stops at the first existing pool.                                                                                                                                                                                                                                         | `--skip-existing`.                                                                                 | P3       | Operator friction.                |
| 5   | Low      | standards | **Hard-coded token-account offsets** (`main.rs:28-29`).                                                                                                                                                                                                                                                           | `spl::decode_*`.                                                                                   | P3       |                                   |
| 6   | Low      | testing   | No tests.                                                                                                                                                                                                                                                                                                         | Inherited once migrated to the harness.                                                            | P3       |                                   |

**Design recommendation:** rebase onto `mtm_dex_harness::cli`, mirroring the
Orca CLI (S).

## mtm-math::clmm (as used here)

The one real shared CLMM abstraction today, and correctly placed.

| #   | Severity | Category    | Finding                                                                                                             | Recommendation                                     | Priority | Impact                           |
| --- | -------- | ----------- | ------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------- | -------- | -------------------------------- |
| 1   | Low      | testing     | **Missing edge vectors:** no wraparound, no `current == lower/upper` boundary, no U256 reference (`clmm.rs:53-75`). | Add the vectors plus a property test against U256. | P2       | Guards the fixed saturation bug. |
| 2   | Low      | correctness | **Saturates where the program returns 0 or errors** (`:30`).                                                        | Document or match.                                 | P3       |                                  |
| 3   | Low      | standards   | **Eight positional arguments** make lower and upper easy to swap (`:8-17`).                                         | `FeeGrowthInputs` plus `Range` structs.            | P3       | Misuse risk.                     |

## Cross-cutting notes: a shared CLMM layer

The Raydium and Orca clients duplicate:

- **helpers:** `require_account`, `ensure_positive`, `ui_to_raw`,
  `price_to_sqrt_x64`, `apply_slippage`, transfer-fee and inverse-fee handling,
  the vault and token-account validators, `decode_mint`, the extension
  allow-list;
- **plumbing:** the `run()` metrics/span wrapper, `submit()` error mapping, the
  outcome and error shapes;
- **flows:** `load_pool` (4 RTTs plus getEpochInfo) and `position_value`.

Proposal, extending the existing crates rather than adding a new one:

1. **`mtm-math::clmm` (pure):** `apply_slippage(round)`,
   `price_to_sqrt_x64` / `sqrt_x64_to_price`, `round_tick_range`, and
   `fees_accrued`, backed by shared reference vectors.
2. **`mtm-dex-harness::token`:** `MintInfo` with a pre-parsed
   `TransferFeeConfig`, the transfer-fee helpers, validators, `ui_to_raw`, and
   the Token-2022 allow-list.
3. **`mtm-dex-harness::action`:** a generic `run_action` (metrics and spans),
   `SubmissionError → VenueError` mapping, and a batched `AccountBatch` loader
   including the Clock sysvar (no more getEpochInfo).
4. **`trait ClmmVenue`** (load_snapshot, quote, swap_ixs,
   open/increase/decrease/collect/close, position_value), implemented per
   client and consumed by mtm-venues, `scenario/pnl.rs` and the orchestrators.
   The program-specific swap loops stay in each client.

Effort: M for steps 1-3, M for step 4. Expected payoff: about 1.5k lines
removed, and RPC batching and slippage fixes land once for both venues.
