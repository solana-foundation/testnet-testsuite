# DEX program forks for testnet

Every DEX this suite exercises is a self-deployed fork: the canonical mainnet
program IDs do not exist on testnet (or are squatted), and several programs
compile their admin keys into the binary. Each fork carries the smallest
possible patch on top of a pinned upstream commit. The off-chain crates in this
repo are pinned to versions that match those commits.

All program and admin keys come from Doppler (`testnet-testsuite` project,
`dev` = local Surfpool, `prd` = testnet). Public keys are derived from the
keypair secrets; never commit a keypair.

## Raydium CLMM

Already forked at
[solana-foundation/raydium-clmm](https://github.com/solana-foundation/raydium-clmm);
see [BOOTSTRAP.md](../BOOTSTRAP.md) and the Raydium sections of the README.

## Orca Whirlpools

### Licensing: pin the last Apache-2.0 commit

Orca relicensed the repository on **2025-02-27** from Apache-2.0 to the
non-standard "Orca License". That license only grants non-commercial use and
excludes any use that "does or could benefit, directly or indirectly, a
competitor" (DEXs, AMMs, aggregators, ...). A testnet traffic suite whose
router composes swaps across DEXs is close enough to that line that we do not
build on post-relicense code.

The fork therefore branches from **`e528dd23bb41571f92cfdb49a2f15d4fa0b01bec`**
(2025-02-26, the last commit released under Apache-2.0), and the Rust crates
are pinned to the matching Apache-2.0 releases:

| Crate | Version | Published | License |
|---|---|---|---|
| `orca_whirlpools_client` | `=2.0.2` | 2025-02-21 | Apache-2.0 |
| `orca_whirlpools_core` | `=1.0.4` | 2025-02-21 | Apache-2.0 |

Do not merge or cherry-pick later upstream commits into the fork, and do not
bump either crate, without a fresh licensing decision.

### Create the fork

```sh
gh repo fork orca-so/whirlpools --org solana-foundation --clone=false
git clone git@github.com:solana-foundation/whirlpools.git
cd whirlpools
git switch -c testnet e528dd23bb41571f92cfdb49a2f15d4fa0b01bec
```

Keep `testnet` as the fork's default branch so its README and LICENSE are the
Apache-2.0 ones.

### Code changes

At this commit the program has **no compiled-in admin**: `initialize_config`
is permissionless and the config's fee authority is set by its arguments
(`orca-whirlpool-orchestrator admin` makes it `KEYPAIR_PROGRAM_AUTHORITY`).
The only required change is the program ID.

1. `programs/whirlpool/src/lib.rs`:

   ```diff
   -declare_id!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc");
   +declare_id!("<public key of KEYPAIR_ORCA_WHIRLPOOL>");
   ```

2. `Anchor.toml` (keeps `anchor` tooling and tests consistent):

   ```diff
    [programs.localnet]
   -whirlpool = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc"
   +whirlpool = "<public key of KEYPAIR_ORCA_WHIRLPOOL>"
   ```

Derive the public key without writing the secret to disk:

```sh
doppler run --project testnet-testsuite --config prd -- \
  bash -c 'solana-keygen pubkey <(printf %s "$KEYPAIR_ORCA_WHIRLPOOL")'
```

Optional, cosmetic only: `programs/whirlpool/src/constants/nft.rs` points
position-NFT metadata URIs at `metadata.orca.so`. The orchestrator opens
positions without the metadata extension by default, so leaving them is
harmless; change them if `--with-metadata` is used on testnet.

Commit the two-line patch as a single commit, e.g.
`chore: testnet program id`, so rebasing onto a later (licensed) base stays
trivial if that is ever approved.

### Build

The snapshot predates current platform tools: its lockfile pins
`solana-program =1.17.22`, whose `ahash 0.8.5` needs a rustc that still has
the `stdsimd` feature (removed in Rust 1.78). Build with platform-tools
**v1.41** (rustc 1.75), which any recent `cargo build-sbf` can fetch:

```sh
cargo build-sbf --tools-version v1.41 \
  --manifest-path programs/whirlpool/Cargo.toml
# → target/deploy/whirlpool.so
```

Upstream CI used Solana 1.17.25, Anchor 0.29.0, and rustc 1.84.0 for host
tooling; `anchor build -p whirlpool` with Anchor 0.29.0 also works but is not
required.

### Deploy and configure

```sh
NO_DNA=1 just deploy-program-dev "KEYPAIR_ORCA_WHIRLPOOL" "../whirlpools/target/deploy/whirlpool.so"
NO_DNA=1 just configure-orca-whirlpool-dev
NO_DNA=1 just orca-whirlpool-create-pools-dev
NO_DNA=1 just orca-whirlpool-user-flow-dev
```

Use `deploy-program-testnet`, `configure-orca-whirlpool-prd`, and the
`-testnet` recipes for testnet.
