# Router architecture

**Status: design 2026-08-31; implementation retargeted 2026-10-10** (see
"Current implementation" below; sections 1–9 remain the design of record).

A swap router: given `(input_mint, output_mint, amount, ExactIn|ExactOut)`,
return a transaction whose instructions route through the best venues/pools.
Covers the full stack: market-state ingestion → indexing per venue class →
routing → transaction assembly under Solana's hard limits.

```
                       ┌────────────────────── services/router ──────────────────────┐
 accounts feed         │                                                              │
 (ws / geyser)  ──────▶│  market-state ──▶ token graph ──▶ route search ──▶ assembler │──▶ tx / ixs
                       │   (shared lib)     + venue         (constrained)    (ALTs,   │
 pool registry  ──────▶│                    adapters                         budgets) │
 (config/xtask)        │        ▲                                              │      │
                       └────────┼──────────────────────────────────────────────┼──────┘
                                │                                              ▼
                          also embedded by                              execution feedback
                          arb / mm bots                                 (quoted vs landed)
```

## Current implementation (2026-10-10)

The M2 router (stash `M2 router work (multi-hop + vetting)`) targeted Raydium
CPMM and Manifest through a websocket market-state cache. It now routes across
the three venues the suite actually deploys, all self-hosted forks:

| Venue          | Client crate            | Swap instruction                       | Quote math                                              |
| -------------- | ----------------------- | -------------------------------------- | ------------------------------------------------------- |
| Raydium CLMM   | `raydium-clmm-client`   | `swap_v2` (`SwapSingleV2`)             | Raydium's own tick walk (`quote.rs`)                    |
| Orca Whirlpool | `orca-whirlpool-client` | `swap_v2` + 2 supplemental tick arrays | `orca_whirlpools_core`, bounded at the loaded arrays    |
| Manifest       | `manifest-dex-client`   | `SwapV2`, seatless, no global accounts | book walk mirroring `place_order` / `impact_base_atoms` |

What changed relative to the design below:

- **Snapshot-at-quote instead of a pushed cache (§2).** Each venue client
  exposes a swap snapshot: one RPC fetch, then pure quoting and instruction
  building for any owner. The router snapshots every candidate pool once per
  request and runs the whole search on them. Quoted == executed held to the
  atom on Surfpool for direct splits, 2-hop bridges, and exact-out. The
  ws/geyser cache (and CLMM tick-window subscriptions) is the latency step
  that replaces the per-request fetch later; `mtm-market-state` from the stash
  is parked until then.
- **Venue trait (§3)** is `mtm_venues::PoolSnapshot` (enum dispatch over the
  three snapshots) plus the engine's `QuoteSource` trait, which lets the
  planner's unit tests run on a constant-product mock.
- **Discovery (§2)** sweeps each venue program by account discriminator
  (Raydium `PoolState`, Orca `Whirlpool`, Manifest `MarketFixed`). Loading a
  pool through its client is the structural check; a two-way probe quote of
  `probe_amount` atoms is the functional check. Failures stay `quarantined`
  and re-vet every sweep. The stash's freeze-authority rule is dropped: our
  own bootstrap mints carry a freeze authority.
- **Budgets are planner inputs (§1, §5).** Until ALTs exist, assembly
  narrows same-pair splits (3 → 2 → 1 legs) and then excludes the widest leg
  until the transaction fits 1,232 bytes. A 3-venue split serializes to
  ~1,360 bytes and a Raydium+Orca 2-hop to ~1,270, so ALTs remain the M3
  priority.
- **Execution modes.** `execution = "direct"` emits each leg's own top-level
  venue instruction (conservative chaining, §8.3). `execution = "jupiter"`
  emits one `route_v2` of a Jupiter aggregator fork, described next.

### Jupiter-fork execution

`mtm_venues::jupiter` builds `route_v2` from the mainnet Jupiter v6 IDL
(discriminator, account order, `RoutePlanStepV2 { swap, bps, input_index,
output_index }`). Each leg becomes a step whose accounts are the AMM program
ID followed by that AMM's swap accounts, the convention of
`jupiter-amm-interface` adapters. Splits become sibling steps with bps from a
largest-remainder split; hops chain through the user's intermediate ATAs and
the aggregator forwards actual amounts, so jupiter routes quote hop 2 at hop
1's expected output and apply slippage once.

Placeholders to confirm against the fork before enabling it:

1. `[router.jupiter] program_id` (Doppler `PUBKEY_JUPITER_FORK`); without it
   jupiter requests return 400.
