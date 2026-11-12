# manifest.json provenance

The Shank IDL that `codegen/generate.mjs` turns into this crate's
`src/generated` (see the fixups applied there).

- SHA-256: `8356c3d05077335ef1c230c2c826937105ff6b2de0a025ccf89cc06265485dad`
- Program: `MNFSTqtC93rEfYHB6hF82sKdZpUDFWkViLByLd1k1Ms` (`metadata.address`),
  IDL `version` 1.0.0.
- Source: Manifest's published `client/idl/manifest.json` from a release
  before the deployed fork's base commit. The fork is pinned at
  `5718d3eed80fa7472a826f346c0668644ff32138` (see `docs/dex-forks.md`).

## Difference from the fork's IDL at 5718d3e

Checked 2026-10-14 against `client/idl/manifest.json` at `5718d3e`
(SHA-256 `d5885040eec224720b12575d324d8af4cb520bdeede1927a23c4c14593c0ebe0`):
every instruction, account, and type matches except `MarketFixed`, whose
last 64 bytes (offset 192..256) are `padding3: [u64; 8]` here and
`baseGlobal: publicKey, quoteGlobal: publicKey` at `5718d3e`. The total size
(256 bytes) and every field before offset 192 are identical, and
`manifest-dex-client` reads nothing at or after offset 192, so decoding is
unaffected. Regenerating from the fork's IDL would expose the two global
addresses.

When the IDL changes, update the SHA-256 above, regenerate (`just gen-sdks`),
and commit the generated code with it.
