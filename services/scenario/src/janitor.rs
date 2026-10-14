//! Close every account the scenario's DEX programs own, once the janitor
//! program (`programs/janitor`) has been deployed over them as an upgrade
//! (`scenario.sh teardown` with `SCENARIO_JANITOR=1` does both).
//!
//! The close instruction is a single byte, which the real DEX programs would
//! read as one of their own instructions, so nothing is sent to a program
//! unless its on-chain code is byte-for-byte the janitor binary given here.

use anyhow::{Context, Result};
use janitor_interface::{MAX_TARGETS_PER_TRANSACTION, close};
use mtm_chain::{ChainClient, SubmissionConfig};
use mtm_scenario::Scenario;
use serde_json::{Value, json};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use tracing::info;

use crate::rpc::JsonRpc;

/// `UpgradeableLoaderState::ProgramData` header: tag (4), deployment slot
/// (8), upgrade-authority option (1 + 32). The ELF follows, zero-padded to
/// the account's allocated length.
const PROGRAM_DATA_HEADER: usize = 45;
/// A `Close` of 24 accounts uses a few thousand compute units.
const CLOSE_CU_LIMIT: u32 = 100_000;
const LAMPORTS_PER_SOL: f64 = 1e9;

/// Is `program_data` (a ProgramData account's data) exactly `elf`?
pub fn runs(program_data: &[u8], elf: &[u8]) -> bool {
    program_data
        .get(PROGRAM_DATA_HEADER..)
        .and_then(|code| {
            let (head, padding) = code.split_at_checked(elf.len())?;
            Some(head == elf && padding.iter().all(|byte| *byte == 0))
        })
        .unwrap_or(false)
}

/// Fail unless `program` currently runs `elf`.
async fn ensure_janitor(chain: &ChainClient, program: &Pubkey, elf: &[u8]) -> Result<()> {
    let account = chain
        .rpc()
        .get_account(program)
        .await
        .with_context(|| format!("reading program {program}"))?;
    // Upgradeable program account: u32 tag + ProgramData address.
    let program_data = account
        .data
        .get(4..36)
        .and_then(|raw| Pubkey::try_from(raw).ok())
        .with_context(|| format!("{program} is not an upgradeable program"))?;
    let data = chain
        .rpc()
        .get_account(&program_data)
        .await
        .with_context(|| format!("reading {program}'s program data"))?
        .data;
    anyhow::ensure!(
        runs(&data, elf),
        "{program} is not running the janitor binary: deploy it over the program first"
    );
    Ok(())
}

/// Every account `program` owns, with its lamports.
async fn owned(rpc: &JsonRpc, program: &Pubkey) -> Result<Vec<(Pubkey, u64)>> {
    let accounts = rpc
        .call(
            "getProgramAccounts",
            json!([program.to_string(), {
                "encoding": "base64",
                "commitment": rpc.commitment,
                "dataSlice": { "offset": 0, "length": 0 },
            }]),
        )
        .await
        .with_context(|| format!("listing {program}'s accounts"))?;
    accounts
        .as_array()
        .context("getProgramAccounts: no array")?
        .iter()
        .map(|entry| {
            let key = entry["pubkey"]
                .as_str()
                .and_then(|key| key.parse().ok())
                .context("getProgramAccounts: bad pubkey")?;
            let lamports = entry["account"]["lamports"]
                .as_u64()
                .context("getProgramAccounts: bad lamports")?;
            Ok((key, lamports))
        })
        .collect()
}

/// Close everything the three DEX programs own, rent to `authority` (one
/// of the authorities compiled into `elf`).
pub async fn run(
    chain: &ChainClient,
    rpc: &JsonRpc,
    scenario: &Scenario,
    authority: &Keypair,
    elf: &[u8],
) -> Result<Value> {
    let programs = &scenario.programs;
    let venues = [
        ("orca-whirlpool", programs.orca_whirlpool),
        ("raydium-clmm", programs.raydium_clmm),
        ("manifest", programs.manifest),
    ];
    // Check every program first: a mismatch must stop the run before any
    // close is sent anywhere.
    for (_, program) in &venues {
        ensure_janitor(chain, program, elf).await?;
    }
    let config = SubmissionConfig {
        compute_unit_limit: CLOSE_CU_LIMIT,
        ..SubmissionConfig::default()
    };
    let mut report = serde_json::Map::new();
    let mut total = 0_u64;
    for (name, program) in venues {
        let accounts = owned(rpc, &program).await?;
        let lamports: u64 = accounts.iter().map(|(_, lamports)| lamports).sum();
        info!(
            venue = name,
            accounts = accounts.len(),
            "closing program accounts"
        );
        let keys: Vec<Pubkey> = accounts.iter().map(|(key, _)| *key).collect();
        for chunk in keys.chunks(MAX_TARGETS_PER_TRANSACTION) {
            chain
                .submit(
                    vec![close(program, authority.pubkey(), chunk)],
                    authority,
                    &[],
                    config,
                )
                .await
                .with_context(|| format!("closing {name} accounts"))?;
        }
        let left = owned(rpc, &program).await?.len();
        total += lamports;
        report.insert(
            name.to_owned(),
            json!({
                "closed": accounts.len(),
                "sol": lamports as f64 / LAMPORTS_PER_SOL,
                "remaining": left,
            }),
        );
    }
    Ok(json!({
        "authority": authority.pubkey().to_string(),
        "recoveredSol": total as f64 / LAMPORTS_PER_SOL,
        "programs": report,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_data_must_hold_exactly_the_janitor() {
        let elf = b"\x7fELF janitor".to_vec();
        let mut data = vec![3, 0, 0, 0];
        data.extend_from_slice(&[0; 41]);
        data.extend_from_slice(&elf);
        data.extend_from_slice(&[0; 100]); // padding up to the old program's size
        assert!(runs(&data, &elf));
        // a longer program whose prefix happens to match: still a different binary
        let mut other = data.clone();
        other[PROGRAM_DATA_HEADER + elf.len() + 3] = 1;
        assert!(!runs(&other, &elf));
        // a different binary
        let mut changed = data.clone();
        changed[PROGRAM_DATA_HEADER + 1] ^= 1;
        assert!(!runs(&changed, &elf));
        // too short to hold it
        assert!(!runs(&data[..PROGRAM_DATA_HEADER + 4], &elf));
    }
}
