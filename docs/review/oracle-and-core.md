# Review: oracle service and core crates

Scope: `services/oracle`, `crates/oracle-client`, `crates/mtm-feeds`,
`crates/mtm-common`, `crates/mtm-telemetry`, `crates/mtm-math`.

Reviewed 2026-10-14 at `bc96670` (feat/test-validator), read-only. Workspace
gates at that commit: `clippy --workspace --all-targets -D warnings` is clean,
and all 67 tests pass. Severity is Critical/High/Medium/Low. Priority runs from
P0 (now), P1 (next) and P2 (later) to P3 (nice to have).

## services/oracle

**Purpose and boundary.** Pulls prices from three sources (Pyth Hermes REST
polling, Binance bookTicker websocket, CoinGecko REST polling). A pricing
engine turns them into derived instruments (underlying, cross, basket, peg,
GBM, plus transforms), and the results are served over HTTP and websocket.
Downstream, every consumer uses it only through `oracle-client` and
`mtm-scenario::PriceBook`, so the dependency direction is clean. The
service-internal boundary has two problems: it depends on a crate it never
uses, and it carries a copy of the Hermes client.

| #   | Severity | Category    | Finding                                                                                                                                                                                                                                                                                                                                                                       | Recommendation                                                                                                                                                | Priority | Impact                                                                          |
| --- | -------- | ----------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------- | ------------------------------------------------------------------------------- |
| 1   | High     | correctness | **A halted feed looks fresh.** `received_at_us` is set to now on every Hermes poll (`feed_task.rs:57`) and again on every engine eval (`pricing/mod.rs:367`), even when `publish_time` hasn't moved. Consumers check age against `received_at_us` (`mtm-scenario/src/pricing.rs:26,44`), but `docs/pricing-types.md:38-39` defines staleness as the age of `publish_time_us`. | Consumers check `publish_time_us`, falling back to `received_at_us` only when it is `None`. In feed_task, skip a tick when `publish_time` hasn't advanced.    | P0       | A halted Pyth feed is served as fresh to every maker and arb.                   |
| 2   | High     | correctness | **Thin books go stale even though nothing is wrong.** Binance bookTicker only fires when the best bid/offer changes, and the flush only sends symbols that got a message (`binance.rs:104,129`). Nothing re-confirms a live but unchanged book. This is the root cause of the RENDER >30 s staleness; the workaround raised the limit to 120 s for every asset.               | Treat "socket alive and unchanged" as fresh: re-send the last book periodically while connected, or add `last_confirmed_us`. Then put the limit back to 30 s. | P0       | Liquid assets lose staleness protection; thin assets are dropped for no reason. |
| 3   | High     | correctness | **A half-open Binance connection hangs forever.** The `select!` loop (`binance.rs:90-137`) has no idle or read timeout. If the TCP connection goes half-open, `stream.next()` never returns, and the last prices are served indefinitely with no reconnect.                                                                                                                   | Track when the last frame arrived and reconnect after about 30 s of silence (`tokio::time::timeout`).                                                         | P0       | Silent stale prices.                                                            |
| 4   | High     | ops         | **Task failures are invisible.** All JoinHandles are dropped (`lib.rs:54-75`) and `/health` always returns "ok" (`api.rs:26-28`). If the engine or a feed task dies, the API keeps serving frozen state and reports healthy.                                                                                                                                                  | Watch the tasks with a `JoinSet` and exit if one dies. Make `/health` fail when any instrument's age exceeds a limit. Export a per-symbol age gauge.          | P1       | Hidden outages; restarts never trigger.                                         |
| 5   | Medium   | correctness | **HTTP requests have no timeout.** The Hermes and CoinGecko clients set none (`hermes.rs:92-95`, `coingecko.rs:36-39`), so a request that hangs stalls the poll loop. `.unwrap_or_default()` also silently drops the User-Agent if the builder fails, and CoinGecko returns 403 without one (`lib.rs:20`).                                                                    | Set `.timeout()` and `.connect_timeout()`; make a builder failure a startup error.                                                                            | P1       | The feed stalls with no error counted.                                          |
| 6   | Medium   | correctness | **Reconnects use a fixed delay.** Binance waits a fixed 5 s or 1 s with no exponential backoff or jitter (`binance.rs:77,140`). Hermes and CoinGecko don't back off either; a CoinGecko 429 just retries on the next interval (`coingecko.rs:83-86`).                                                                                                                         | Exponential backoff with jitter, and honour `Retry-After`.                                                                                                    | P2       | Ban or rate-limit spirals.                                                      |
| 7   | Medium   | correctness | **BONK is served with about 2-3 significant digits.** Every instrument defaults to `expo = -8` (`pricing/spec.rs:21-23`), and BONK/USD uses that default. At about $3e-6 the mantissa is about 300, so each step is roughly 33 bps — the size of maker spreads.                                                                                                               | Validate a minimum number of significant figures at startup, or derive `expo`. Set BONK to around -12.                                                        | P1       | Mispriced quotes and false arbitrage on low-priced assets.                      |
| 8   | Medium   | correctness | **Instruments can stop publishing with no log or metric.** `eval` returns `None` without logging (`pricing/mod.rs:354,437,453`). NaN or ±inf in GBM `price` or Noise `x` is permanent (`:453-454`, `:528`; e.g. `halflife_s = 0` with `dt = 0` gives 0/0). `expo`, `halflife_s > 0` and `sigma` bounds aren't validated.                                                      | Count eval failures per symbol, check generated values are finite, and validate config in `from_config`.                                                      | P1       | Silent per-instrument outage.                                                   |
| 9   | Medium   | correctness | **Synthetic prices jump on restart.** GBM restarts from `initial` and the basket `factor` is recomputed (`pricing/mod.rs:192,420-425`), so the price jumps away from what is already on-chain.                                                                                                                                                                                | Save state, or seed from the current snapshot.                                                                                                                | P2       | The jump creates false arbitrage after a restart.                               |
| 10  | Medium   | perf        | **Every request and websocket client redoes work.** Each `/v1/prices` call clones the map under a RwLock and serializes it again (`state.rs:47-49`). Each websocket subscriber serializes every point again (`api.rs:76`). Each update allocates a metric-label `String` (`state.rs:33`).                                                                                     | Serialize once per update: broadcast `Arc<str>` or `Bytes`, cache the body, use `ArcSwap`.                                                                    | P2       | CPU cost grows with the number of pollers.                                      |
| 11  | Low      | perf        | **Small avoidable work in hot paths.** A Vec is cloned per tick (`pricing/mod.rs:315`) and `timed.clone()` runs every 250 ms (`:326`). Hermes's binary payload is downloaded and parsed every second but never used (`hermes.rs:111`).                                                                                                                                        | Iterate without cloning; drop the binary payload until a pusher needs it.                                                                                     | P3       | Minor.                                                                          |
| 12  | Low      | boundary    | **Unused dependency.** `mtm-chain` is in `Cargo.toml:9` but nothing uses it.                                                                                                                                                                                                                                                                                                  | Remove it.                                                                                                                                                    | P2       | Misleading dependency graph.                                                    |
| 13  | Low      | boundary    | **Duplicate Hermes client.** `src/hermes.rs` is a forked copy of `crates/mtm-feeds/src/hermes.rs`.                                                                                                                                                                                                                                                                            | Keep one (see mtm-feeds).                                                                                                                                     | P2       | The copies drift apart.                                                         |
| 14  | Low      | ops         | **No SIGTERM handling.** Only `ctrl_c` is handled (`lib.rs:92-95`).                                                                                                                                                                                                                                                                                                           | Also handle `SignalKind::terminate()`.                                                                                                                        | P3       | `docker stop` is ungraceful.                                                    |
| 15  | Low      | standards   | **f64 in config.** `Beta.beta` is an f64 config field (`spec.rs:79`); every other numeric config is a decimal string.                                                                                                                                                                                                                                                         | Use a string.                                                                                                                                                 | P3       | Consistency only.                                                               |
| 16  | Medium   | testing     | **Gaps in test coverage.** No route tests, no Hermes parsing tests, no Binance reconnect, idle or coalescing tests, no staleness, transform or crossed-book tests. The sources can't be tested because each builds its own client.                                                                                                                                            | Put a trait (or an injected base URL plus a mock server) behind each source; add `oneshot` route tests.                                                       | P1       | Findings 1-3 would have been caught.                                            |

