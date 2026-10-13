//! Graceful teardown: unwind every participant (LP positions, Manifest
//! orders and seat balances), burn its scenario tokens, close its token
//! accounts, and sweep every lamport back to the faucet; then account for
//! the rent still locked in accounts nothing can close (pools, tick arrays,
//! markets, vaults, mints, programs).
//!
//! Idempotent: wallets that are already empty are skipped, so it can be
//! re-run after a partial failure.

use std::collections::{BTreeMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use base64::Engine;
use futures::StreamExt;
use mtm_chain::{ChainClient, SubmissionConfig};
use mtm_dex_harness::spl;
use mtm_scenario::{Scenario, Strategy, Venue};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use solana_commitment_config::CommitmentConfig;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use tracing::{info, warn};

/// One signature, no priority fee: what `ChainClient::submit` charges.
const FEE_LAMPORTS: u64 = 5_000;
/// Wallets swept concurrently.
const CONCURRENCY: usize = 128;
/// Token instructions per transaction (burn + close pairs).
const PER_TRANSACTION: usize = 8;
const METAPLEX: Pubkey = Pubkey::from_str_const("metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s");
const NATIVE_MINT: Pubkey = Pubkey::from_str_const("So11111111111111111111111111111111111111112");
const LAMPORTS_PER_SOL: f64 = 1e9;

type SharedSigner = Arc<dyn Signer + Send + Sync>;

/// Every participant wallet: makers, arb bots, takers.
fn participants(scenario: &Scenario) -> Vec<(String, Keypair)> {
    let mut out: Vec<(String, Keypair)> = scenario
        .config
        .makers
        .iter()
        .map(|maker| {
            (
                format!("maker/{}", maker.name),
                scenario.maker_wallet(&maker.name),
            )
        })
        .collect();
    if let Some(arb) = &scenario.config.arb {
        out.extend(
            arb.bots
                .iter()
                .map(|bot| (format!("arb/{}", bot.name), scenario.arb_wallet(&bot.name))),
        );
    }
    if let Some(flow) = &scenario.config.flow {
        out.extend((0..flow.takers).map(|i| (format!("taker/{i}"), scenario.taker_wallet(i))));
    }
    out
}

/// Close every maker's venue exposure so its tokens are back in its wallet.
async fn unwind_makers(chain: &ChainClient, scenario: &Scenario) -> Vec<Value> {
    futures::stream::iter(scenario.config.makers.iter().map(|maker| async move {
        let wallet: SharedSigner = Arc::new(scenario.maker_wallet(&maker.name));
        let outcome = match (&maker.strategy, maker.venue) {
            (Strategy::ClobLadder { .. }, Venue::Manifest) => {
                unwind_manifest(chain, scenario, maker, wallet).await
            }
            (_, Venue::OrcaWhirlpool | Venue::RaydiumClmm) => {
                unwind_clmm(chain, scenario, maker.venue, wallet).await
            }
            _ => Ok(json!({})),
        };
        match outcome {
            Ok(done) => json!({ "maker": maker.name, "unwound": done }),
            Err(error) => {
                warn!(maker = %maker.name, error = %format!("{error:#}"), "unwind failed");
                json!({ "maker": maker.name, "error": format!("{error:#}") })
            }
        }
    }))
    .buffer_unordered(16)
    .collect()
    .await
}

async fn unwind_manifest(
    chain: &ChainClient,
    scenario: &Scenario,
    maker: &mtm_scenario::MakerConfig,
    wallet: SharedSigner,
) -> Result<Value> {
    let Some(market) = scenario
        .pool_address(chain, maker.venue, &maker.base, &maker.quote)
        .await?
    else {
        return Ok(json!({ "market": null }));
    };
    let owner = wallet.pubkey();
    let client = manifest_dex_client::ManifestClient::new(
        chain.clone(),
        scenario.programs.manifest,
        wallet,
        10_000,
        400_000,
        0,
        CommitmentConfig::confirmed(),
    )?;
    let snapshot = client.load_market(market).await?;
    let Some(seat) = snapshot.book.seat(&owner) else {
        return Ok(json!({ "market": market.to_string(), "seat": false }));
    };
    let orders = snapshot.book.orders_of(seat.index).count();
    if orders > 0 {
        client.cancel_all_orders(market).await?;
    }
    client.withdraw_all(market).await?;
    Ok(json!({ "market": market.to_string(), "ordersCancelled": orders, "withdrawn": true }))
}

/// Remove all liquidity, collect fees, and close every position NFT the
/// wallet holds (closing returns the position, NFT mint, and NFT account rent).
async fn unwind_clmm(
    chain: &ChainClient,
    scenario: &Scenario,
    venue: Venue,
    wallet: SharedSigner,
) -> Result<Value> {
    let owner = wallet.pubkey();
    let program = scenario.programs.of(venue);
    let confirmed = CommitmentConfig::confirmed();
    let nfts: Vec<Pubkey> = token_accounts(chain, &owner)
        .await?
        .into_iter()
        .filter(|account| account.decimals == 0 && account.amount == 1)
        .map(|account| account.mint)
        .collect();
    let mut closed = Vec::new();
    for mint in nfts {
        // Decrease fails on an empty position; collect and close still apply.
        let result: Result<()> = match venue {
            Venue::OrcaWhirlpool => {
                let client = orca_whirlpool_client::OrcaWhirlpoolClient::new(
                    chain.clone(),
                    program,
                    Arc::clone(&wallet),
                    10_000,
                    400_000,
                    0,
                    confirmed,
                )?;
                if let Err(error) = client
                    .decrease_liquidity(orca_whirlpool_client::DecreaseLiquidityParams {
                        position_mint: mint,
                        amount: None,
                    })
                    .await
                {
                    info!(%mint, error = %error, "decrease skipped");
                }
                client.collect_position(mint).await?;
                client
                    .close_position(mint)
                    .await
                    .map(|_| ())
                    .map_err(Into::into)
            }
            Venue::RaydiumClmm => {
                let client = raydium_clmm_client::RaydiumClmmClient::new(
                    chain.clone(),
                    program,
                    Arc::clone(&wallet),
                    10_000,
                    400_000,
                    0,
                    confirmed,
                )?;
                if let Err(error) = client
                    .decrease_liquidity(raydium_clmm_client::DecreaseLiquidityParams {
                        position_mint: mint,
                        amount: None,
                    })
                    .await
                {
                    info!(%mint, error = %error, "decrease skipped");
                }
                client.collect_position(mint).await?;
                client
                    .close_position(mint)
                    .await
                    .map(|_| ())
                    .map_err(Into::into)
            }
            Venue::Manifest => Ok(()),
        };
        match result {
            Ok(()) => closed.push(mint.to_string()),
            Err(error) => warn!(%mint, error = %format!("{error:#}"), "closing position failed"),
        }
    }
    Ok(json!({ "positionsClosed": closed }))
}

#[derive(Debug, Clone)]
struct TokenAccount {
    address: Pubkey,
    program: Pubkey,
    mint: Pubkey,
    amount: u64,
    decimals: u8,
}

/// Every token account `owner` holds under either token program, read at
/// the client's commitment.
async fn token_accounts(chain: &ChainClient, owner: &Pubkey) -> Result<Vec<TokenAccount>> {
    let commitment = chain.rpc().commitment().commitment.to_string();
    let mut out = Vec::new();
    for program in [spl::TOKEN_PROGRAM_ID, spl::TOKEN_2022_PROGRAM_ID] {
        let response = raw_rpc(
            chain,
            "getTokenAccountsByOwner",
            json!([owner.to_string(), { "programId": program.to_string() },
                   { "encoding": "jsonParsed", "commitment": commitment }]),
        )
        .await?;
        for account in response["value"].as_array().into_iter().flatten() {
            let info = &account["account"]["data"]["parsed"]["info"];
            let parse = |value: &Value| value.as_str().and_then(|s| Pubkey::from_str(s).ok());
            let (Some(address), Some(mint)) = (parse(&account["pubkey"]), parse(&info["mint"]))
            else {
                continue;
            };
            out.push(TokenAccount {
                address,
                program,
                mint,
                amount: info["tokenAmount"]["amount"]
                    .as_str()
                    .and_then(|a| a.parse().ok())
                    .unwrap_or(0),
                decimals: info["tokenAmount"]["decimals"].as_u64().unwrap_or(0) as u8,
            });
        }
    }
    Ok(out)
}

async fn raw_rpc(chain: &ChainClient, method: &str, params: Value) -> Result<Value> {
    let response: Value = reqwest::Client::new()
        .post(chain.rpc().url())
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
        .send()
        .await?
        .json()
        .await?;
    if let Some(error) = response.get("error") {
        anyhow::bail!("{method}: {error}");
    }
    Ok(response["result"].clone())
}

/// `BurnChecked` (discriminant 15), same layout for Token and Token-2022.
fn burn_checked(account: &TokenAccount, owner: &Pubkey) -> Instruction {
    let mut data = vec![15];
    data.extend_from_slice(&account.amount.to_le_bytes());
    data.push(account.decimals);
    Instruction {
        program_id: account.program,
        accounts: vec![
            AccountMeta::new(account.address, false),
            AccountMeta::new(account.mint, false),
            AccountMeta::new_readonly(*owner, true),
        ],
        data,
    }
}

#[derive(Debug, Default)]
struct Sweep {
    accounts_closed: u64,
    lamports_swept: u64,
    /// Token accounts left open: a non-scenario token with a balance (e.g.
    /// a position NFT whose close failed); burning it would destroy value.
    left_open: Vec<String>,
    /// Lamports left because they cannot pay the transfer's own fee.
    dust: u64,
}

/// Burn scenario tokens, close every token account (rent to `treasury`),
/// then send the wallet's remaining SOL to `treasury`, leaving it at zero.
async fn sweep(
    chain: &ChainClient,
    scenario: &Scenario,
    wallet: &Keypair,
    treasury: &Pubkey,
) -> Result<Sweep> {
    let owner = wallet.pubkey();
    let mut out = Sweep::default();
    let mut instructions = Vec::new();
    for account in token_accounts(chain, &owner).await? {
        if account.amount > 0 && account.mint != NATIVE_MINT {
            if scenario.token_by_mint(&account.mint).is_none() {
                out.left_open.push(account.address.to_string());
                continue;
            }
            instructions.push(burn_checked(&account, &owner));
        }
        instructions.push(spl::close_account(
            &account.program,
            &account.address,
            treasury,
            &owner,
        ));
        out.accounts_closed += 1;
    }
    for chunk in instructions.chunks(PER_TRANSACTION) {
        chain
            .submit(chunk.to_vec(), wallet, &[], SubmissionConfig::default())
            .await
            .with_context(|| format!("closing token accounts of {owner}"))?;
    }
    let lamports = chain.rpc().get_balance(&owner).await?;
    if lamports > FEE_LAMPORTS {
        let amount = lamports - FEE_LAMPORTS;
        chain
            .submit(
                vec![spl::system_transfer(&owner, treasury, amount)],
                wallet,
                &[],
                SubmissionConfig::default(),
            )
            .await
            .with_context(|| format!("sweeping SOL of {owner}"))?;
        out.lamports_swept = amount;
    } else {
        out.dust = lamports;
    }
    Ok(out)
}

async fn sweep_all(
    chain: &ChainClient,
    scenario: &Scenario,
    wallets: Vec<(String, Keypair)>,
    treasury: &Pubkey,
) -> Value {
    let results: Vec<(String, Result<Sweep>)> =
        futures::stream::iter(wallets.into_iter().map(|(label, wallet)| async move {
            let result = sweep(chain, scenario, &wallet, treasury).await;
            (label, result)
        }))
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await;
    let (mut closed, mut swept, mut dust) = (0, 0, 0);
    let (mut left_open, mut failed) = (Vec::new(), Vec::new());
    for (label, result) in results {
        match result {
            Ok(done) => {
                closed += done.accounts_closed;
                swept += done.lamports_swept;
                dust += done.dust;
                left_open.extend(
                    done.left_open
                        .into_iter()
                        .map(|a| json!({ "wallet": label, "account": a })),
                );
            }
            Err(error) => {
                warn!(%label, error = %format!("{error:#}"), "sweep failed");
                failed.push(json!({ "wallet": label, "error": format!("{error:#}") }));
            }
        }
    }
    json!({
        "tokenAccountsClosed": closed,
        "solSwept": swept as f64 / LAMPORTS_PER_SOL,
        "dustLamports": dust,
        "leftOpen": left_open,
        "failed": failed,
    })
}

/// Unwind and sweep every participant into the faucet; with `operators`,
/// also sweep these operator wallets (mint/program authorities, …).
pub async fn teardown(
    chain: &ChainClient,
    scenario: &Scenario,
    faucet: &Pubkey,
    operators: Vec<(String, Keypair)>,
) -> Result<Value> {
    let before = chain.rpc().get_balance(faucet).await?;
    let makers = unwind_makers(chain, scenario).await;
    let wallets = participants(scenario);
    let count = wallets.len();
    info!(wallets = count, "sweeping participant wallets");
    let participants = sweep_all(chain, scenario, wallets, faucet).await;
    let operators = if operators.is_empty() {
        Value::Null
    } else {
        sweep_all(chain, scenario, operators, faucet).await
    };
    let after = chain.rpc().get_balance(faucet).await?;
    Ok(json!({
        "faucet": faucet.to_string(),
        "faucetGainedSol": (after as f64 - before as f64) / LAMPORTS_PER_SOL,
        "makers": makers,
        "participants": json!({ "wallets": count, "sweep": participants }),
        "operators": operators,
    }))
}

/// Anchor account discriminator: the first 8 bytes of sha256("account:<Name>").
fn discriminator(name: &str) -> [u8; 8] {
    let hash = Sha256::digest(format!("account:{name}").as_bytes());
    hash[..8].try_into().expect("8 bytes")
}

const ORCA_ACCOUNTS: &[&str] = &[
    "Whirlpool",
    "WhirlpoolsConfig",
    "WhirlpoolsConfigExtension",
    "FeeTier",
    "AdaptiveFeeTier",
    "TickArray",
    "FixedTickArray",
    "DynamicTickArray",
    "Oracle",
    "Position",
    "PositionBundle",
    "TokenBadge",
    "LockConfig",
];
const RAYDIUM_ACCOUNTS: &[&str] = &[
    "AmmConfig",
    "PoolState",
    "TickArrayState",
    "DynTickArrayState",
    "TickArrayBitmapExtension",
    "ObservationState",
    "PersonalPositionState",
    "ProtocolPositionState",
    "OperationState",
    "SupportMintAssociated",
    "LimitOrderState",
];

#[derive(Default)]
struct Bucket {
    accounts: u64,
    lamports: u64,
}

fn add(buckets: &mut BTreeMap<String, Bucket>, label: String, lamports: u64) {
    let bucket = buckets.entry(label).or_default();
    bucket.accounts += 1;
    bucket.lamports += lamports;
}

fn render(buckets: BTreeMap<String, Bucket>) -> (Value, u64) {
    let total = buckets.values().map(|b| b.lamports).sum();
    let rows = buckets
        .into_iter()
        .map(|(label, b)| {
            (
                label,
                json!({ "accounts": b.accounts, "sol": b.lamports as f64 / LAMPORTS_PER_SOL }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    (Value::Object(rows), total)
}

/// Accounts owned by `program`, labelled by Anchor discriminator (or size).
async fn program_accounts(
    chain: &ChainClient,
    program: &Pubkey,
    names: &[&str],
) -> Result<BTreeMap<String, Bucket>> {
    let known: Vec<([u8; 8], &str)> = names.iter().map(|n| (discriminator(n), *n)).collect();
    let accounts = raw_rpc(
        chain,
        "getProgramAccounts",
        json!([program.to_string(), { "encoding": "base64", "dataSlice": { "offset": 0, "length": 8 } }]),
    )
    .await?;
    let mut buckets = BTreeMap::new();
    for account in accounts.as_array().into_iter().flatten() {
        let lamports = account["account"]["lamports"].as_u64().unwrap_or(0);
        let head = account["account"]["data"][0]
            .as_str()
            .and_then(|b| base64_decode(b).ok())
            .unwrap_or_default();
        let space = account["account"]["space"].as_u64().unwrap_or(0);
        let label = known
            .iter()
            .find(|(d, _)| head.starts_with(d))
            .map(|(_, n)| (*n).to_owned())
            .unwrap_or_else(|| format!("{space}-byte account"));
        add(&mut buckets, label, lamports);
    }
    Ok(buckets)
}

fn base64_decode(text: &str) -> Result<Vec<u8>> {
    Ok(base64::engine::general_purpose::STANDARD.decode(text)?)
}

/// Rent still locked after a teardown, by owner and account type.
pub async fn leftovers(
    chain: &ChainClient,
    scenario: &Scenario,
    operators: &[(String, Pubkey)],
) -> Result<Value> {
    let programs = &scenario.programs;
    let mut sections = serde_json::Map::new();
    let mut grand = 0_u64;

    for (name, program, names) in [
        ("orca-whirlpool", programs.orca_whirlpool, ORCA_ACCOUNTS),
        ("raydium-clmm", programs.raydium_clmm, RAYDIUM_ACCOUNTS),
        ("manifest", programs.manifest, &[][..]),
    ] {
        let mut buckets = program_accounts(chain, &program, names).await?;
        if name == "manifest" {
            // Manifest is not Anchor: its accounts are markets (and globals).
            buckets = buckets
                .into_iter()
                .map(|(label, b)| (format!("market ({label})"), b))
                .collect();
        }
        let (rows, total) = render(buckets);
        grand += total;
        sections.insert(
            format!("{name} program accounts"),
            json!({ "sol": total as f64 / LAMPORTS_PER_SOL, "byType": rows }),
        );
    }

    // Token accounts of scenario mints: pool / market vaults, plus anything
    // a participant still holds.
    let wallets: HashSet<Pubkey> = participants(scenario)
        .iter()
        .map(|(_, k)| k.pubkey())
        .collect();
    let mut buckets = BTreeMap::new();
    for token in scenario.tokens.values() {
        let accounts = raw_rpc(
            chain,
            "getProgramAccounts",
            json!([token.token_program.to_string(), {
                "encoding": "base64",
                "dataSlice": { "offset": 32, "length": 32 },
                "filters": [{ "memcmp": { "offset": 0, "bytes": token.mint.to_string() } }],
            }]),
        )
        .await?;
        for account in accounts.as_array().into_iter().flatten() {
            let lamports = account["account"]["lamports"].as_u64().unwrap_or(0);
            let owner = account["account"]["data"][0]
                .as_str()
                .and_then(|b| base64_decode(b).ok())
                .and_then(|raw| Pubkey::try_from(raw.as_slice()).ok());
            if owner.is_some_and(|o| o == token.mint) {
                continue; // the mint itself also matches offset 0 on some layouts
            }
            let label = match owner {
                Some(o) if wallets.contains(&o) => "participant token accounts (not swept)",
                Some(o) if operators.iter().any(|(_, k)| *k == o) => "operator token accounts",
                _ => "pool / market vaults",
            };
            add(&mut buckets, label.to_owned(), lamports);
        }
    }
    let (rows, total) = render(buckets);
    grand += total;
    sections.insert(
        "token accounts (scenario mints)".into(),
        json!({ "sol": total as f64 / LAMPORTS_PER_SOL, "byType": rows }),
    );

    // Mints and their Metaplex metadata.
    let mut buckets = BTreeMap::new();
    for token in scenario.tokens.values() {
        let (metadata, _) = Pubkey::find_program_address(
            &[b"metadata", METAPLEX.as_ref(), token.mint.as_ref()],
            &METAPLEX,
        );
        for (label, address) in [("mints", token.mint), ("Metaplex metadata", metadata)] {
            let lamports = chain.rpc().get_balance(&address).await.unwrap_or(0);
            if lamports > 0 {
                add(&mut buckets, label.to_owned(), lamports);
            }
        }
    }
    let (rows, total) = render(buckets);
    grand += total;
    sections.insert(
        "mints".into(),
        json!({ "sol": total as f64 / LAMPORTS_PER_SOL, "byType": rows }),
    );

    // The DEX programs (program + programdata), unless closed.
    let mut buckets = BTreeMap::new();
    for (name, program) in [
        ("orca-whirlpool", programs.orca_whirlpool),
        ("raydium-clmm", programs.raydium_clmm),
        ("manifest", programs.manifest),
    ] {
        let Ok(account) = chain.rpc().get_account(&program).await else {
            continue;
        };
        let mut lamports = account.lamports;
        // Upgradeable program account: u32 tag + programdata address.
        if let Some(raw) = account.data.get(4..36)
            && let Ok(programdata) = Pubkey::try_from(raw)
        {
            lamports += chain.rpc().get_balance(&programdata).await.unwrap_or(0);
        }
        add(&mut buckets, name.to_owned(), lamports);
    }
    let (rows, total) = render(buckets);
    grand += total;
    sections.insert(
        "programs (program + programdata)".into(),
        json!({ "sol": total as f64 / LAMPORTS_PER_SOL, "byType": rows }),
    );

    // Participant wallets should be empty; operators hold what was recovered.
    let mut residual = 0_u64;
    let mut nonempty = 0_u64;
    let all: Vec<Pubkey> = wallets.into_iter().collect();
    for chunk in all.chunks(100) {
        for account in chain
            .rpc()
            .get_multiple_accounts(chunk)
            .await?
            .into_iter()
            .flatten()
        {
            residual += account.lamports;
            nonempty += 1;
        }
    }
    let mut operator_balances = serde_json::Map::new();
    for (label, key) in operators {
        let lamports = chain.rpc().get_balance(key).await.unwrap_or(0);
        operator_balances.insert(label.clone(), json!(lamports as f64 / LAMPORTS_PER_SOL));
    }
    Ok(json!({
        "lockedSol": grand as f64 / LAMPORTS_PER_SOL,
        "locked": Value::Object(sections),
        "participantWallets": { "nonEmpty": nonempty, "sol": residual as f64 / LAMPORTS_PER_SOL },
        "operatorBalancesSol": Value::Object(operator_balances),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_discriminators_match_the_published_idls() {
        // From the Orca Whirlpools and Raydium CLMM IDLs.
        assert_eq!(
            discriminator("Whirlpool"),
            [63, 149, 209, 12, 225, 128, 99, 9]
        );
        assert_eq!(
            discriminator("PoolState"),
            [247, 237, 227, 245, 215, 195, 222, 70]
        );
    }

    #[test]
    fn burn_checked_encodes_amount_and_decimals() {
        let account = TokenAccount {
            address: Pubkey::new_unique(),
            program: spl::TOKEN_PROGRAM_ID,
            mint: Pubkey::new_unique(),
            amount: 1_234,
            decimals: 6,
        };
        let owner = Pubkey::new_unique();
        let ix = burn_checked(&account, &owner);
        assert_eq!(ix.data, [15, 210, 4, 0, 0, 0, 0, 0, 0, 6]);
        assert!(ix.accounts[2].is_signer && ix.accounts[2].pubkey == owner);
        assert!(ix.accounts[0].is_writable && ix.accounts[1].is_writable);
    }
}
