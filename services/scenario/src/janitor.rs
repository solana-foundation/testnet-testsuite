//! Close every account the scenario's DEX programs own, once the janitor
//! program (`programs/janitor`) has been deployed over them as an upgrade
//! (`scenario.sh teardown` with `SCENARIO_JANITOR=1` does both).
//!
//! Two passes per run. First the token accounts the programs' PDAs hold
//! (pool and market vaults): every token account of a scenario mint is
//! listed, and each one whose owner is a PDA in the per-venue seed map
//! below is burned and closed with `CloseTokenAccounts`, the janitor
//! signing as that PDA. The seed recipes are the only protocol-specific
//! part; the program re-derives and checks every one. Then every account
//! the programs own, with `Close`.
//!
//! The close instruction is a single byte, which the real DEX programs would
//! read as one of their own instructions, so nothing is sent to a program
//! unless its on-chain code is byte-for-byte the janitor binary given here.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result};
use janitor_interface::{
    MAX_TARGETS_PER_TRANSACTION, MAX_TOKEN_ACCOUNTS_PER_TRANSACTION, close, close_token_accounts,
};
use mtm_chain::{ChainClient, SubmissionConfig};
use mtm_scenario::{Scenario, Venue};
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

/// The slot a ProgramData account was last deployed (or upgraded) in.
fn deployment_slot(program_data: &[u8]) -> Option<u64> {
    Some(u64::from_le_bytes(
        program_data.get(4..12)?.try_into().ok()?,
    ))
}

/// Fail unless `program` currently runs `elf`; returns the slot it was
/// deployed in.
async fn ensure_janitor(chain: &ChainClient, program: &Pubkey, elf: &[u8]) -> Result<u64> {
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
    deployment_slot(&data).with_context(|| format!("{program}'s program data has no slot"))
}