2. `Swap` variant indices: `RaydiumClmmV2 = 40`, `WhirlpoolSwapV2 = 47`,
   `Manifest = 116` on mainnet. A fork that adds or removes variants shifts
   them.
3. The fork must accept our fork program IDs; mainnet Jupiter pins each
   variant to the canonical AMM.
4. Per-variant account counts, especially the Raydium tick-array tail.
5. ExactOut (`exact_out_route_v2`) is not wired; such requests return 400.

## 1. API surface (`crates/router-client` owns wire types)

```
POST /v1/quote
  { input_mint, output_mint, amount, mode: "exact_in"|"exact_out",
    slippage_bps, max_accounts?, max_hops?, venues?: allow/deny,
    user?: pubkey }                    # if present, response includes the tx
→ { quote_id, in_amount, out_amount, min_out_amount, price_impact_bps,
    route: [ { venue, pool, in_mint, out_mint, in_amount, out_amount,
               fee_amount, cu_estimate, writable_accounts } ],
    state_slot, oracle_divergence_bps?,          # sanity signal, see §6
    tx?: base64 versioned tx, alt_addresses?: [] }

POST /v1/swap          # quote_id (or inline quote params) + user pubkey → tx
POST /v1/instructions  # same, but raw instructions + ALT list — for callers
                       # composing their own tx (arb bot, CPI-style flows)
GET  /v1/venues, /v1/pools   # registry introspection
```

Design points:

- **`max_accounts` and `max_hops` are router INPUTS, not post-hoc checks** —
  the search must treat tx budgets as constraints (§5), because a "best" route
  that doesn't fit in a transaction is not a route.
- Amounts are u64 atoms in the wire (mint decimals from registry); all
  internal math fixed-point per repo rules.
- **ExactIn is the primary mode.** ExactOut v1 is restricted to single-hop
  direct pairs (inverse math per venue; multi-hop ExactOut needs
  chain-backward simulation and is deferred).
- Two-phase (quote → swap) with a short-TTL quote cache keyed by `quote_id`,
  plus one-shot (`user` in the quote call) for bots.

## 2. Market-state ingestion

### Transport (in order of adoption)

1. **WebSocket `accountSubscribe`** via a backend trait in `mtm-chain`
   (`AccountStream`), initial snapshot via `getMultipleAccounts`. Sufficient
   for our testnet world: we create the pools, so the account universe is
   hundreds, not millions.
2. **Yellowstone/Geyser gRPC** as the second backend behind the same trait —
   the scale/latency answer. Requires a geyser-enabled testnet node
   (infra ask, open decision §8). Slot-tagged updates come for free.
3. RPC polling only as a degraded fallback / gap-filler after ws reconnects.

### Market discovery & lifecycle

We know our _venue programs_, but market/pool creation on them is
permissionless (anyone can `CreateMarket` on Manifest or `initialize` a CPMM
pool once the amm_config exists) — and other testnet actors joining is the
point of the exercise. So the registry is a **policy overlay**, not the source
of truth; discovery is a first-class subsystem of `mtm-market-state`.

**Detection — three mechanisms, layered by latency:**

1. **Creation-event stream (primary, near-real-time)**: subscribe to venue
   program logs (`logsSubscribe` mentions-filter per program id; geyser
   transaction stream when available), detect creation instructions by
   discriminator (CPMM `initialize` 8-byte anchor disc; Manifest
   `CreateMarket = 0` / its `CreateMarketLog`), decode via the codama SDKs we
   already generate. One `Discoverer` impl per venue class, sibling to `Venue`.
2. **gPA reconciler (gap-filler, minutes cadence)**: per-venue
   `getProgramAccounts` with dataSize/memcmp-discriminator filters, diffed
   against the known set — catches anything the ws stream dropped.
3. **Registry/bootstrap (curated seed)**: `xtask bootstrap-dex` output; these
   markets skip vetting (we made them) and land directly in the top trust tier.

**Vetting — a discovered market is not a routable market.** Pipeline before an
edge activates in the graph:

- structural: account owner == expected venue program, layout
  version/discriminator valid, vault PDAs derive correctly;
