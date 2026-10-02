# Instrument registry & price generation

Maps testnet SPL mints to prices. Some instruments track real markets 1:1; the
rest get derived or synthetic prices. Lives in `services/oracle/src/pricing`
and serves results through the normal `PricePoint` API.

```
underlyings (hermes, binance, coingecko) ──ticks──▶ engine ──PricePoint──▶ HTTP/WSS
                                                      ▲
                                        instrument registry (config)
```

- **Underlyings** (`[[oracle.underlyings]]`) — real market data inputs.
- **Instruments** (`[[oracle.instruments]]`) — what we serve: a base source plus
  an ordered list of transforms. For testnet tokens, set `mint` and omit
  `symbol`; the mint address becomes the symbol (avoids duplicate tickers), so
  `Symbol` is case-sensitive. Readable symbols are for local dev.
- **Engine** — re-evaluates instruments on underlying ticks (or on its own timer
  for synthetic bases) and quantizes to `Price` at the instrument's `expo`
  (default −8).

See `config/default.toml` for working examples of every kind.

## Base sources

| kind         | semantics                                                    | use                      |
| ------------ | ------------------------------------------------------------ | ------------------------ |
| `underlying` | passthrough (top-of-book reduces to mid, conf = half-spread) | 1:1 majors               |
| `cross`      | `base / quote`                                               | e.g. BTC priced in SOL   |
| `basket`     | Σ weightᵢ · feedᵢ, optionally rebased on first evaluation    | index tokens             |
| `peg`        | constant target                                              | test stablecoins         |
| `gbm`        | geometric Brownian motion on its own clock, seeded           | synthetic tokens (memes) |

## Transforms (applied in order)

| kind     | semantics                               | use                          |
| -------- | --------------------------------------- | ---------------------------- |
| `scale`  | `factor · P + offset`                   | cheap variants               |
| `invert` | `1 / P`                                 | quote inversion              |
| `beta`   | `initial · (P / anchor)^β`              | leveraged / inverse trackers |
| `lag`    | `P(t − ms)`                             | stale-price simulation       |
| `noise`  | `P · (1 + ε)`, ε mean-reverting, seeded | venue jitter, depeg wobble   |

Two instruments off the same underlying with different `lag`/`noise` produce
**known arbitrage opportunities**, so tests can assert the arb bot captures
them instead of hoping testnet produces some.

## Determinism & floats

`gbm`, `noise`, and `beta` need `exp`/`sqrt`/`powf`, so floats are allowed
inside generators only; every output is quantized to `Price` immediately. All
randomness is seeded per instrument (`ChaCha8Rng`), so the same config and the
same underlying ticks give the same synthetic prices. Generator state is
in-memory; a restart re-anchors at `initial`.

## Source and conf

Plain passthroughs keep the underlying's `source`; anything transformed or
synthetic is `derived`. `conf` scales with the underlying's conf when it has
one, `gbm` synthesizes it from its configured vol, and `peg` has none.

## API

- `GET /v1/price?symbol=…` or `?mint=…`
- `GET /v1/instruments` — symbol, mint, base kind, transform kinds

## Later

- `jump` transform (scheduled/Poisson jumps, vol regimes, halts)
- `replay` base (historical ticks from a fixture file)
- admin endpoint to trigger crash/halt/vol-spike scenarios on a live instrument