**Design recommendations**

- **Source abstraction (M).** One `Source` trait and a shared supervisor that
  owns backoff, idle timeout, liveness re-sending, metrics and health. It fixes
  findings 3, 5 and 6 in one place and makes the sources testable.
- **Explicit freshness model (S-M).** Store when the upstream published a point,
  when we received it, and when the source last confirmed it was live. Define
  `age()` once in `oracle-client` (findings 1 and 2).
- **Supervised tasks and real health (S).** Put the tasks in a `JoinSet`, make
  `/health` and `/ready` reflect staleness, and export a per-symbol
  `oracle_price_age_seconds` gauge.
- **Publish snapshots instead of a locked map (S).** Keep one
  `ArcSwap<Snapshot>` of pre-serialized JSON and broadcast `Bytes`
  (finding 10).

## crates/oracle-client

**Purpose and boundary.** Wire types (`PricePoint`, `PriceData`,
`PriceSource`, `InstrumentInfo`) plus a typed HTTP/websocket client, used by
every participant. The boundary is mostly clean, but freshness policy has
leaked into `mtm-scenario::PriceBook`, and the client has no resilience of its
own.

| #   | Severity | Category    | Finding                                                                                                                                                                                                 | Recommendation                                                               | Priority | Impact                                 |
| --- | -------- | ----------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------- | -------- | -------------------------------------- |
| 1   | High     | boundary    | **Each consumer decides staleness itself, and gets it wrong.** There is no age helper on `PricePoint`; `mtm-scenario/src/pricing.rs:44` uses `received_at_us`, contrary to `docs/pricing-types.md:38`.  | Add `observed_at_us()` (publish time, else received time) and `age_us(now)`. | P0       | Same as oracle #1.                     |
| 2   | Medium   | correctness | **No timeout on oracle calls.** `reqwest::Client::new()` sets none (`lib.rs:34`), so a hung oracle hangs every participant loop.                                                                        | Use a builder with timeouts, or accept an injected client.                   | P1       | Participants freeze.                   |
| 3   | Medium   | correctness | **The websocket subscription never reconnects.** `subscribe()` (`lib.rs:93-107`) ends on disconnect, and a decode error is yielded as an `Err` item.                                                    | Reconnecting wrapper that resumes from a snapshot.                           | P2       | Latent; no callers yet.                |
| 4   | Medium   | correctness | **Adding a variant breaks every older client.** `#[non_exhaustive]` (`types.rs:11,34`) doesn't help on the wire: one unknown `kind` or `source` fails the whole `Vec<PricePoint>` decode (`lib.rs:55`). | `#[serde(other)] Unknown`; decode element by element and skip unknowns.      | P2       | A server-first rollout breaks clients. |
| 5   | Low      | standards   | **Errors are hard to tell apart.** A 404 surfaces as a generic `Http` error (`lib.rs:64-66`). `InstrumentInfo.base` and `transforms` are plain strings.                                                 | Add `OracleClientError::NotFound`.                                           | P3       | Weaker error handling in callers.      |
| 6   | Low      | standards   | **Fragile URL rewrite.** `ws_url` uses `replacen("http","ws",1)` (`lib.rs:111`).                                                                                                                        | Parse with `url::Url`.                                                       | P3       | Edge cases.                            |
| 7   | Low      | testing     | **Limited tests.** Only a wire-shape test exists; the client methods and some variants are untested.                                                                                                    | Round-trip and mock-server tests.                                            | P2       |                                        |