- token-level: mint decimals sane, **freeze authority** flagged (freezable
  intermediate mints can brick a route mid-flight), Token-2022 extension
  screening (transfer fees change quoting math; dangerous extensions →
  deny), oracle coverage noted (mints in our instrument registry get the
  divergence guard; unknown mints don't — that lowers their tier);
- functional: probe quotes both directions through the adapter math against a
  live snapshot, plus (when artifacts are loaded) a LiteSVM dry-run swap —
  the market must actually function;
- liquidity: minimum depth threshold before activation.

**Trust tiers drive routing policy**: `curated` (bootstrap) →
`verified` (passed vetting) → `quarantined` (new/failed/one-sided) →
`denied`. Default routing uses curated+verified; requests may opt into
quarantined for testing. Tier is per-edge metadata in the graph.

**Lifecycle state machine** (per market):
`discovered → vetting → active ⇄ degraded (stale/empty/one-sided) → dormant → delisted`.
Dormant markets release their hot subscriptions (the subscription budget is
finite) and keep a cold graph edge; periodic cold probes can reactivate.
Delisting on: mint frozen, pool drained + inactive N hours, or explicit
denylist. All transitions emit `MarketEvent`s on an internal bus consumed by
the graph, the subscription manager, and metrics.

**Persistence**: discovered-market cache (JSON snapshot or sqlite, gitignored)
so restarts don't re-vet the world from a cold gPA sweep; the registry file
overlays allow/deny/curation on top.

**ALT interaction**: discovered pools aren't in the xtask-managed ALTs. They
remain routable at raw-address cost (32B/account — fine single-hop, painful
multi-hop); an **ALT-maintainer task** extends the venue ALT when a market
reaches `verified` (needs custody of the ALT authority key — see §8). The
assembler always verifies resolved addresses, so a stale/wrong table entry is
detectable, not exploitable.

**Discovery as a test surface**: same ground-truth philosophy as the lag/noise
arb pairs — `xtask spawn-junk-markets` creates adversarial fixtures (freezable
mints, absurd decimals, one-sided books, dust liquidity) and the suite asserts
the vetting pipeline quarantines/denies them. Discovery correctness becomes
testable instead of hoped-for.

### Per-venue account sets (what we actually subscribe to)

| Venue class      | Hot accounts per pool                                                                          | Notes                                                                                                                          |
| ---------------- | ---------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| CPMM             | pool_state + 2 vaults                                                                          | fixed, tiny; amm_config/observation are slow-moving                                                                            |
| CLMM             | pool_state + tick-array **window** + bitmap ext                                                | the window _moves with price_ — the subscription manager must re-subscribe as the active tick migrates; account set is dynamic |
| CLOB (Manifest)  | **one market account** (entire book lives in one expandable account) + global accounts if used | best-case ingestion: one sub per market; worst-case parsing (§8.1)                                                             |
| PropAMM (future) | adapter-defined                                                                                | the trait carries this                                                                                                         |

Every cached account is tagged `(slot, received_at_us)`. A route is computed
against a snapshot view; quotes carry `state_slot` and per-venue staleness
bounds reject state older than N slots at quote time.

## 3. Indexing: token graph + venue adapters

**Token graph**: nodes = mints; edges = (venue, pool) — a multigraph, many
edges per pair. Adjacency indexed both directions. Edge metadata: depth score
(for pruning), fee tier, footprint summary.

**The `Venue` trait is the core abstraction** (one adapter crate per venue
class, depending on the codama SDK crates):

```rust
trait Venue {
    fn quote_exact_in(&self, state: &Snapshot, amount: u64) -> Quote;
    fn quote_exact_out(&self, state: &Snapshot, amount: u64) -> Option<Quote>;
    /// marginal price after consuming `amount` — drives split optimization
    fn marginal_after(&self, state: &Snapshot, amount: u64) -> Price;
    fn build_swap_ix(&self, leg: &RouteLeg, user: &Address) -> Instruction;
    /// tx-budget inputs: accounts (writable/readonly), data bytes, CU
    fn footprint(&self, leg: &RouteLeg) -> Footprint;
}
```

`Quote` = out amount, fee, price impact, **and the footprint** — because for
CLMM the account count and CU grow with trade size (each tick-array crossed is
another writable account), the footprint is a function of the quoted amount,
not a constant. That coupling is why footprint lives on the quote path.

**Quoting is simulation-exact, never oracle-based**: each adapter reimplements
the venue's on-chain integer math bit-for-bit (CPMM constant-product u128;
CLMM sqrt-price Q64.64 tick walk; CLOB book walk in atoms). The oracle's price
is a _sanity reference_, not an input. Correctness contract: **quoted out ==
executed out, exactly** — enforced by golden tests (§7).

## 4. Routing algorithm (staged)

- **v1 — best single venue**: enumerate direct edges, quote each at full
  amount, pick max. Trivial, correct, ships the whole pipeline end-to-end.
