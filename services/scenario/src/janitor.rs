//! Close every account the scenario's DEX programs own, once the janitor
//! program (`programs/janitor`) has been deployed over them as an upgrade
//! (`scenario.sh teardown` with `SCENARIO_JANITOR=1` does both).
//!
//! Two passes per run. First the token accounts the programs' PDAs hold
//! (pool and market vaults): the vaults named by every pool on the program
//! IDs (and derived for every Manifest market) are read, and each one whose
//! owner is the venue's PDA is burned and closed with `CloseTokenAccounts`,
//! the janitor signing as that PDA. The layouts and seed recipes below are
//! the only protocol-specific part; the program re-derives and checks every
//! one. Then every account the programs own, with `Close`.
//!
//! The close instruction is a single byte, which the real DEX programs would
//! read as one of their own instructions, so nothing is sent to a program
//! unless its on-chain code is byte-for-byte the janitor binary given here.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use futures::StreamExt;
use janitor_interface::{
    MAX_TARGETS_PER_TRANSACTION, MAX_TOKEN_ACCOUNTS_PER_TRANSACTION, close, close_token_accounts,
};
use mtm_chain::{ChainClient, SubmissionConfig};
use mtm_dex_harness::spl;
use mtm_scenario::{Programs, Scenario};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
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

/// sha256 (hex) of a program's code with its trailing zero bytes dropped: a
/// ProgramData account pads the ELF with zeros up to its allocated length,
/// so the deployed code and the artifact it came from hash the same, and a
/// rebuilt artifact passes exactly when it is the same binary.
pub fn code_hash(code: &[u8]) -> String {
    let end = code
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |last| last + 1);
    Sha256::digest(&code[..end])
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// What a program ID runs, compared with the code it should run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Code {
    /// Exactly the expected code.
    Matches,
    /// The janitor binary (a janitor teardown did not restore it).
    Janitor,
    /// Other code: a different build, or something else entirely.
    Differs,
    /// No account at the program ID.
    Absent,
    /// An upgradeable program with no program data: closed for good.
    Closed,
    /// No expected code (no lock entry and no artifact file): nothing was
    /// compared.
    Unverified,
}

/// What a program ID holds: the hash of its code, unless absent or closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Deployed {
    Absent,
    Closed,
    Code(String),
}

/// Read one program's code. `program` and its program data are read with
/// the batched raw reader, so an absent account is a definite answer and an
/// RPC error is an error.
pub async fn deployed(rpc: &JsonRpc, program: &Pubkey) -> Result<Deployed> {
    let Some(account) = accounts(rpc, &[*program]).await?.pop().flatten() else {
        return Ok(Deployed::Absent);
    };
    // Upgradeable program account: u32 tag + ProgramData address.
    let program_data = account
        .data
        .get(4..36)
        .and_then(|raw| Pubkey::try_from(raw).ok())
        .with_context(|| format!("{program} is not an upgradeable program"))?;
    let Some(data) = accounts(rpc, &[program_data]).await?.pop().flatten() else {
        return Ok(Deployed::Closed);
    };
    Ok(Deployed::Code(code_hash(
        data.data.get(PROGRAM_DATA_HEADER..).unwrap_or_default(),
    )))
}

/// `on_chain` against the expected code hash and the janitor's.
fn classify(on_chain: &str, expected: Option<&str>, janitor: Option<&str>) -> Code {
    if janitor == Some(on_chain) {
        return Code::Janitor;
    }
    match expected {
        None => Code::Unverified,
        Some(expected) if expected == on_chain => Code::Matches,
        Some(_) => Code::Differs,
    }
}

/// The lock file `deploy` records: one `<program id> <code sha256>` line
/// per program (`#` comments). A missing file has no entries.
pub fn read_lock(path: &Path) -> Result<BTreeMap<Pubkey, String>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let (program, hash) = line
                .split_once(char::is_whitespace)
                .with_context(|| format!("{}: bad line {line:?}", path.display()))?;
            let hash = hash.trim();
            anyhow::ensure!(
                hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "{}: bad sha256 in {line:?}",
                path.display()
            );
            Ok((
                program
                    .parse()
                    .with_context(|| format!("{}: bad program id in {line:?}", path.display()))?,
                hash.to_ascii_lowercase(),
            ))
        })
        .collect()
}