**Design recommendations**

- **Own the freshness API here (S):** move `mid()` and the age logic into
  `PricePoint`.
- **Split the wire types from the client (S):** an `oracle-types` crate or a
  feature, so code that only needs the types doesn't pull in reqwest and
  tungstenite.

## crates/mtm-feeds

**Purpose and boundary.** Meant to be the shared feed layer (Hermes plus a
`PriceFeed` trait). In practice it is **dead code**:

- it is not a root workspace member;
- it is listed only in a stale, tracked `mtm/Cargo.toml`;
- it would not resolve, because `async-trait` is not a workspace dependency;
- nothing depends on it.

| #   | Severity | Category  | Finding                                                                                                                                                                             | Recommendation                                                                              | Priority | Impact                                |
| --- | -------- | --------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------- | -------- | ------------------------------------- |
| 1   | Medium   | boundary  | **Orphan crate that diverges from the live code.** Its `hermes.rs` copy has no auth, no metadata or slot, and parses the mantissa as i128 instead of i64 (`hermes.rs:21-25,55-63`). | Delete it, or make it the home of the oracle's sources. Delete `mtm/Cargo.toml` either way. | P1       | Confuses readers; CI never checks it. |
| 2   | Low      | standards | **Interface conflicts with workspace rules.** `PriceUpdate.publish_time` is in seconds and `source` is a `&'static str` (`lib.rs:15-17`).                                           | If revived, emit `PricePoint`.                                                              | P2       |                                       |

