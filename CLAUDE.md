# mtm — Claude Code notes

Solana **testnet** traffic-testing stack (testnet is the target, not devnet — the goal
is exercising testnet itself). Rust workspace; no on-chain programs yet (Pyth pull
oracle instead of a custom one — `programs/` is reserved).

## Commands

- `just check` / `just lint` / `just test` / `just fmt` — standard gates (rust + ts); CI runs `just ci`
- `just oracle [profile]` — run the oracle directly (profile: `local` | `testnet`)
- `just up [profile]` / `just down` / `just logs [svc]` — containerized stack
  (oracle + prometheus + grafana via ops/compose.yaml; local profile expects
  surfpool running on the host)
- `just bootstrap-tokens[-dev]` — token bootstrap via doppler-managed secrets

## Architecture rules

- Services must not depend on other service crates, only on `*-client` crates.
  Type homes: `Price` (i128 mantissa × 10^expo) in `crates/mtm-math`; `Symbol`,
  config loading, and `time::now_us()` in `crates/mtm-common`; the observation
  envelope (`PricePoint`/`PriceData`/`PriceSource`) in `crates/oracle-client`.
- Money math is fixed-point; never `f64` except display. Mantissas serialize as
  JSON strings. Timestamps are unix microseconds. See docs/pricing-types.md for
  the agreed design + per-source ingestion rules.
- Solana deps: granular crates only (solana-rpc-client etc.) — the
  solana-sdk/solana-client meta-crates have unresolvable version conflicts at 4.x.
  Keep the solana-rpc-client floor at 4.2.1 (4.1.x resolves a broken wincode mix)
  and pin majors to match the Agave 4.x CLI.
- When adding an on-chain program later: Anchor workspace under `programs/`, plus a
  `<name>-interface` crate in `crates/` shared by the program and off-chain code.
