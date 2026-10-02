# Pricing types

Canonical price and observation types shared by the oracle and its consumers.

- `Price` — `crates/mtm-math`: fixed-point `mantissa * 10^expo`, i128 mantissa.
- `PricePoint` / `PriceData` / `PriceSource` — `crates/oracle-client/src/types.rs`.
- `Symbol` and `time::now_us()` — `crates/mtm-common`.

A `PricePoint` carries one observation: `symbol`, `data` (a `kind`-tagged
`PriceData`: `aggregate`, `top_of_book`, or `trade`), `publish_time_us`
(optional), `received_at_us`, `slot` (optional), and `source`.

## Rules

**No f64 for money.** Prices are `mantissa * 10^expo` everywhere except display.
Floats are only allowed inside the pricing engine's generators, and the result
is quantized to `Price` immediately (see instrument-pricing.md).

**i128 mantissa.** Pyth fits in i64, but other oracles don't (Switchboard
stores i128 at a fixed scale of 18; SOL at $97 is ~9.7e19 > i64::MAX).
Rescaling to fit i64 would silently truncate. `Price::to_pyth()` is the checked
narrowing for on-chain interop.

**Mantissas are JSON strings.** JSON numbers above 2^53 lose precision in JS.
Hermes uses the same convention.

**`conf` is `Option<Price>`.** Pyth provides it; most other sources don't. It
carries its own expo so cross-source conversion stays trivial.

**Timestamps are unix microseconds.** Sources range from seconds to µs; µs i64
covers all of them losslessly. `publish_time_us` is `None` when the source
doesn't stamp updates (Binance spot bookTicker); consumers fall back to
`received_at_us` rather than trusting a fabricated time.

**`slot` is the source's slot.** For Hermes that's a _Pythnet_ slot — never
compare it with Solana slots.

**No status field.** A halted market stops publishing; staleness (age of
`publish_time_us`) is the halt signal, and staleness policy lives in consumers.

**Symbols are config-driven.** Every venue names markets differently (Pyth hex
feed ids, `SOLUSDT`, coin ids…). The canonical `Symbol` is `BASE/QUOTE`, or the
mint address for testnet instruments; per-source ids live in
`[[oracle.underlyings]]`, not in types.

## Ingestion

| Source    | Wire format                                          | Rule                                                                    |
| --------- | ---------------------------------------------------- | ----------------------------------------------------------------------- |
| Hermes    | `price`/`conf` string mantissas, `expo` i32, seconds | parse string → i128; seconds × 1e6; `ema_price` → `ema`                 |
| Binance   | decimal strings, no timestamp on spot bookTicker     | parse string into fixed point → `top_of_book`; `publish_time_us = None` |
| CoinGecko | JSON floats, `last_updated_at` seconds               | `Price::from_f64_lossy` at expo −8; display-grade, cross-checks only    |