**Design recommendation (S-M):** delete the crate, or move the Hermes, Binance
and CoinGecko sources into it behind one trait that emits `PricePoint`. The
second option also enables replaying a recorded tick tape against the engine.

## crates/mtm-common

**Purpose and boundary.** `Symbol`, `Cluster`, `RpcConfig`, the layered
figment config loader, and `now_us()`. Dependency-light and clean.

| #   | Severity | Category    | Finding                                                                                                                                                                   | Recommendation                                          | Priority | Impact                              |
| --- | -------- | ----------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------- | -------- | ----------------------------------- |
| 1   | Medium   | ops         | **A mistyped profile is silently ignored.** A missing file is skipped (`config.rs:31-34`), so `MTM_PROFILE=testnte` runs on `default.toml` (localhost RPC) with no error. | Fail on a missing named profile; log the resolved list. | P1       | A wrong-cluster run nobody notices. |
| 2   | Low      | correctness | **Loader settings leak into config.** `MTM_PROFILE` and `MTM_CONFIG_DIR` leak into config via `Env::prefixed("MTM_")` (`config.rs:36`).                                   | `.ignore(&["PROFILE","CONFIG_DIR"])`.                   | P3       | Breaks `deny_unknown_fields` later. |
| 3   | Low      | correctness | **A bad clock returns 0.** `now_us()` returns 0 when the clock is before the epoch (`time.rs:11`).                                                                        | Saturate or log.                                        | P3       |                                     |
| 4   | Low      | standards   | **Stringly-typed or unused types.** `RpcConfig.commitment` is a `String` (`config.rs:20`), and `Cluster` is unused outside the crate.                                     | Use an enum; remove or use `Cluster`.                   | P3       |                                     |

**Design recommendation (S):** a `Validate` step after `load()`, so bad values
fail at startup rather than at runtime.

## crates/mtm-telemetry

**Purpose and boundary.** A single `init()` for tracing and the Prometheus
exporter. Clean, but too thin.

| #   | Severity | Category  | Finding                                                                                                                                                                                        | Recommendation                                              | Priority | Impact                                         |
| --- | -------- | --------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------- | -------- | ---------------------------------------------- |
| 1   | Medium   | ops       | **Histograms export as summaries.** No buckets are configured (`lib.rs:16-18`), so all 12 `histogram!` call sites export summaries, which can't be aggregated across flow shards or instances. | Set buckets per metric family, or globally.                 | P1       | Load-test percentiles across shards are wrong. |
| 2   | Low      | standards | **`init()` can panic, and logs are text-only.** `fmt().init()` panics if a subscriber is already set (`lib.rs:13`). There is no JSON log option and no global `service` label.                 | `try_init`, JSON via env, `add_global_label("service", …)`. | P2       |                                                |

