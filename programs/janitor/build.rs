//! Compiles the authority set into the program.
//!
//! `JANITOR_AUTHORITIES` is a comma-separated list of base58 pubkeys (e.g.
//! the Doppler PUBKEY_PROGRAM_AUTHORITY and PUBKEY_FAUCET). It is required
//! for the SBF build; a host build (tests, IDE) without it compiles an empty
//! set, which rejects every signer.

use std::fmt::Write;

fn main() {
    println!("cargo:rerun-if-env-changed=JANITOR_AUTHORITIES");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let raw = std::env::var("JANITOR_AUTHORITIES").unwrap_or_default();
    let keys: Vec<[u8; 32]> = raw
        .split(',')
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(|key| {
            let bytes = bs58::decode(key).into_vec().unwrap_or_else(|error| {
                panic!("JANITOR_AUTHORITIES: `{key}` is not base58: {error}")
            });
            bytes
                .try_into()
                .unwrap_or_else(|_| panic!("JANITOR_AUTHORITIES: `{key}` is not a 32-byte pubkey"))
        })
        .collect();
    if keys.is_empty() && target_os == "solana" {
        panic!("JANITOR_AUTHORITIES must name at least one authority for the SBF build");
    }
    let mut out = String::from(
        "/// The authorities compiled in from JANITOR_AUTHORITIES.\npub const AUTHORITIES: &[[u8; 32]] = &[\n",
    );
    for key in &keys {
        writeln!(out, "    {key:?},").expect("write to string");
    }
    out.push_str("];\n");
    let path =
        std::path::Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("authorities.rs");
    std::fs::write(path, out).expect("write authorities.rs");
}
