# mtm — Claude Code notes

Solana **testnet** traffic-testing stack (testnet is the target, not devnet — the goal
is exercising testnet itself). Rust workspace; no on-chain programs yet (Pyth pull
oracle instead of a custom one — `programs/` is reserved).

## Commands

- `just check` / `just lint` / `just test` / `just fmt` — standard gates; CI runs `just ci`
- `just oracle [profile]`, `just arb`, `just mm` — run services (profile: `local` | `testnet`)

## Architecture rules

- Services must not depend on other service crates, only on `*-client` crates.
- Solana crates are pinned to major 4 to match the installed Agave 4.x CLI; bump
  `[workspace.dependencies]` in lockstep.
- When adding an on-chain program later: Anchor workspace under `programs/`, plus a
  `<name>-interface` crate in `crates/` shared by the program and off-chain code.