fn write_lock(path: &Path, entries: &BTreeMap<Pubkey, String>) -> Result<()> {
    let mut text = String::from(
        "# Code sha256 (trailing zeros dropped) each DEX program ID runs, recorded by\n\
         # `scenario.sh deploy`; `up`, `deploy` and the janitor teardown check against it.\n",
    );
    for (program, hash) in entries {
        text.push_str(&format!("{program} {hash}\n"));
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

/// How strict [`check_programs`] is.
#[derive(Debug, Clone, Default)]
pub struct CheckOptions {
    /// A program that is not deployed passes (`deploy` creates it).
    pub allow_absent: bool,
    /// No expected code (no lock entry, no artifact) fails instead of
    /// warning: a remote target, where a program left as anything else must
    /// not pass unnoticed.
    pub require_verified: bool,
    /// Each artifact must exist and be exactly the deployed code: the
    /// janitor's restore deploys it, so it must be the code to restore.
    pub require_artifact: bool,
    /// The lock file (`<program id> <sha256>` lines); its entry, when there
    /// is one, is the expected code, ahead of the artifact.
    pub lock: Option<PathBuf>,
    /// On success, record each matching program's hash in `lock`. A
    /// program running exactly its artifact matches (and is recorded)
    /// whatever the lock held: `deploy` records what it just deployed.
    pub record: bool,
}

/// `deploy` / `up` / janitor preflight: every program must run its expected
/// code, the lock-file hash when there is one, else its artifact's. Fails
/// listing the programs that run the janitor or other code (with both
/// hashes), are closed, or (unless `allow_absent`) are not deployed; with
/// nothing to compare against, it warns (fails with `require_verified`).
pub async fn check_programs(
    rpc: &JsonRpc,
    programs: &[(Pubkey, PathBuf)],
    janitor_so: Option<&Path>,
    options: &CheckOptions,
) -> Result<Value> {
    let janitor = janitor_so
        .map(|path| {
            std::fs::read(path)
                .map(|elf| code_hash(&elf))
                .with_context(|| format!("reading {}", path.display()))
        })
        .transpose()?;
    let mut lock = options
        .lock
        .as_deref()
        .map(read_lock)
        .transpose()?
        .unwrap_or_default();
    let lock_name = options.lock.as_deref().map_or_else(
        || "no lock file".to_owned(),
        |path| path.display().to_string(),
    );
    let mut rows = Vec::new();
    let mut bad = Vec::new();
    let mut matched = Vec::new();
    for (program, path) in programs {
        let artifact = match std::fs::read(path) {
            Ok(elf) => Some(code_hash(&elf)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", path.display()));
            }
        };
        let locked = lock.get(program).cloned();
        let on_chain = deployed(rpc, program).await?;
        // Recording after a deploy: the artifact just deployed is the code
        // to record, whatever the lock held before.
        let deployed_artifact = options.record
            && matches!((&on_chain, &artifact), (Deployed::Code(code), Some(hash)) if code == hash);
        let (expected, source) = match (&locked, &artifact) {
            (_, Some(hash)) if deployed_artifact => {
                (Some(hash.as_str()), path.display().to_string())
            }
            (Some(hash), _) => (Some(hash.as_str()), lock_name.clone()),
            (None, Some(hash)) => (Some(hash.as_str()), path.display().to_string()),
            (None, None) => (None, String::new()),
        };
        let (code, on_chain) = match on_chain {
            Deployed::Absent => (Code::Absent, None),
            Deployed::Closed => (Code::Closed, None),
            Deployed::Code(hash) => (classify(&hash, expected, janitor.as_deref()), Some(hash)),
        };
        let on_chain_text = on_chain.as_deref().unwrap_or("-");
        match code {
            Code::Matches => {
                if options.require_artifact && artifact != on_chain {
                    bad.push(match &artifact {
                        None => format!(
                            "{program}: artifact {} is missing, so the forks could not be restored",
                            path.display()
                        ),
                        Some(hash) => format!(
                            "{program}: artifact {} (sha256 {hash}) is not the deployed code \
                             (sha256 {on_chain_text}), so a restore would deploy another build",
                            path.display()
                        ),
                    });
                } else {
                    if let Some(hash) = &artifact
                        && Some(hash) != on_chain.as_ref()
                    {
                        tracing::warn!(%program, artifact = %path.display(), artifact_sha256 = %hash, deployed_sha256 = %on_chain_text,
                            "the local artifact is another build than the deployed (locked) code");
                    }
                    matched.push((*program, on_chain.clone().unwrap_or_default()));
                }
            }
            Code::Absent if options.allow_absent => {}
            Code::Unverified if options.require_verified || options.require_artifact => {
                bad.push(format!(
                    "{program}: nothing to compare its code (sha256 {on_chain_text}) with: artifact \
                     {} is missing and {lock_name} has no entry for it",
                    path.display()
                ));
            }
            Code::Unverified => {
                tracing::warn!(%program, artifact = %path.display(), deployed_sha256 = %on_chain_text,
                    "no artifact and no lock entry: the program's code was not checked")
            }
            Code::Janitor => bad.push(format!("{program} runs the janitor")),
            Code::Differs => bad.push(format!(
                "{program} runs code sha256 {on_chain_text}, expected sha256 {} (from {source})",
                expected.unwrap_or_default()
            )),
            Code::Absent => bad.push(format!("{program} is not deployed")),
            Code::Closed => bad.push(format!("{program} was closed")),
        }
        rows.push(json!({
            "program": program.to_string(),
            "artifact": path,
            "code": code,
            "sha256": on_chain,
            "artifactSha256": artifact,
            "lockedSha256": locked,
        }));
    }
    anyhow::ensure!(bad.is_empty(), "{}", bad.join("; "));
    if options.record
        && let Some(path) = &options.lock
    {
        lock.extend(matched);
        write_lock(path, &lock)?;
    }
    Ok(json!({ "programs": rows }))
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
pub struct VaultAuthority {
    pub venue: &'static str,
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

/// Orca `Whirlpool` (Borsh): config 8..40, tick-spacing seed 43..45, mint A
/// 101..133, vault A 133..165, mint B 181..213, vault B 213..245.
const WHIRLPOOL: [u8; 8] = [63, 149, 209, 12, 225, 128, 99, 9];
/// Raydium `PoolState` (packed): amm config 9..41, mint 0 73..105, mint 1
/// 105..137, vault 0 137..169, vault 1 169..201, seed index 391..393.
const POOL_STATE: [u8; 8] = [247, 237, 227, 245, 215, 195, 222, 70];

fn key_at(data: &[u8], offset: usize) -> Option<Pubkey> {
    Pubkey::try_from(data.get(offset..offset + 32)?).ok()
}

/// A pool's two (vault, mint) pairs and the PDA that holds them, from its
/// account data. `None` unless the pool's own seeds re-derive its address
/// (the janitor signs with those seeds).
type PoolVaults = ([(Pubkey, Pubkey); 2], Vec<(Pubkey, VaultAuthority)>);

/// Orca: the whirlpool PDA, `["whirlpool", config, mint_a, mint_b, tick
/// spacing seed]`, holds both vaults (keypair accounts named in the pool).
fn orca_pool(program: Pubkey, pool: Pubkey, data: &[u8]) -> Option<PoolVaults> {
    if data.get(..8)? != WHIRLPOOL {
        return None;
    }
    let (config, mint_a, mint_b) = (key_at(data, 8)?, key_at(data, 101)?, key_at(data, 181)?);
    let (address, authority) = pda(
        "orca-whirlpool",
        program,
        &[
            b"whirlpool",
            config.as_ref(),
            mint_a.as_ref(),
            mint_b.as_ref(),
            data.get(43..45)?,
        ],
    );
    if address != pool {
        return None;
    }
    Some((
        [(key_at(data, 133)?, mint_a), (key_at(data, 213)?, mint_b)],
        vec![(address, authority)],
    ))
}

/// Raydium CLMM: the pool PDA, `["pool", amm_config, mint_0, mint_1,
/// (seed index, when set)]`, holds its vaults; each vault is itself the PDA
/// `["pool_vault", pool, mint]`, accepted as a holder too (the on-chain
/// owner decides).
fn raydium_pool(program: Pubkey, pool: Pubkey, data: &[u8]) -> Option<PoolVaults> {
    if data.get(..8)? != POOL_STATE {
        return None;
    }
    let (config, mint_0, mint_1) = (key_at(data, 9)?, key_at(data, 73)?, key_at(data, 105)?);
    let index = data.get(391..393)?;
    let mut seeds: Vec<&[u8]> = vec![b"pool", config.as_ref(), mint_0.as_ref(), mint_1.as_ref()];
    if index != [0, 0] {
        seeds.push(index);
    }
    let (address, authority) = pda("raydium-clmm", program, &seeds);
    if address != pool {
        return None;
    }
    let vaults = [(key_at(data, 137)?, mint_0), (key_at(data, 169)?, mint_1)];
    let mut holders = vec![(address, authority)];
    for (vault, mint) in vaults {
        let held = pda(
            "raydium-clmm",
            program,
            &[b"pool_vault", pool.as_ref(), mint.as_ref()],
        );
        if held.0 == vault {
            holders.push(held);
        }
    }
    Some((vaults, holders))
}

/// Manifest: markets are keypair accounts, and each vault is the PDA
/// `["vault", market, mint]` holding itself, derived for every scenario
/// mint (the ones a market does not trade do not exist).
fn manifest_vaults(
    program: Pubkey,
    market: Pubkey,
    mints: &[Pubkey],
) -> Vec<(Pubkey, Pubkey, (Pubkey, VaultAuthority))> {
    mints
        .iter()
        .map(|mint| {
            let held = pda(
                "manifest",
                program,
                &[b"vault", market.as_ref(), mint.as_ref()],
            );
            (held.0, *mint, held)
        })
        .collect()
}

/// An account as `getMultipleAccounts` returns it.
pub struct Raw {
    pub owner: Pubkey,
    pub lamports: u64,
    pub data: Vec<u8>,
}

/// `keys` in batches of 100 (a few in flight), at the client's commitment;
/// `None` for an absent account.
pub async fn accounts(rpc: &JsonRpc, keys: &[Pubkey]) -> Result<Vec<Option<Raw>>> {
    let batches: Vec<Vec<Option<Raw>>> =
        futures::stream::iter(keys.chunks(100).map(|chunk| async move {
            let keys: Vec<String> = chunk.iter().map(Pubkey::to_string).collect();
            let response = rpc
                .call(
                    "getMultipleAccounts",
                    json!([keys, { "encoding": "base64", "commitment": rpc.commitment }]),
                )
                .await
                .context("getMultipleAccounts")?;
            response["value"]
                .as_array()
                .filter(|values| values.len() == chunk.len())
                .context("getMultipleAccounts: wrong value array")?
                .iter()
                .map(raw)
                .collect::<Result<Vec<_>>>()
        }))
        .buffered(8)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<_>>()?;
    Ok(batches.into_iter().flatten().collect())
}

fn raw(value: &Value) -> Result<Option<Raw>> {
    if value.is_null() {
        return Ok(None);
    }
    let owner = value["owner"]
        .as_str()
        .and_then(|owner| owner.parse().ok())
        .context("getMultipleAccounts: bad owner")?;
    let lamports = value["lamports"]
        .as_u64()
        .context("getMultipleAccounts: bad lamports")?;
    let data = value["data"][0]
        .as_str()
        .and_then(|b64| {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.decode(b64).ok()
        })
        .context("getMultipleAccounts: bad data")?;
    Ok(Some(Raw {
        owner,
        lamports,
        data,
    }))
}

/// The accounts `program` owns whose data starts with `discriminator`.
async fn tagged(rpc: &JsonRpc, program: &Pubkey, discriminator: [u8; 8]) -> Result<Vec<Pubkey>> {
    let accounts = rpc
        .call(
            "getProgramAccounts",
            json!([program.to_string(), {
                "encoding": "base64",
                "commitment": rpc.commitment,
                "dataSlice": { "offset": 0, "length": 8 },
            }]),
        )
        .await
        .with_context(|| format!("listing {program}'s accounts"))?;
    let mut out = Vec::new();
    for entry in accounts
        .as_array()
        .context("getProgramAccounts: no array")?
    {
        let head = entry["account"]["data"][0].as_str().and_then(|b64| {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.decode(b64).ok()
        });
        if head.as_deref() == Some(&discriminator[..]) {
            out.push(
                entry["pubkey"]
                    .as_str()
                    .and_then(|key| key.parse().ok())
                    .context("getProgramAccounts: bad pubkey")?,
            );
        }
    }
    Ok(out)
}

/// A pool or market vault, as read on chain.
pub struct Vault {
    pub address: Pubkey,
    pub mint: Pubkey,
    pub token_program: Pubkey,
    /// The token account's owner.
    pub owner: Pubkey,
    pub lamports: u64,
}

/// The vaults of every pool and market on the three program IDs, with the
/// PDAs that hold them. Nothing scans a token program: the Whirlpool and
/// PoolState accounts name their vaults, and Manifest's are derived per
/// market and scenario mint; only token accounts of the expected mint come
/// back. A vault whose owner is not in the map was not created by the venue
/// (the janitor cannot close it).
pub async fn vaults(
    rpc: &JsonRpc,
    programs: &Programs,
    mints: &[Pubkey],
) -> Result<(Vec<Vault>, HashMap<Pubkey, VaultAuthority>)> {
    let mut authorities = HashMap::new();
    // (vault, expected mint)
    let mut candidates: Vec<(Pubkey, Pubkey)> = Vec::new();
    for (program, discriminator, decode) in [
        (
            programs.orca_whirlpool,
            WHIRLPOOL,
            orca_pool as fn(Pubkey, Pubkey, &[u8]) -> Option<PoolVaults>,
        ),
        (programs.raydium_clmm, POOL_STATE, raydium_pool),
    ] {
        let pools = tagged(rpc, &program, discriminator).await?;
        for (pool, account) in pools.iter().zip(accounts(rpc, &pools).await?) {
            let Some((vaults, holders)) = account.and_then(|a| decode(program, *pool, &a.data))
            else {
                tracing::warn!(%pool, %program, "pool does not re-derive from its own seeds: its vaults are left");
                continue;
            };
            candidates.extend(vaults);
            authorities.extend(holders);
        }
    }
    for (market, _) in owned(rpc, &programs.manifest).await? {
        for (vault, mint, held) in manifest_vaults(programs.manifest, market, mints) {
            candidates.push((vault, mint));
            authorities.extend([held]);
        }
    }
    candidates.sort();
    candidates.dedup();
    let keys: Vec<Pubkey> = candidates.iter().map(|(vault, _)| *vault).collect();
    let mut out = Vec::new();
    for ((address, mint), account) in candidates.into_iter().zip(accounts(rpc, &keys).await?) {
        let Some(account) = account else { continue };
        let Some(token) = (spl::is_token_program(&account.owner) && account.data.len() >= 165)
            .then(|| spl::decode_token_account(&account.data))
            .flatten()
            .filter(|token| token.mint == mint)
        else {
            continue;
        };
        out.push(Vault {
            address,
            mint,
            token_program: account.owner,
            owner: token.owner,
            lamports: account.lamports,
        });
    }
    Ok((out, authorities))
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
    let mints: Vec<Pubkey> = scenario.tokens.values().map(|token| token.mint).collect();
    let (found, authorities) = vaults(rpc, &scenario.programs, &mints).await?;
    // (pda, token program) -> [(vault, mint)]
    let mut groups: BTreeMap<(Pubkey, Pubkey), Vec<(Pubkey, Pubkey)>> = BTreeMap::new();
    let mut lamports_of: HashMap<Pubkey, u64> = HashMap::new();
    let mut foreign = 0_usize;
    for vault in found {
        if authorities.contains_key(&vault.owner) {
            lamports_of.insert(vault.address, vault.lamports);
            groups
                .entry((vault.owner, vault.token_program))
                .or_default()
                .push((vault.address, vault.mint));
        } else {
            foreign += 1;
        }
    }
    let mut by_venue: BTreeMap<&str, (usize, u64)> = BTreeMap::new();
    let mut total = 0_u64;
    for ((owner, token_program), vaults) in groups {
        let held = &authorities[&owner];
        let seeds: Vec<&[u8]> = held.seeds.iter().map(Vec::as_slice).collect();
        let lamports: u64 = vaults.iter().map(|(vault, _)| lamports_of[vault]).sum();
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
        "vaultsNotHeldByTheVenue": foreign,
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
    fn program_code_is_the_fork_the_janitor_or_other() {
        let (fork, janitor) = (code_hash(b"\x7fELF fork"), code_hash(b"\x7fELF janitor"));
        // Zero padding up to the account's length hashes away.
        assert_eq!(code_hash(b"\x7fELF fork\0\0\0\0"), fork);
        assert_ne!(code_hash(b"\x7fELF for"), fork);
        assert_eq!(classify(&fork, Some(&fork), Some(&janitor)), Code::Matches);
        assert_eq!(
            classify(&janitor, Some(&fork), Some(&janitor)),
            Code::Janitor
        );
        // Without the expected code the janitor is still recognised.
        assert_eq!(classify(&janitor, None, Some(&janitor)), Code::Janitor);
        assert_eq!(classify(&fork, None, Some(&janitor)), Code::Unverified);
        assert_eq!(
            classify(&code_hash(b"\x7fELF v2"), Some(&fork), None),
            Code::Differs
        );
    }

    /// A scratch directory, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "mtm-janitor-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).expect("mkdir");
            Self(dir)
        }

        fn file(&self, name: &str, data: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, data).expect("write");
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn check_programs_compares_code_hashes() {
        let elf = |name: &str| format!("\x7fELF {name}").into_bytes();
        // program → its program data account, holding `code` (padded).
        let mut accounts: HashMap<Pubkey, (Pubkey, u64, Vec<u8>)> = HashMap::new();
        let loader = Pubkey::new_unique();
        let mut deploy = |code: Option<&[u8]>| {
            let (program, program_data) = (Pubkey::new_unique(), Pubkey::new_unique());
            let mut data = vec![2, 0, 0, 0];
            data.extend_from_slice(program_data.as_ref());
            accounts.insert(program, (loader, 1, data));
            if let Some(code) = code {
                let mut data = vec![3, 0, 0, 0];
                data.extend_from_slice(&[0; 41]);
                data.extend_from_slice(code);
                data.extend_from_slice(&[0; 32]);
                accounts.insert(program_data, (loader, 1, data));
            }
            program
        };
        let matches = deploy(Some(&elf("fork")));
        let janitor = deploy(Some(&elf("janitor")));
        let closed = deploy(None);
        let rebuilt = deploy(Some(&elf("release build")));
        let unverified = deploy(Some(&elf("unknown")));
        let differs = deploy(Some(&elf("v1")));
        let absent = Pubkey::new_unique();
        let (url, _) = stub_rpc(move |method, params| {
            assert_eq!(method, "getMultipleAccounts");
            let value: Vec<Value> = params[0]
                .as_array()
                .expect("keys")
                .iter()
                .map(|key| {
                    let key: Pubkey = key.as_str().and_then(|k| k.parse().ok()).expect("key");
                    accounts.get(&key).map_or(Value::Null, |(owner, lamports, data)| {
                        json!({ "owner": owner.to_string(), "lamports": lamports, "data": [b64(data), "base64"] })
                    })
                })
                .collect();
            json!({ "jsonrpc": "2.0", "id": 1, "result": { "context": { "slot": 1 }, "value": value } })
        })
        .await;
        let rpc = JsonRpc::new(url, "confirmed").expect("client");
        let dir = Scratch::new("check");
        let janitor_so = dir.file("janitor.so", &elf("janitor"));
        let lock = dir.0.join("programs.lock");
        std::fs::write(
            &lock,
            format!(
                "# recorded\n{rebuilt} {}\n",
                code_hash(&elf("release build"))
            ),
        )
        .expect("lock");
        let fork = dir.file("fork.so", &elf("fork"));
        // Another build of the locked program: the lock decides.
        let other_build = dir.file("rebuilt.so", &elf("local build"));
        let missing = dir.0.join("missing.so");
        let v2 = dir.file("v2.so", &elf("v2"));
        let check = |programs: Vec<(Pubkey, PathBuf)>, options: CheckOptions| {
            let (rpc, janitor_so) = (&rpc, janitor_so.clone());
            async move {
                check_programs(rpc, &programs, Some(&janitor_so), &options)
                    .await
                    .map_err(|error| format!("{error:#}"))
            }
        };
        let locked = || CheckOptions {
            lock: Some(lock.clone()),
            ..CheckOptions::default()
        };

        let healthy = vec![
            (matches, fork.clone()),
            (rebuilt, other_build.clone()),
            (unverified, missing.clone()),
        ];
        let report = check(healthy.clone(), locked()).await.expect("healthy");
        assert_eq!(report["programs"][0]["code"], "matches");
        assert_eq!(report["programs"][1]["code"], "matches");
        assert_eq!(report["programs"][2]["code"], "unverified");
        // A remote target refuses what it cannot verify.
        let error = check(
            healthy.clone(),
            CheckOptions {
                require_verified: true,
                ..locked()
            },
        )
        .await
        .expect_err("unverified");
        assert!(
            error.contains(&format!("{unverified}: nothing to compare")),
            "{error}"
        );
        assert!(!error.contains(&rebuilt.to_string()), "{error}");
        // The janitor preflight needs every artifact to be the deployed code.
        let error = check(
            healthy,
            CheckOptions {
                require_artifact: true,
                ..locked()
            },
        )
        .await
        .expect_err("artifacts");
        assert!(
            error.contains(&format!("{rebuilt}: artifact"))
                && error.contains("is not the deployed code"),
            "{error}"
        );
        assert!(error.contains(&unverified.to_string()), "{error}");
        assert!(!error.contains(&matches.to_string()), "{error}");
        let error = check(
            vec![(matches, missing.clone())],
            CheckOptions {
                require_artifact: true,
                ..locked()
            },
        )
        .await
        .expect_err("missing artifact");
        assert!(error.contains("is missing"), "{error}");

        for (program, artifact, want) in [
            (janitor, fork.clone(), "runs the janitor".to_owned()),
            (closed, fork.clone(), "was closed".to_owned()),
            (absent, fork.clone(), "is not deployed".to_owned()),
            (
                differs,
                v2.clone(),
                format!(
                    "runs code sha256 {}, expected sha256 {}",
                    code_hash(&elf("v1")),
                    code_hash(&elf("v2"))
                ),
            ),
        ] {
            let error = check(vec![(program, artifact)], locked())
                .await
                .expect_err(&want);
            assert!(error.contains(&want), "{error}");
        }
        check(
            vec![(absent, fork.clone())],
            CheckOptions {
                allow_absent: true,
                ..locked()
            },
        )
        .await
        .expect("absent allowed");

        // `deploy` records what it verified; existing entries stay.
        check(
            vec![(matches, fork.clone())],
            CheckOptions {
                record: true,
                ..locked()
            },
        )
        .await
        .expect("recorded");
        let recorded = read_lock(&lock).expect("lock");
        assert_eq!(recorded.len(), 2);
        assert_eq!(recorded[&matches], code_hash(&elf("fork")));
        assert_eq!(recorded[&rebuilt], code_hash(&elf("release build")));
        // After an upgrade to the artifact, recording replaces a stale entry
        // (a plain check refuses it).
        std::fs::write(&lock, format!("{differs} {}\n", code_hash(&elf("v0")))).expect("lock");
        let v1 = dir.file("v1.so", &elf("v1"));
        check(vec![(differs, v1.clone())], locked())
            .await
            .expect_err("stale lock");
        check(
            vec![(differs, v1)],
            CheckOptions {
                record: true,
                ..locked()
            },
        )
        .await
        .expect("recorded over the stale entry");
        assert_eq!(
            read_lock(&lock).expect("lock")[&differs],
            code_hash(&elf("v1"))
        );
        std::fs::write(
            &lock,
            format!(
                "{matches} {}\n{rebuilt} {}\n",
                code_hash(&elf("fork")),
                code_hash(&elf("release build"))
            ),
        )
        .expect("lock");
        // A program running the janitor fails the check and records nothing.
        check(
            vec![(janitor, fork)],
            CheckOptions {
                record: true,
                ..locked()
            },
        )
        .await
        .expect_err("janitor");
        assert_eq!(read_lock(&lock).expect("lock").len(), 2);
        assert!(
            read_lock(&dir.0.join("none.lock"))
                .expect("missing")
                .is_empty()
        );
        assert!(read_lock(&dir.file("bad.lock", b"abc 12\n")).is_err());
    }

    #[test]
    fn the_deployment_slot_follows_the_tag() {
        let mut data = vec![3, 0, 0, 0];
        data.extend_from_slice(&1_234_567_u64.to_le_bytes());
        data.extend_from_slice(&[0; 33]);
        assert_eq!(deployment_slot(&data), Some(1_234_567));
        assert_eq!(deployment_slot(&data[..8]), None);
    }

    /// A JSON-RPC server on localhost answering with `handle`; every
    /// request's (method, first param) is recorded.
    async fn stub_rpc(
        handle: impl Fn(&str, &Value) -> Value + Send + Sync + 'static,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (handle, seen) = (std::sync::Arc::new(handle), calls.clone());
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (handle, seen) = (handle.clone(), seen.clone());
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0_u8; 4096];
                    let body = loop {
                        let read = socket.read(&mut chunk).await.expect("read");
                        assert!(read > 0, "request ended early");
                        buffer.extend_from_slice(&chunk[..read]);
                        let text = String::from_utf8_lossy(&buffer).to_string();
                        let Some(split) = text.find("\r\n\r\n") else {
                            continue;
                        };
                        let length: usize = text[..split]
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse().ok())?
                            })
                            .unwrap_or(0);
                        if buffer.len() >= split + 4 + length {
                            break buffer[split + 4..split + 4 + length].to_vec();
                        }
                    };
                    let request: Value = serde_json::from_slice(&body).expect("json");
                    let method = request["method"].as_str().unwrap_or_default().to_owned();
                    let first = request["params"][0].as_str().unwrap_or_default().to_owned();
                    seen.lock().expect("lock").push((method.clone(), first));
                    let reply = handle(&method, &request["params"]);
                    let reply = serde_json::to_string(&reply).expect("reply");
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                        reply.len()
                    );
                    socket.write_all(response.as_bytes()).await.expect("write");
                });
            }
        });
        (url, calls)
    }

    fn b64(data: &[u8]) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(data)
    }

    fn put(data: &mut [u8], offset: usize, key: &Pubkey) {
        data[offset..offset + 32].copy_from_slice(key.as_ref());
    }

    fn token_account(mint: &Pubkey, owner: &Pubkey) -> Vec<u8> {
        let mut data = vec![0; 165];
        put(&mut data, 0, mint);
        put(&mut data, 32, owner);
        data
    }

    struct Fixture {
        programs: Programs,
        mints: [Pubkey; 2],
        /// address → (owner program, lamports, data)
        accounts: HashMap<Pubkey, (Pubkey, u64, Vec<u8>)>,
        whirlpool: Pubkey,
        raydium_pool: Pubkey,
        market: Pubkey,
    }

    /// One pool per venue on fresh program IDs, each with its two vaults;
    /// Orca's vault B is held by someone else.
    fn fixture() -> Fixture {
        let programs = Programs {
            raydium_clmm: Pubkey::new_unique(),
            raydium_amm_config: Pubkey::new_unique(),
            orca_whirlpool: Pubkey::new_unique(),
            orca_whirlpools_config: Pubkey::new_unique(),
            orca_tick_spacing: 64,
            manifest: Pubkey::new_unique(),
        };
        let (a, b) = (Pubkey::new_unique(), Pubkey::new_unique());
        let (a, b) = if a < b { (a, b) } else { (b, a) };
        let token = spl::TOKEN_PROGRAM_ID;
        let mut accounts = HashMap::new();

        let config = programs.orca_whirlpools_config;
        let (whirlpool, _) = Pubkey::find_program_address(
            &[
                b"whirlpool",
                config.as_ref(),
                a.as_ref(),
                b.as_ref(),
                &64_u16.to_le_bytes(),
            ],
            &programs.orca_whirlpool,
        );
        let (vault_a, vault_b) = (Pubkey::new_unique(), Pubkey::new_unique());
        let mut pool = vec![0; 653];
        pool[..8].copy_from_slice(&WHIRLPOOL);
        put(&mut pool, 8, &config);
        pool[43..45].copy_from_slice(&64_u16.to_le_bytes());
        put(&mut pool, 101, &a);
        put(&mut pool, 133, &vault_a);
        put(&mut pool, 181, &b);
        put(&mut pool, 213, &vault_b);
        accounts.insert(whirlpool, (programs.orca_whirlpool, 7, pool));
        accounts.insert(vault_a, (token, 2, token_account(&a, &whirlpool)));
        let stranger = Pubkey::new_unique();
        accounts.insert(vault_b, (token, 3, token_account(&b, &stranger)));
        // Another Orca account (a tick array, say): not a pool.
        accounts.insert(
            Pubkey::new_unique(),
            (programs.orca_whirlpool, 1, vec![9; 16]),
        );

        let amm = programs.raydium_amm_config;
        let (raydium_pool, _) = Pubkey::find_program_address(
            &[b"pool", amm.as_ref(), a.as_ref(), b.as_ref()],
            &programs.raydium_clmm,
        );
        let vault = |mint: &Pubkey| {
            Pubkey::find_program_address(
                &[b"pool_vault", raydium_pool.as_ref(), mint.as_ref()],
                &programs.raydium_clmm,
            )
            .0
        };
        let mut pool = vec![0; 1544];
        pool[..8].copy_from_slice(&POOL_STATE);
        put(&mut pool, 9, &amm);
        put(&mut pool, 73, &a);
        put(&mut pool, 105, &b);
        put(&mut pool, 137, &vault(&a));
        put(&mut pool, 169, &vault(&b));
        accounts.insert(raydium_pool, (programs.raydium_clmm, 11, pool));
        accounts.insert(vault(&a), (token, 4, token_account(&a, &raydium_pool)));
        accounts.insert(vault(&b), (token, 5, token_account(&b, &raydium_pool)));

        let market = Pubkey::new_unique();
        accounts.insert(market, (programs.manifest, 13, vec![1; 64]));
        for mint in [a, b] {
            let (vault, _) = Pubkey::find_program_address(
                &[b"vault", market.as_ref(), mint.as_ref()],
                &programs.manifest,
            );
            accounts.insert(vault, (token, 6, token_account(&mint, &vault)));
        }
        Fixture {
            programs,
            mints: [a, b],
            accounts,
            whirlpool,
            raydium_pool,
            market,
        }
    }

    #[test]
    fn pools_name_their_vaults_and_re_derive_their_holder() {
        let fixture = fixture();
        let (_, _, data) = &fixture.accounts[&fixture.whirlpool];
        let (vaults, holders) =
            orca_pool(fixture.programs.orca_whirlpool, fixture.whirlpool, data).expect("whirlpool");
        assert_eq!(vaults[0].1, fixture.mints[0]);
        assert_eq!(vaults[1].1, fixture.mints[1]);
        assert_eq!(holders.len(), 1);
        assert_eq!(holders[0].0, fixture.whirlpool);
        // An address its seeds do not derive: not ours to sign for.
        assert!(orca_pool(fixture.programs.orca_whirlpool, Pubkey::new_unique(), data).is_none());

        let (_, _, data) = &fixture.accounts[&fixture.raydium_pool];
        let (_, holders) = raydium_pool(fixture.programs.raydium_clmm, fixture.raydium_pool, data)
            .expect("pool state");
        // The pool, and each vault as its own PDA.
        assert_eq!(holders.len(), 3);
        assert_eq!(holders[0].0, fixture.raydium_pool);
        // A set seed index is part of the pool's seeds.
        let mut indexed = data.clone();
        indexed[391..393].copy_from_slice(&[1, 0]);
        assert!(
            raydium_pool(
                fixture.programs.raydium_clmm,
                fixture.raydium_pool,
                &indexed
            )
            .is_none()
        );
        let (a, b) = (fixture.mints[0], fixture.mints[1]);
        let amm = fixture.programs.raydium_amm_config;
        let (other, _) = Pubkey::find_program_address(
            &[b"pool", amm.as_ref(), a.as_ref(), b.as_ref(), &[1, 0]],
            &fixture.programs.raydium_clmm,
        );
        assert!(raydium_pool(fixture.programs.raydium_clmm, other, &indexed).is_some());
        // Wrong discriminator.
        assert!(orca_pool(fixture.programs.orca_whirlpool, fixture.raydium_pool, data).is_none());
    }

    /// A stub RPC serving `accounts` (`getProgramAccounts` refuses a
    /// token-program scan).
    async fn accounts_rpc(
        accounts: HashMap<Pubkey, (Pubkey, u64, Vec<u8>)>,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
    ) {
        stub_rpc(move |method, params| match method {
            "getProgramAccounts" => {
                let program: Pubkey = params[0].as_str().and_then(|p| p.parse().ok()).expect("program");
                if spl::is_token_program(&program) {
                    return json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32010, "message": "scan refused" } });
                }
                let length = params[1]["dataSlice"]["length"].as_u64().expect("slice") as usize;
                let listed: Vec<Value> = accounts
                    .iter()
                    .filter(|(_, (owner, _, _))| *owner == program)
                    .map(|(key, (_, lamports, data))| json!({
                        "pubkey": key.to_string(),
                        "account": { "lamports": lamports, "data": [b64(&data[..length.min(data.len())]), "base64"] },
                    }))
                    .collect();
                json!({ "jsonrpc": "2.0", "id": 1, "result": listed })
            }
            "getMultipleAccounts" => {
                let value: Vec<Value> = params[0]
                    .as_array()
                    .expect("keys")
                    .iter()
                    .map(|key| {
                        let key: Pubkey = key.as_str().and_then(|k| k.parse().ok()).expect("key");
                        accounts.get(&key).map_or(Value::Null, |(owner, lamports, data)| {
                            json!({ "owner": owner.to_string(), "lamports": lamports, "data": [b64(data), "base64"] })
                        })
                    })
                    .collect();
                json!({ "jsonrpc": "2.0", "id": 1, "result": { "context": { "slot": 1 }, "value": value } })
            }
            other => panic!("unexpected {other}"),
        })
        .await
    }

    #[tokio::test]
    async fn vaults_come_from_the_pools_without_scanning_a_token_program() {
        let fixture = fixture();
        let (url, calls) = accounts_rpc(fixture.accounts.clone()).await;
        let rpc = JsonRpc::new(url, "confirmed").expect("client");
        let (found, authorities) = vaults(&rpc, &fixture.programs, &fixture.mints)
            .await
            .expect("vaults");
        // 2 Orca + 2 Raydium + 2 Manifest; the stranger-held Orca vault is
        // found but its owner is no venue PDA.
        assert_eq!(found.len(), 6);
        let held: Vec<&Vault> = found
            .iter()
            .filter(|vault| authorities.contains_key(&vault.owner))
            .collect();
        assert_eq!(held.len(), 5);
        assert_eq!(
            held.iter().map(|v| v.lamports).sum::<u64>(),
            2 + 4 + 5 + 6 + 6
        );
        assert!(authorities.contains_key(&fixture.whirlpool));
        assert!(authorities.contains_key(&fixture.raydium_pool));
        let (manifest_vault, _) = Pubkey::find_program_address(
            &[b"vault", fixture.market.as_ref(), fixture.mints[0].as_ref()],
            &fixture.programs.manifest,
        );
        assert!(authorities.contains_key(&manifest_vault));
        let calls = calls.lock().expect("lock");
        assert!(
            calls
                .iter()
                .all(|(method, first)| method != "getProgramAccounts"
                    || ![spl::TOKEN_PROGRAM_ID, spl::TOKEN_2022_PROGRAM_ID]
                        .iter()
                        .any(|program| program.to_string() == *first)),
            "{calls:?}"
        );
    }
}