- **v1.5 — single-pair splits**: out(in) per venue is concave ⇒ greedy chunk
  allocation by marginal price (equalize marginal rates across venues)
  converges to the optimal split. Chunk count bounded by footprint budget
  (each split leg costs accounts/bytes/CU).
- **v2 — multi-hop**: bounded path enumeration (max 2–3 hops) through
  configured bridge mints (our tUSDC/tSOL), then split optimization across
  candidate paths: iteratively allocate amount chunks to the path with best
  marginal rate, re-simulating (Jupiter-style). Path pruning by edge depth
  scores keeps enumeration small; our graph is tiny (dozens of mints) so
  exhaustive-within-bounds is fine — the structure matters for when it isn't.
- **Slippage**: `min_out` on the final leg only; intermediate legs get
  conservative amount buffers (see §5 chaining caveat).

Heuristic hooks per venue class live in the adapter (e.g. CLOB: don't quote
through crossed/empty books; CLMM: cap ticks-crossed per leg both for CU and
for sanity; all: freshness gates).

## 5. Transaction assembly under Solana constraints

Budget table the assembler enforces (verify exact current values at
implementation start — marked ⚠ where consensus limits have moved before):

| Limit                     | Value                                                                    | Impact                                                                         |
| ------------------------- | ------------------------------------------------------------------------ | ------------------------------------------------------------------------------ |
| Packet size               | **1232 bytes** total (sigs + message + ALT refs)                         | ALT ref = 1 byte/account vs 32 raw; multi-hop without ALTs is hopeless         |
| Account locks/tx          | **64** ⚠ (raise to 128 landed behind a feature gate — verify on testnet) | dedup accounts across legs; writable set is the scarce resource                |
| CU/tx                     | **1.4M**                                                                 | CLMM tick walks are the hog; per-leg CU estimates from measurement (§7)        |
| CU/writable-account/block | ~12M ⚠                                                                   | hot-pool throughput cap under traffic-gen load — relevant to _us_ specifically |
| ALT                       | 256 addresses/table; extensions usable next slot                         | warmup matters for freshly-bootstrapped pools                                  |

Assembly pipeline:

1. Route plan → legs → concrete account lists (dedup: user ATAs, token
   programs, shared configs appear once).
2. **ATA preflight**: check user's ATAs for intermediate/output mints; prepend
   `create_associated_token_account_idempotent` (bytes + CU they cost are part
   of the budget); wSOL wrap/unwrap legs where SOL is an endpoint.
3. Resolve against **our ALTs**: `xtask bootstrap-dex` creates/extends one ALT
   per venue (pool accounts, vaults, configs) at pool-creation time; router
   config lists them. User-specific accounts (ATAs) ride raw.
4. Compute budget ixs (CU limit from summed leg estimates + margin; priority
   fee via mtm-chain policy), then legs in order.
5. Fit check — **which should never fail**, because the search already
   received the budgets. Assembly-time overflow is a bug, not a fallback path.

**The chaining caveat** (drives a future decision): with off-chain composed
transactions, leg N's input amount is fixed at build time from simulation, so
intermediate legs need slippage buffers and dust accumulates. The robust
answer is a thin **on-chain router program** (forwards actual received balance
into the next leg, enforces final min_out atomically) — this is the first real
tenant for `programs/` when we commit to it. v1 ships without it; the API
shape doesn't change.

## 6. Service shape

- **`crates/mtm-market-state`** — the ingestion + cache + token graph as a
  LIBRARY, embedded by the router _and directly by the bots_ (the arb bot
  wants the same books/pools without an HTTP hop). One subscription manager,
  typed per-venue state, snapshot views. This is the biggest reuse decision in
  the design.
- **`crates/venues/*`** (or one `mtm-venues` crate, modules per class) —
  `Venue` impls over the codama SDKs.
- **`services/router`** — axum service: quote engine + assembler + registry
  API; same config/telemetry patterns as the oracle.
- **`crates/router-client`** — wire types + typed client (convention).
- Oracle integration: router pulls oracle mid-prices to compute
  `oracle_divergence_bps` per quote — a testnet-grade sanity guard (flag or
  refuse routes wildly off oracle mid; catches broken pool state, decimals
  bugs, stale books).
- Metrics: quote latency, state staleness per venue, route-shape distribution,
  and above all **quoted-vs-landed deviation** (execution feedback from bot
  fills) — the router's single quality number.

## 7. Testing strategy

1. **Golden math tests vs LiteSVM**: load the real program `.so` artifacts
   (artifact pipeline), construct pool/book states, assert
   `adapter.quote_exact_in(x) == on-chain executed out` across randomized
   amounts and states. This is the load-bearing test — it pins our simulators
   to the deployed bytecode version, and re-runs whenever `programs.toml`
   pins move.