**Design recommendation (S):** shared metric helpers (feed age, source
errors), so every service exports the same names.

## crates/mtm-math

**Purpose and boundary.** `Price` (i128 mantissa with an exponent) plus the
CLMM fee math. Outside the oracle, only `Price` and `clmm::fees_accrued` are
used. Consumers do their money math in `rust_decimal`, and the router has its
own `apply_bps_floor` (`router/src/engine.rs:400`), so "fixed-point lives in
mtm-math" is mostly nominal.

| #   | Severity | Category    | Finding                                                                                                                                                                              | Recommendation                                                    | Priority | Impact                                                      |
| --- | -------- | ----------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------- | -------- | ----------------------------------------------------------- |
| 1   | Medium   | correctness | **Equal prices compare unequal.** `Price` derives structural `Eq` (`lib.rs:24`), so `Price(15327,-2) != Price(1532700,-4)`. There is no value ordering.                              | Value equality and `Ord` via a normalized exponent, or drop `Eq`. | P2       | A trap for comparisons.                                     |
| 2   | Low      | correctness | **Rounding modes are inconsistent.** `from_decimal` uses banker's rounding (`lib.rs:71`), while `checked_scale` and `apply_bps` truncate toward zero (`:99,112`).                    | An explicit `RoundingMode`.                                       | P2       | Small quoting bias.                                         |
| 3   | Low      | correctness | **i32 overflow on extreme exponents.** `self.expo - new_expo` and `powi(-expo)` (`lib.rs:87,79`).                                                                                    | `checked_sub` / `checked_neg`.                                    | P3       |                                                             |
| 4   | Low      | correctness | **Fee overflow is hidden.** `fees_accrued` saturates to `u64::MAX` (`clmm.rs:30`).                                                                                                   | Return `Option<u64>`.                                             | P3       | Misleading PnL on corrupt state.                            |
| 5   | Low      | standards   | **Misplaced doc comment.** It sits on `pub mod clmm;` (`lib.rs:156-157`).                                                                                                            | Move it to `mantissa_serde`.                                      | P3       |                                                             |
| 6   | Low      | standards   | **Serde limits for non-JSON formats.** `deserialize_any` (`lib.rs:196`) breaks bincode, and `visit_u128` is missing.                                                                 | Document as JSON-only.                                            | P3       |                                                             |
| 7   | Low      | testing     | **Test gaps.** `fees_accrued` has no above-range or wrapping cases, and there are no `FromStr`/`Display` property tests. `mul_shr_64` itself is exact and guarded (`clmm.rs:37-39`). | Proptests plus edge cases.                                        | P2       | Guards against regressions like the earlier saturation bug. |

**Design recommendations**

- **Make mtm-math the real money-math home (M).** Add rounding-aware `mul_div`
  and `apply_bps(round)` on raw token amounts, and replace the router's
  `apply_bps_floor` and the scattered `Decimal` math in mtm-scenario and mm.
- **Remove or justify unused APIs (S):** `to_pyth` and `checked_scale` have no
  callers outside the crate.

## Cross-cutting notes

- **Staleness is broken at both ends, and these are the most important fixes.**
  The oracle re-stamps `received_at_us` on every poll and eval; thin Binance
  books look stale while live; consumers age-check on `received_at_us`. The
  global 120 s workaround hides all three. Fix them together: oracle-client
  gets an `age()` API, the oracle confirms liveness, and the limit goes back to
  30 s.
- **Workspace hygiene.** Delete the tracked orphan `mtm/Cargo.toml` and settle
  `crates/mtm-feeds`.
- **No timeouts on any HTTP client in scope.** Provide one shared builder with
  a User-Agent and timeouts.
- **No SIGTERM handling in any service.**
- **Histogram buckets** (mtm-telemetry) affect every service's percentiles.
- **The default `expo = -8`** quantizes BONK to about 33 bps per step, which
  probably contributes to the router quarantining BONK pools.
