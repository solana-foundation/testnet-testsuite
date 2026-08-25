# Testnet test suite

## Token bootstrap

`bootstrap/tokens/config.json` declares each mainnet mint and the names of the
environment variables that contain the required Solana keypairs. The deployment script
reads the mainnet mint and Metaplex metadata, then creates a classic SPL Token
mint and Metaplex metadata PDA on Solana testnet.

It never reads local keypair files and never prints secret material. The
`mintAuthority` keypair is the testnet transaction payer. The four configured
keypairs must be JSON-encoded Solana secret-key byte arrays in Doppler.

The default is a read-only dry run:

```sh
just bootstrap-tokens
```

The Justfile injects those variables using Doppler project `testnet-testsuite`
and configuration `prd`. To submit the simulated deployment, pass `--apply`:

```sh
just bootstrap-tokens --apply
```

Optional flags are `--config <path>`, `--mainnet-rpc <url>`, and
`--testnet-rpc <url>`. Both RPC defaults are Solana public endpoints.

The script deliberately creates a zero-supply mint. Recreating a mainnet supply
requires an explicit recipient and distribution policy, neither of which belongs
in this bootstrap configuration. The dry-run report includes the original supply
for audit purposes.