2. CU measurement harness in the same LiteSVM setup → per-venue CU estimate
   tables consumed by the assembler (measured, not guessed).
3. Packing property tests: generated route plans never exceed the budget table.
4. E2E on surfpool with **cloned mainnet pools** — realistic liquidity shapes
   (real Raydium/Manifest state) without deploying anything.
5. Determinism: same snapshot + same request ⇒ identical route (no wall-clock
   or randomness in the search).

## 8. Open decisions

1. **Manifest book readback is now unavoidable** (the router must walk the
   book; fill-tracking doesn't cut it). Options: (a) clean-room hypertree
   reader from the on-chain layout (Apache-clean, most work), (b) GPL-isolated
   `manifest-book` crate porting their reader (fine per our licensing analysis
   — Apache repo, public source; binaries linking it stay OSS), (c) their TS
   SDK sidecar (no). Recommend **(b)**: isolate, mark GPL, move fast; revisit
   clean-room only if licensing posture changes.
2. **Geyser on testnet**: infra ask (a geyser-plugin node or Yellowstone
   endpoint). Start ws-only; the `AccountStream` trait keeps the swap cheap.
3. **On-chain router program** (chaining, §5): **decided at M2 — defer.**
   Multi-hop ships with conservative chaining instead: the slippage budget is
   split across hops, hop 2's input is fixed at hop 1's on-chain minimum, and
   anything received above that minimum stays in the intermediate ATA as
   dust. On a testnet traffic stack the dust is our own tokens flowing back
   through our own bots, so the cost is a bookkeeping smell, not a loss.
   Revisit when the bots can measure the dust rate under real traffic — that
   measurement is the M3 execution-feedback loop's job.
4. **State layer as shared lib** (`mtm-market-state`): recommended strongly;
   the alternative (router-internal) forces the arb bot to duplicate
   subscriptions or eat an API hop.
5. Verify the ⚠ limits (account locks 64 vs 128, per-account block CU) on
   testnet at M0.
6. **ALT authority custody**: the ALT-maintainer needs the authority keypair
   (doppler). Router service holds it vs an xtask cron — service is more
   responsive, cron is a smaller trust surface. Lean: cron first.
7. **Discovery persistence**: JSON snapshot (simple, greppable) vs sqlite
   (query-able history). Lean: JSON snapshot until the market count argues
   otherwise.

## 9. Milestones

- **M0 — pipeline skeleton**: `mtm-market-state` (ws backend, CPMM accounts),
  CPMM adapter with golden tests, single-hop `/v1/quote` + `/v1/swap` returning
  a working tx on surfpool. Everything end-to-end, one venue, no splits.
- **M1 — CLOB + splits**: Manifest adapter (book decision executed), ALT
  bootstrap in xtask, single-pair splits, `/v1/instructions`. **Passive
  discovery**: creation-event stream + gPA reconciler feeding the cache,
  everything lands `quarantined` (visible, not routed).
- **M2 — multi-hop + ExactOut(single-hop)** _(done)_: bridge-mint paths
  (`[router] bridge_mints`, 2-hop, conservative chaining per §8.3), on-chain
  router decided (defer). **Active discovery** _(done)_: vetting pipeline
  (`mtm-market-state::vetting` — freeze-authority/Token-2022/mint-owner checks
  deny or quarantine, functional probes + liquidity floors quarantine;
  quarantined markets re-vet every sweep so transient failures heal),
  `auto_promote` config, junk-market test fixtures. In lieu of ALTs, assembly
  enforces a hard 1232-byte serialize guard; ALT-maintainer + xtask ALT
  bootstrap moved to M3 (blocked on xtask revival + authority custody, §8.6).
  Geyser backend and quote cache deferred: no geyser endpoint on our testnet
  infra yet, and quoting is pure in-memory math over pushed state — there is
  no RPC call to cache until that changes.
- **M2.5 — three deployed venues** _(done, 2026-10-10)_: Raydium CLMM, Orca
  Whirlpool, and Manifest via venue snapshots; discriminator discovery with
  probe vetting; footprint-aware fitting; Jupiter-fork `route_v2` assembly
  behind the placeholders above; Surfpool end-to-end runs for split, bridge,
  and exact-out routes (`services/router/examples`).
- **M3 — hardening**: execution feedback loop, CLMM adapter (tick-window
  subscriptions), prop-AMM slot in the trait proven by a third venue;
  lifecycle degradation/dormancy tuning under sustained traffic.