/// A program upgraded in slot N only runs its new code from slot N + 1
/// (invoking it earlier fails with UnsupportedProgramId): wait until the
/// cluster is past `slot`.
async fn wait_past(chain: &ChainClient, slot: u64) -> Result<()> {
    let started = std::time::Instant::now();
    while chain.current_slot().await? <= slot {
        anyhow::ensure!(
            started.elapsed() < std::time::Duration::from_secs(60),
            "the cluster did not pass slot {slot} within 60 s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
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

/// A PDA that may hold vaults: its program and its seeds (bump last).
struct VaultAuthority {
    venue: &'static str,
    program: Pubkey,
    seeds: Vec<Vec<u8>>,
}

fn pda(venue: &'static str, program: Pubkey, seeds: &[&[u8]]) -> (Pubkey, VaultAuthority) {
    let (address, bump) = Pubkey::find_program_address(seeds, &program);
    let mut seeds: Vec<Vec<u8>> = seeds.iter().map(|seed| seed.to_vec()).collect();
    seeds.push(vec![bump]);
    (
        address,
        VaultAuthority {
            venue,
            program,
            seeds,
        },
    )
}

/// The PDAs that may hold the scenario's vaults, by venue:
///
/// - Orca: the whirlpool, `["whirlpool", config, mint_a, mint_b, tick
///   spacing (u16 LE)]`;
/// - Raydium CLMM: the pool, `["pool", amm_config, mint_0, mint_1]`, and the
///   vault itself, `["pool_vault", pool, mint]` (either may hold it; the
///   on-chain owner decides);
/// - Manifest: markets are keypair accounts, so each vault holds itself:
///   `["vault", market, mint]`, for every market the program owns.
///
/// Mints are ordered as the venues order them (`a < b`).
fn vault_authorities(
    scenario: &Scenario,
    markets: &[Pubkey],
) -> Result<HashMap<Pubkey, VaultAuthority>> {
    let programs = &scenario.programs;
    let mut out = HashMap::new();
    for pool in &scenario.config.pools {
        let (base, quote) = (
            scenario.token(&pool.base)?.mint,
            scenario.token(&pool.quote)?.mint,
        );
        let (a, b) = if base < quote {
            (base, quote)
        } else {
            (quote, base)
        };
        match pool.venue {
            Venue::OrcaWhirlpool => {
                let spacing = programs.orca_tick_spacing.to_le_bytes();
                let config = programs.orca_whirlpools_config;
                out.extend([pda(
                    "orca-whirlpool",
                    programs.orca_whirlpool,
                    &[
                        b"whirlpool",
                        config.as_ref(),
                        a.as_ref(),
                        b.as_ref(),
                        &spacing,
                    ],
                )]);
            }
            Venue::RaydiumClmm => {
                let config = programs.raydium_amm_config;
                let program = programs.raydium_clmm;
                let (pool_key, pool_pda) = pda(
                    "raydium-clmm",
                    program,
                    &[b"pool", config.as_ref(), a.as_ref(), b.as_ref()],
                );
                out.insert(pool_key, pool_pda);
                for mint in [a, b] {
                    out.extend([pda(
                        "raydium-clmm",
                        program,
                        &[b"pool_vault", pool_key.as_ref(), mint.as_ref()],
                    )]);
                }
            }
            // Markets are listed from the program below.
            Venue::Manifest => {}
        }
    }
    for market in markets {
        for token in scenario.tokens.values() {
            out.extend([pda(
                "manifest",
                programs.manifest,
                &[b"vault", market.as_ref(), token.mint.as_ref()],
            )]);
        }
    }
    Ok(out)
}

/// Every token account of `mint` under `token_program`, with its owner.
async fn token_accounts_of(
    rpc: &JsonRpc,
    token_program: &Pubkey,
    mint: &Pubkey,
) -> Result<Vec<(Pubkey, Pubkey)>> {
    let accounts = rpc
        .call(
            "getProgramAccounts",
            json!([token_program.to_string(), {
                "encoding": "base64",
                "commitment": rpc.commitment,
                "dataSlice": { "offset": 32, "length": 32 },
                "filters": [{ "memcmp": { "offset": 0, "bytes": mint.to_string() } }],
            }]),
        )
        .await
        .with_context(|| format!("listing token accounts of {mint}"))?;
    let mut out = Vec::new();
    for entry in accounts
        .as_array()
        .context("getProgramAccounts: no array")?
    {
        let key: Pubkey = entry["pubkey"]
            .as_str()
            .and_then(|key| key.parse().ok())
            .context("getProgramAccounts: bad pubkey")?;
        if key == *mint {
            continue; // the mint itself can match offset 0
        }
        let owner = entry["account"]["data"][0]
            .as_str()
            .and_then(|b64| {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD.decode(b64).ok()
            })
            .and_then(|raw| Pubkey::try_from(raw.as_slice()).ok())
            .context("getProgramAccounts: bad owner slice")?;
        out.push((key, owner));
    }
    Ok(out)
}

/// Burn and close every scenario-mint token account held by one of the
/// venues' PDAs. Returns the report and the lamports recovered.
async fn close_vaults(
    chain: &ChainClient,
    rpc: &JsonRpc,
    scenario: &Scenario,
    authority: &Keypair,
    config: SubmissionConfig,
) -> Result<(Value, u64)> {
    let markets: Vec<Pubkey> = owned(rpc, &scenario.programs.manifest)
        .await?
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    let authorities = vault_authorities(scenario, &markets)?;
    // (pda, token program) -> [(vault, mint)]
    let mut groups: BTreeMap<(Pubkey, Pubkey), Vec<(Pubkey, Pubkey)>> = BTreeMap::new();
    let mut others = 0_usize;
    for token in scenario.tokens.values() {
        for (account, owner) in token_accounts_of(rpc, &token.token_program, &token.mint).await? {
            if authorities.contains_key(&owner) {
                groups
                    .entry((owner, token.token_program))
                    .or_default()
                    .push((account, token.mint));
            } else {
                others += 1;
            }
        }
    }
    let mut by_venue: BTreeMap<&str, (usize, u64)> = BTreeMap::new();
    let mut total = 0_u64;
    for ((owner, token_program), vaults) in groups {
        let held = &authorities[&owner];
        let seeds: Vec<&[u8]> = held.seeds.iter().map(Vec::as_slice).collect();
        let keys: Vec<Pubkey> = vaults.iter().map(|(vault, _)| *vault).collect();
        let lamports: u64 = chain
            .rpc()
            .get_multiple_accounts(&keys)
            .await?
            .into_iter()
            .flatten()
            .map(|account| account.lamports)
            .sum();
        for chunk in vaults.chunks(MAX_TOKEN_ACCOUNTS_PER_TRANSACTION) {
            let ix = close_token_accounts(
                held.program,
                authority.pubkey(),
                token_program,
                owner,
                &seeds,
                chunk,
            )
            .context("vault-authority seeds exceed the PDA limits")?;
            chain
                .submit(vec![ix], authority, &[], config)
                .await
                .with_context(|| format!("closing {} vaults held by {owner}", held.venue))?;
        }
        let entry = by_venue.entry(held.venue).or_default();
        entry.0 += vaults.len();
        entry.1 += lamports;
        total += lamports;
    }
    let report = json!({
        "byVenue": by_venue
            .into_iter()
            .map(|(venue, (count, lamports))| (venue.to_owned(), json!({ "closed": count, "sol": lamports as f64 / LAMPORTS_PER_SOL })))
            .collect::<serde_json::Map<_, _>>(),
        "otherTokenAccountsLeft": others,
    });
    Ok((report, total))
}

/// Close everything the three DEX programs own, rent to `authority` (one
/// of the authorities compiled into `elf`): first the vaults their PDAs
/// hold, then their own accounts.
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
    let mut deployed = 0;
    for (_, program) in &venues {
        deployed = deployed.max(ensure_janitor(chain, program, elf).await?);
    }
    wait_past(chain, deployed).await?;
    let config = SubmissionConfig {
        compute_unit_limit: CLOSE_CU_LIMIT,
        ..SubmissionConfig::default()
    };
    // Vaults first: the Manifest market list comes from the program's own
    // accounts, which the second pass closes.
    let (vaults, mut total) = close_vaults(chain, rpc, scenario, authority, config).await?;
    let mut report = serde_json::Map::new();
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
        "vaults": vaults,
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

    #[test]
    fn the_deployment_slot_follows_the_tag() {
        let mut data = vec![3, 0, 0, 0];
        data.extend_from_slice(&1_234_567_u64.to_le_bytes());
        data.extend_from_slice(&[0; 33]);
        assert_eq!(deployment_slot(&data), Some(1_234_567));
        assert_eq!(deployment_slot(&data[..8]), None);
    }
}
