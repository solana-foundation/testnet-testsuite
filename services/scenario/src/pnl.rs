//! Participant accounting: snapshots of everything each scenario wallet owns
//! (wallet tokens and SOL, Manifest seat balances and resting orders, LP
//! positions) valued at oracle prices, and a report comparing two snapshots
//! with on-chain transaction counts and trade counts from Prometheus, and
//! a per-venue breakdown of the window's transactions (counts and errors
//! from every block, compute from a sample of full blocks).
//!
//! Reads fail loudly: an RPC error never turns into "no positions" or "empty
//! block". The report's Prometheus parts are best-effort and say so in
//! `warnings`.

use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use futures::StreamExt;
use mtm_chain::ChainClient;
use mtm_scenario::funding::balances;
use mtm_scenario::pricing::PriceBook;
use mtm_scenario::venue::to_ui;
use mtm_scenario::{Scenario, Strategy, Token, Venue};
use oracle_client::OracleClient;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_signer::null_signer::NullSigner;

use crate::rpc::{JsonRpc, RpcError, backoff};

const D18: u128 = 1_000_000_000_000_000_000;
/// A maker's wallet token atoms (in `scenario.tokens` order) and its venue
/// holdings, read consistently.
type MakerRead = (Vec<u64>, Option<(String, BTreeMap<String, Decimal>)>);
/// Concurrent RPC reads per snapshot / report.
const CONCURRENCY: usize = 32;
/// Consensus votes: a real validator (solana-test-validator) lands one per
/// slot; Surfpool has none. Excluded so the two compare.
const VOTE_PROGRAM: &str = "Vote111111111111111111111111111111111111111";
/// `getBlock` errors that mean "no block in this slot": skipped, or
/// missing in long-term storage.
const SLOT_SKIPPED: [i64; 2] = [-32007, -32009];
/// "Block not available yet": retried, then fatal.
const BLOCK_NOT_AVAILABLE: i64 = -32004;
/// Peak TPS window: 25 consecutive slots (~10 s).
const PEAK_SLOTS: u64 = 25;
/// At most this many distinct errors listed per venue.
const TOP_ERRORS: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub time_us: i64,
    pub slot: u64,
    /// USD per token, by token name.
    pub prices: BTreeMap<String, Decimal>,
    pub participants: Vec<Holdings>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Holdings {
    pub role: String,
    pub kind: String,
    pub wallet: String,
    /// Token name → UI amount, summed over wallet, seat, and positions.
    pub tokens: BTreeMap<String, Decimal>,
    pub sol: Decimal,
    /// Where a maker's tokens sit (wallet, venue), for inspection; empty
    /// for wallet-only participants, whose `tokens` say it all.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub breakdown: BTreeMap<String, BTreeMap<String, Decimal>>,
}

pub async fn snapshot(chain: &ChainClient, rpc: &JsonRpc, scenario: &Scenario) -> Result<Snapshot> {
    let oracle = OracleClient::new(&scenario.config.oracle_url);
    let book = PriceBook::fetch(&oracle, scenario.config.max_price_age_ms).await?;
    let mut prices = BTreeMap::new();
    for token in scenario.tokens.values() {
        prices.insert(token.name.clone(), book.usd(token)?);
    }
    let people = scenario.participants();
    let tokens: Vec<&Token> = scenario.tokens.values().collect();
    // Every wallet balance in a few batched reads, so the snapshot is close
    // to a single point in time even with thousands of participants.
    let pairs: Vec<(Pubkey, &Token)> = people
        .iter()
        .flat_map(|p| tokens.iter().map(move |token| (p.wallet, *token)))
        .collect();
    let wallets: Vec<Pubkey> = people.iter().map(|p| p.wallet).collect();
    let (token_atoms, lamports) = balances(chain, &pairs, &wallets).await?;
    // Venue holdings (seats, orders, positions) only exist for makers. A
    // maker moving funds between wallet and venue mid-read (a re-centre)
    // would be counted twice or not at all, so each maker is read
    // optimistically: wallet, venue, wallet again, retried until the wallet
    // did not change around the venue read.
    let makers: Vec<Option<MakerRead>> = futures::stream::iter(people.iter().map(|person| {
        let tokens = &tokens;
        async move {
            let Some(name) = person.role.strip_prefix("maker/") else {
                return Ok(None);
            };
            let maker = scenario
                .config
                .makers
                .iter()
                .find(|m| m.name == name)
                .context("maker vanished")?;
            // Resolved once: a Manifest lookup is a program-wide scan.
            let market = match maker.strategy {
                Strategy::ClobLadder { .. } => {
                    scenario
                        .pool_address(chain, maker.venue, &maker.base, &maker.quote)
                        .await?
                }
                _ => None,
            };
            let pairs: Vec<(Pubkey, &Token)> =
                tokens.iter().map(|token| (person.wallet, *token)).collect();
            let mut before = balances(chain, &pairs, &[]).await?.0;
            for _ in 0..5 {
                let venue = venue_holdings(chain, rpc, scenario, maker, person.wallet, market)
                    .await?
                    .map(|held| (maker.venue.as_str().to_owned(), held));
                let after = balances(chain, &pairs, &[]).await?.0;
                if after == before {
                    return Ok::<_, anyhow::Error>(Some((after, venue)));
                }
                before = after;
            }
            anyhow::bail!("maker {name} kept moving funds during the snapshot")
        }
    }))
    .buffered(CONCURRENCY)
    .collect::<Vec<_>>()
    .await
    .into_iter()
    .collect::<Result<_>>()?;
    let participants = people
        .into_iter()
        .zip(makers)
        .enumerate()
        .map(|(index, (person, maker))| {
            let is_maker = maker.is_some();
            let (atoms, venue) = maker.unwrap_or_else(|| {
                (
                    token_atoms[index * tokens.len()..(index + 1) * tokens.len()].to_vec(),
                    None,
                )
            });
            let wallet_tokens: BTreeMap<String, Decimal> = tokens
                .iter()
                .zip(&atoms)
                .map(|(token, atoms)| (token.name.clone(), to_ui(token, *atoms)))
                .collect();
            let mut breakdown: BTreeMap<String, BTreeMap<String, Decimal>> =
                BTreeMap::from([("wallet".to_owned(), wallet_tokens)]);
            breakdown.extend(venue);
            let mut totals: BTreeMap<String, Decimal> = BTreeMap::new();
            for place in breakdown.values() {
                for (name, amount) in place {
                    *totals.entry(name.clone()).or_default() += *amount;
                }
            }
            if !is_maker {
                breakdown.clear();
            }
            Holdings {
                role: person.role,
                kind: person.kind.label().to_owned(),
                wallet: person.wallet.to_string(),
                tokens: totals,
                sol: Decimal::from(lamports[index]) / Decimal::from(1_000_000_000_u64),
                breakdown,
            }
        })
        .collect();
    // Taken after the reads: with participants frozen while a snapshot
    // runs, this is the state the reads saw.
    let (slot, time_us) = (chain.rpc().get_slot().await?, mtm_common::time::now_us());
    Ok(Snapshot {
        time_us,
        slot,
        prices,
        participants,
    })
}

/// Tokens a maker holds inside its venue (Manifest seat + resting orders,
/// or its open LP positions). `market`: the maker's Manifest market.
async fn venue_holdings(
    chain: &ChainClient,
    rpc: &JsonRpc,
    scenario: &Scenario,
    maker: &mtm_scenario::MakerConfig,
    wallet: Pubkey,
    market: Option<Pubkey>,
) -> Result<Option<BTreeMap<String, Decimal>>> {
    let base = scenario.token(&maker.base)?;
    let quote = scenario.token(&maker.quote)?;
    let mut out = BTreeMap::new();
    match (&maker.strategy, maker.venue) {
        (Strategy::ClobLadder { .. }, Venue::Manifest) => {
            let Some(market) = market else {
                return Ok(None);
            };
            let snapshot = manifest_dex_client::MarketSnapshot::load(
                chain,
                scenario.programs.manifest,
                market,
            )
            .await?;
            let Some(seat) = snapshot.book.seat(&wallet) else {
                return Ok(None);
            };
            let (base_atoms, quote_atoms) = manifest_holdings(
                seat.base_withdrawable,
                seat.quote_withdrawable,
                snapshot
                    .book
                    .asks
                    .iter()
                    .filter(|o| o.trader_index == seat.index)
                    .map(|o| o.base_atoms),
                snapshot
                    .book
                    .bids
                    .iter()
                    .filter(|o| o.trader_index == seat.index)
                    .map(|o| (o.price_raw, o.base_atoms)),
            )
            .with_context(|| format!("maker {}: seat value overflows", maker.name))?;
            out.insert(base.name.clone(), to_ui(base, base_atoms));
            out.insert(quote.name.clone(), to_ui(quote, quote_atoms));
        }
        (Strategy::ClmmRange { .. }, venue) => {
            // Positions are found from the position NFTs the wallet holds
            // (not the mm's state file, which is briefly absent while a
            // position is re-centred).
            let nfts = position_nfts(rpc, &wallet).await?;
            let reader: Arc<dyn Signer + Send + Sync> = Arc::new(NullSigner::new(&wallet));
            let program = scenario.programs.of(venue);
            let confirmed = CommitmentConfig::confirmed();
            for mint in nfts {
                // `MalformedAccount` is "this NFT is not one of this venue's
                // positions" (no position account, or someone else's); any
                // other error (RPC, decoding a real position) is fatal, so
                // the optimistic loop or the caller sees it.
                let value = match venue {
                    Venue::OrcaWhirlpool => {
                        match orca_whirlpool_client::OrcaWhirlpoolClient::new(
                            chain.clone(),
                            program,
                            Arc::clone(&reader),
                            0,
                            200_000,
                            0,
                            confirmed,
                        )?
                        .position_value(mint)
                        .await
                        {
                            Ok(v) => Some((v.mint_a, v.mint_b, v.amount_a, v.amount_b)),
                            Err(orca_whirlpool_client::OrcaWhirlpoolError::MalformedAccount {
                                ..
                            }) => None,
                            Err(error) => return Err(error.into()),
                        }
                    }
                    Venue::RaydiumClmm => {
                        match raydium_clmm_client::RaydiumClmmClient::new(
                            chain.clone(),
                            program,
                            Arc::clone(&reader),
                            0,
                            200_000,
                            0,
                            confirmed,
                        )?
                        .position_value(mint)
                        .await
                        {
                            Ok(v) => Some((v.mint_a, v.mint_b, v.amount_a, v.amount_b)),
                            Err(raydium_clmm_client::RaydiumClmmError::MalformedAccount {
                                ..
                            }) => None,
                            Err(error) => return Err(error.into()),
                        }
                    }
                    Venue::Manifest => return Ok(None),
                };
                let Some((mint_a, mint_b, amount_a, amount_b)) = value else {
                    tracing::debug!(maker = %maker.name, %mint, "NFT is not a position of this venue");
                    continue;
                };
                for (mint, amount) in [(mint_a, amount_a), (mint_b, amount_b)] {
                    let token = scenario
                        .token_by_mint(&mint)
                        .context("position mint is not a scenario token")?;
                    *out.entry(token.name.clone()).or_default() += to_ui(token, amount);
                }
            }
            if out.is_empty() {
                return Ok(None);
            }
        }
        _ => return Ok(None),
    }
    Ok(Some(out))
}

/// A Manifest seat's base and quote atoms: withdrawable balances plus what
/// resting orders lock (asks lock base; bids lock quote at their price,
/// rounded up as the program does). Errors instead of saturating.
fn manifest_holdings(
    base_withdrawable: u64,
    quote_withdrawable: u64,
    asks: impl Iterator<Item = u64>,
    bids: impl Iterator<Item = (u128, u64)>,
) -> Result<(u64, u64)> {
    let overflow = || anyhow!("Manifest seat value overflows");
    let mut base = u128::from(base_withdrawable);
    for ask in asks {
        base = base.checked_add(u128::from(ask)).ok_or_else(overflow)?;
    }
    let mut quote = u128::from(quote_withdrawable);
    for (price_raw, base_atoms) in bids {
        let locked = price_raw
            .checked_mul(u128::from(base_atoms))
            .ok_or_else(overflow)?
            .div_ceil(D18);
        quote = quote.checked_add(locked).ok_or_else(overflow)?;
    }
    Ok((
        u64::try_from(base).map_err(|_| overflow())?,
        u64::try_from(quote).map_err(|_| overflow())?,
    ))
}

/// Mints of the NFTs (amount 1, 0 decimals) `owner` holds under either
/// token program: candidate CLMM position mints. Read at the client's
/// commitment like every other snapshot read (the RPC default, finalized,
/// lags confirmed by ~32 slots on a real validator: recently opened
/// positions would be missing and recently closed ones still listed).
async fn position_nfts(rpc: &JsonRpc, owner: &Pubkey) -> Result<Vec<Pubkey>> {
    let mut mints = Vec::new();
    for program in [
        "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
        "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
    ] {
        let result = rpc
            .call(
                "getTokenAccountsByOwner",
                json!([owner.to_string(), { "programId": program }, { "encoding": "jsonParsed", "commitment": rpc.commitment }]),
            )
            .await
            .with_context(|| format!("listing {owner}'s token accounts"))?;
        let accounts = result["value"]
            .as_array()
            .context("getTokenAccountsByOwner: no value array")?;
        for account in accounts {
            let info = &account["account"]["data"]["parsed"]["info"];
            if info["tokenAmount"]["decimals"] == 0
                && info["tokenAmount"]["amount"] == "1"
                && let Some(mint) = info["mint"].as_str().and_then(|m| Pubkey::from_str(m).ok())
            {
                mints.push(mint);
            }
        }
    }
    Ok(mints)
}

/// Holdings at `prices`; a held token without a price is an error, not 0.
fn value(
    tokens: &BTreeMap<String, Decimal>,
    prices: &BTreeMap<String, Decimal>,
) -> Result<Decimal> {
    tokens
        .iter()
        .filter(|(_, amount)| !amount.is_zero())
        .map(|(name, amount)| {
            let price = prices
                .get(name)
                .with_context(|| format!("no price for {name} in the snapshot"))?;
            Ok(*amount * *price)
        })
        .sum()
}

/// One landed non-vote transaction.
#[derive(Debug, PartialEq)]
struct BlockTx {
    signers: Vec<String>,
    ok: bool,
    slot: u64,
    /// Bit `i` set: the transaction calls `venues[i]`'s program.
    venues: u8,
    /// [`error_label`] of a failed transaction.
    error: Option<String>,
}

/// The venues whose programs the report attributes transactions to.
pub struct VenuePrograms(pub Vec<(&'static str, Pubkey)>);

impl VenuePrograms {
    pub fn of(scenario: &Scenario) -> Self {
        let programs = &scenario.programs;
        Self(vec![
            (Venue::RaydiumClmm.as_str(), programs.raydium_clmm),
            (Venue::OrcaWhirlpool.as_str(), programs.orca_whirlpool),
            (Venue::Manifest.as_str(), programs.manifest),
        ])
    }

    /// Bitmask of the venues among `keys`.
    fn mask<'a>(&self, keys: impl Iterator<Item = &'a str>) -> u8 {
        let ids: Vec<String> = self.0.iter().map(|(_, id)| id.to_string()).collect();
        keys.fold(0, |mask, key| {
            ids.iter()
                .position(|id| id == key)
                .map_or(mask, |i| mask | 1 << i)
        })
    }
}

/// A short label for a transaction error: `Custom(6018)` for program
/// errors, the variant name otherwise.
fn error_label(error: &Value) -> String {
    if let Some([_, detail]) = error["InstructionError"].as_array().map(Vec::as_slice) {
        if let Some(code) = detail["Custom"].as_u64() {
            return format!("Custom({code})");
        }
        return match detail {
            Value::String(name) => name.clone(),
            other => other
                .as_object()
                .and_then(|o| o.keys().next().cloned())
                .unwrap_or_else(|| other.to_string()),
        };
    }
    match error {
        Value::String(name) => name.clone(),
        Value::Object(o) => o.keys().next().cloned().unwrap_or_default(),
        other => other.to_string(),
    }
}

/// A `getBlock` result's non-vote transactions, plus how many votes it
/// held.
fn parse_block(block: &Value, slot: u64, venues: &VenuePrograms) -> (Vec<BlockTx>, u64) {
    let empty = vec![];
    let (mut out, mut votes) = (Vec::new(), 0);
    for tx in block["transactions"].as_array().unwrap_or(&empty) {
        let keys = tx["transaction"]["accountKeys"]
            .as_array()
            .unwrap_or(&empty);
        if keys
            .iter()
            .any(|key| key["pubkey"].as_str() == Some(VOTE_PROGRAM))
        {
            votes += 1;
            continue;
        }
        let signers = keys
            .iter()
            .filter(|key| key["signer"].as_bool() == Some(true))
            .filter_map(|key| key["pubkey"].as_str().map(str::to_owned))
            .collect();
        let error = &tx["meta"]["err"];
        out.push(BlockTx {
            signers,
            ok: error.is_null(),
            slot,
            venues: venues.mask(keys.iter().filter_map(|key| key["pubkey"].as_str())),
            error: (!error.is_null()).then(|| error_label(error)),
        });
    }
    (out, votes)
}

/// Compute units of a full (`transactionDetails: full`, `encoding: json`)
/// block's non-vote transactions: (venue mask, units) each.
fn block_compute(block: &Value, venues: &VenuePrograms) -> Vec<(u8, u64)> {
    let empty = vec![];
    block["transactions"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter_map(|tx| {
            let keys = tx["transaction"]["message"]["accountKeys"].as_array()?;
            let keys = keys.iter().filter_map(Value::as_str);
            if keys.clone().any(|key| key == VOTE_PROGRAM) {
                return None;
            }
            Some((
                venues.mask(keys),
                tx["meta"]["computeUnitsConsumed"].as_u64()?,
            ))
        })
        .collect()
}

/// What the block scan saw.
#[derive(Default)]
struct BlockScan {
    transactions: Vec<BlockTx>,
    votes: u64,
    skipped_slots: u64,
}

/// One block at `details` (`accounts` or `full`), or `None` for an empty
/// slot. Only "slot skipped" errors mean an empty slot; "not available
/// yet" is retried and anything else fails.
async fn get_block(rpc: &JsonRpc, slot: u64, details: &str) -> Result<Option<Value>> {
    let params = json!([slot, {
        "encoding": "json", "transactionDetails": details, "rewards": false,
        "maxSupportedTransactionVersion": 1, "commitment": rpc.block_commitment(),
    }]);
    let mut attempt = 0;
    loop {
        attempt += 1;
        match rpc.call("getBlock", params.clone()).await {
            // A null result: no block (some RPCs answer skipped slots so).
            Ok(block) if block.is_null() => return Ok(None),
            Ok(block) => return Ok(Some(block)),
            Err(error) => match error.downcast_ref::<RpcError>().map(|e| e.code) {
                Some(code) if SLOT_SKIPPED.contains(&code) => return Ok(None),
                Some(BLOCK_NOT_AVAILABLE) if attempt < 5 => {
                    tokio::time::sleep(backoff(attempt)).await;
                }
                _ => return Err(error.context(format!("getBlock {slot}"))),
            },
        }
    }
}

/// Every transaction in `(after_slot, until_slot]`, read block by block.
/// Block scans are linear in the window, unlike per-wallet signature
/// lookups, which a local validator answers by scanning its whole history.
async fn block_transactions(
    rpc: &JsonRpc,
    after_slot: u64,
    until_slot: u64,
    venues: &VenuePrograms,
) -> Result<BlockScan> {
    let blocks = futures::stream::iter((after_slot + 1..=until_slot).map(|slot| async move {
        Ok::<_, anyhow::Error>(
            get_block(rpc, slot, "accounts")
                .await?
                .map(|block| parse_block(&block, slot, venues)),
        )
    }))
    .buffered(CONCURRENCY)
    .collect::<Vec<Result<Option<(Vec<BlockTx>, u64)>>>>()
    .await;
    let mut scan = BlockScan::default();
    for block in blocks {
        match block? {
            Some((txs, votes)) => {
                scan.transactions.extend(txs);
                scan.votes += votes;
            }
            None => scan.skipped_slots += 1,
        }
    }
    Ok(scan)
}

/// Compute units of the non-vote transactions in every `every`-th slot of
/// `(after_slot, until_slot]`: full blocks carry every log line, so the
/// whole window would multiply the scan's cost for the same averages.
async fn sampled_compute(
    rpc: &JsonRpc,
    after_slot: u64,
    until_slot: u64,
    every: u64,
    venues: &VenuePrograms,
) -> Result<(u64, Vec<(u8, u64)>)> {
    let slots: Vec<u64> = (after_slot + 1..=until_slot)
        .filter(|slot| slot % every == 0)
        .collect();
    let sampled = slots.len() as u64;
    let blocks = futures::stream::iter(slots.into_iter().map(|slot| async move {
        Ok::<_, anyhow::Error>(
            get_block(rpc, slot, "full")
                .await?
                .map(|block| block_compute(&block, venues))
                .unwrap_or_default(),
        )
    }))
    .buffered(CONCURRENCY)
    .collect::<Vec<Result<Vec<(u8, u64)>>>>()
    .await;
    let mut units = Vec::new();
    for block in blocks {
        units.extend(block?);
    }
    Ok((sampled, units))
}

/// p95 of unsorted values (nearest rank).
fn p95(values: &mut [u64]) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[(values.len() * 95).div_ceil(100) - 1]
}

/// Sampled compute: (every n-th slot, slots sampled, (venue mask, units)
/// per transaction).
type ComputeSample<'a> = (u64, u64, &'a [(u8, u64)]);

/// Landed and failed transactions, errors and sampled compute per venue,
/// plus `other` for transactions that call none of them. A multi-venue
/// route counts for every venue it touches, so venue shares can sum past
/// 100%.
fn venue_breakdown(
    transactions: &[BlockTx],
    compute: Option<ComputeSample<'_>>,
    venues: &VenuePrograms,
) -> Value {
    let names: Vec<&str> = venues
        .0
        .iter()
        .map(|(name, _)| *name)
        .chain(["other"])
        .collect();
    // A venue's bit, or none for `other`.
    let matches = |i: usize, mask: u8| {
        if i < venues.0.len() {
            mask & (1 << i) != 0
        } else {
            mask == 0
        }
    };
    let total_units: u64 = compute.map_or(0, |(_, _, units)| units.iter().map(|(_, u)| u).sum());
    let mut out = serde_json::Map::new();
    for (i, name) in names.iter().enumerate() {
        let (mut landed, mut failed) = (0_u64, 0_u64);
        let mut errors: HashMap<&str, u64> = HashMap::new();
        for tx in transactions.iter().filter(|tx| matches(i, tx.venues)) {
            if tx.ok {
                landed += 1;
            } else {
                failed += 1;
                *errors
                    .entry(tx.error.as_deref().unwrap_or("unknown"))
                    .or_default() += 1;
            }
        }
        let mut errors: Vec<(&str, u64)> = errors.into_iter().collect();
        errors.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let all = landed + failed;
        let mut venue = json!({
            "transactionsLanded": landed,
            "transactionsFailed": failed,
            "failureRatePct": if all == 0 { 0.0 } else { (failed as f64 / all as f64 * 10_000.0).round() / 100.0 },
            "errors": errors.into_iter().take(TOP_ERRORS).collect::<BTreeMap<_, _>>(),
        });
        if let Some((_, _, units)) = compute {
            let mut own: Vec<u64> = units
                .iter()
                .filter(|(mask, _)| matches(i, *mask))
                .map(|(_, u)| *u)
                .collect();
            let sum: u64 = own.iter().sum();
            venue["compute"] = json!({
                "sampledTransactions": own.len(),
                "meanUnits": if own.is_empty() { 0 } else { sum / own.len() as u64 },
                "p95Units": p95(&mut own),
                "sharePct": if total_units == 0 { 0.0 } else { (sum as f64 / total_units as f64 * 10_000.0).round() / 100.0 },
            });
        }
        out.insert((*name).to_owned(), venue);
    }
    if let Some((every, sampled_slots, units)) = compute {
        out.insert(
            "computeSample".to_owned(),
            json!({
                "everyNthSlot": every,
                "slots": sampled_slots,
                "transactions": units.len(),
                "meanUnitsPerSlot": total_units / sampled_slots.max(1),
                "note": "sharePct: of all non-vote compute in the sampled slots; meanUnitsPerSlot includes empty slots",
            }),
        );
    }
    Value::Object(out)
}

/// Landed TPS of the busiest `PEAK_SLOTS` consecutive slots, at the
/// window's mean slot time; windows shorter than that use the whole
/// window.
fn peak_tps(
    per_slot: &BTreeMap<u64, u64>,
    start_slot: u64,
    end_slot: u64,
    slot_seconds: f64,
) -> f64 {
    let window = end_slot.saturating_sub(start_slot);
    if window == 0 || slot_seconds <= 0.0 {
        return 0.0;
    }
    let width = PEAK_SLOTS.min(window);
    let landed = (start_slot + 1..=end_slot + 1 - width)
        .map(|first| {
            (first..first + width)
                .filter_map(|slot| per_slot.get(&slot))
                .sum::<u64>()
        })
        .max()
        .unwrap_or_default();
    landed as f64 / (width as f64 * slot_seconds)
}

/// Median of sorted values (mean of the middle pair for even counts).
fn median(sorted: &[Decimal]) -> Decimal {
    match sorted.len() {
        0 => Decimal::ZERO,
        n if n % 2 == 1 => sorted[n / 2],
        n => (sorted[n / 2 - 1] + sorted[n / 2]) / Decimal::TWO,
    }
}

/// (trading PnL, mark-to-market PnL, landed, failed) per participant.
type KindRow = (Decimal, Decimal, u64, u64);

fn kind_summary(rows: &mut [KindRow]) -> Value {
    rows.sort_by_key(|row| row.0);
    let n = rows.len();
    let trading: Vec<Decimal> = rows.iter().map(|row| row.0).collect();
    let sum: Decimal = trading.iter().sum();
    json!({
        "participants": n,
        "tradingPnlUsd": sum.round_dp(2).to_string(),
        "markToMarketPnlUsd": rows.iter().map(|row| row.1).sum::<Decimal>().round_dp(2).to_string(),
        "meanTradingPnlUsd": (sum / Decimal::from(n.max(1))).round_dp(2).to_string(),
        "medianTradingPnlUsd": median(&trading).round_dp(2).to_string(),
        "minTradingPnlUsd": trading.first().copied().unwrap_or_default().round_dp(2).to_string(),
        "maxTradingPnlUsd": trading.last().copied().unwrap_or_default().round_dp(2).to_string(),
        "profitable": trading.iter().filter(|pnl| pnl.is_sign_positive() && !pnl.is_zero()).count(),
        "transactionsLanded": rows.iter().map(|row| row.2).sum::<u64>(),
        "transactionsFailed": rows.iter().map(|row| row.3).sum::<u64>(),
    })
}

async fn prometheus(
    client: &reqwest::Client,
    url: &str,
    query: &str,
    at_s: f64,
) -> Result<Vec<(BTreeMap<String, String>, f64)>> {
    let response: Value = client
        .get(format!("{url}/api/v1/query"))
        .query(&[("query", query), ("time", &at_s.to_string())])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let series = response["data"]["result"]
        .as_array()
        .with_context(|| format!("Prometheus answered without a result for {query}"))?;
    series
        .iter()
        .map(|series| {
            let labels = series["metric"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned()))
                        .collect()
                })
                .unwrap_or_default();
            let value = series["value"][1]
                .as_str()
                .and_then(|v| v.parse().ok())
                .with_context(|| format!("unparseable sample in {query}"))?;
            Ok((labels, value))
        })
        .collect()
}

/// Trade counts per participant from service metrics over the window.
/// Best-effort: a failed query adds a warning and leaves its trades out.
async fn trade_counts(
    url: &str,
    range: &str,
    end_s: f64,
    warnings: &mut Vec<String>,
) -> BTreeMap<String, Value> {
    // A stuck Prometheus costs a warning, not a hung report.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let mut trades: BTreeMap<String, Value> = BTreeMap::new();
    let queries = [
        (
            "maker",
            "action",
            format!("sum by (maker, action) (increase(mm_actions_total[{range}]))"),
        ),
        (
            "arb",
            "status",
            format!("sum by (bot, status) (increase(arb_trades_total[{range}]))"),
        ),
    ];
    for (prefix, label, query) in queries {
        match prometheus(&client, url, &query, end_s).await {
            Ok(series) => {
                for (labels, n) in series {
                    let who = labels
                        .get(if prefix == "maker" { "maker" } else { "bot" })
                        .cloned()
                        .unwrap_or_default();
                    trades
                        .entry(format!("{prefix}/{who}"))
                        .or_insert_with(|| json!({}))
                        [labels.get(label).cloned().unwrap_or_default()] = json!(n.round());
                }
            }
            Err(error) => warnings.push(format!("{prefix} trade counts unavailable: {error:#}")),
        }
    }
    let flow_query = format!("sum by (status, size) (increase(flow_swaps_total[{range}]))");
    match prometheus(&client, url, &flow_query, end_s).await {
        Ok(series) => {
            let mut flow = json!({});
            for (labels, n) in series {
                flow[format!(
                    "{}_{}",
                    labels.get("status").cloned().unwrap_or_default(),
                    labels.get("size").cloned().unwrap_or_default()
                )] = json!(n.round());
            }
            trades.insert("flow (all takers)".into(), flow);
        }
        Err(error) => warnings.push(format!("flow trade counts unavailable: {error:#}")),
    }
    // Refills mint tokens into taker wallets, which reads as taker profit.
    let refills = format!("sum(increase(flow_refills_total[{range}]))");
    match prometheus(&client, url, &refills, end_s).await {
        Ok(series) => {
            let n: f64 = series.iter().map(|(_, n)| n).sum();
            if n >= 0.5 {
                warnings.push(format!(
                    "{} taker refills inside the window: their mints count as taker PnL (run with MTM_SCENARIO__FLOW__REFILL_BELOW_RATIO=0)",
                    n.round()
                ));
            }
        }
        Err(error) => warnings.push(format!("refill count unavailable: {error:#}")),
    }
    trades
}

/// `compute_every`: sample compute from every n-th slot's full block
/// (0: no compute in the venue breakdown).
pub async fn report(
    rpc: &JsonRpc,
    start: &Snapshot,
    end: &Snapshot,
    prometheus_url: Option<&str>,
    venues: &VenuePrograms,
    compute_every: u64,
) -> Result<Value> {
    let window_s = (end.time_us - start.time_us) as f64 / 1e6;
    let end_s = end.time_us as f64 / 1e6;
    let range = format!("{}s", window_s.round().max(1.0) as u64);
    let mut warnings: Vec<String> = Vec::new();

    let trades = match prometheus_url {
        Some(url) => trade_counts(url, &range, end_s, &mut warnings).await,
        None => BTreeMap::new(),
    };

    let starts: HashMap<&str, &Holdings> = start
        .participants
        .iter()
        .map(|p| (p.role.as_str(), p))
        .collect();
    let pairs: Vec<(&Holdings, &Holdings)> = end
        .participants
        .iter()
        .filter_map(|e| Some((*starts.get(e.role.as_str())?, e)))
        .collect();
    let unmatched = start.participants.len() + end.participants.len() - 2 * pairs.len();
    if unmatched > 0 {
        warnings.push(format!(
            "{unmatched} participants appear in only one snapshot and are left out"
        ));
    }
    let scan = block_transactions(rpc, start.slot, end.slot, venues).await?;
    let compute = match compute_every {
        0 => None,
        every => Some((
            every,
            sampled_compute(rpc, start.slot, end.slot, every, venues).await?,
        )),
    };
    let by_venue = venue_breakdown(
        &scan.transactions,
        compute
            .as_ref()
            .map(|(every, (slots, units))| (*every, *slots, units.as_slice())),
        venues,
    );
    // Per signer wallet: (landed, failed); per slot: landed count.
    let mut signed: HashMap<&str, (u64, u64)> = HashMap::new();
    let mut per_slot: BTreeMap<u64, u64> = BTreeMap::new();
    let (mut network_ok, mut network_failed) = (0_u64, 0_u64);
    for tx in &scan.transactions {
        if tx.ok {
            network_ok += 1;
            *per_slot.entry(tx.slot).or_default() += 1;
        } else {
            network_failed += 1;
        }
        for signer in &tx.signers {
            let entry = signed.entry(signer.as_str()).or_default();
            if tx.ok {
                entry.0 += 1;
            } else {
                entry.1 += 1;
            }
        }
    }
    let slot_seconds = window_s / (end.slot.saturating_sub(start.slot)).max(1) as f64;
    let peak = peak_tps(&per_slot, start.slot, end.slot, slot_seconds);
    let mut rows = Vec::new();
    let mut by_kind: BTreeMap<String, Vec<KindRow>> = BTreeMap::new();
    let mut totals = (Decimal::ZERO, Decimal::ZERO, 0_u64, 0_u64);
    for (start_holdings, end_holdings) in &pairs {
        let role = &end_holdings.role;
        let end_value = value(&end_holdings.tokens, &end.prices).with_context(|| role.clone())?;
        // Trading PnL values both inventories at end prices, removing the
        // market move; mark-to-market PnL includes it.
        let trading =
            end_value - value(&start_holdings.tokens, &end.prices).with_context(|| role.clone())?;
        let start_value =
            value(&start_holdings.tokens, &start.prices).with_context(|| role.clone())?;
        let mtm = end_value - start_value;
        let deltas: BTreeMap<String, String> = end_holdings
            .tokens
            .iter()
            .map(|(name, amount)| {
                let before = start_holdings.tokens.get(name).copied().unwrap_or_default();
                (name.clone(), (*amount - before).round_dp(6).to_string())
            })
            .collect();
        let (ok_n, failed_n) = signed
            .get(end_holdings.wallet.as_str())
            .copied()
            .unwrap_or_default();
        totals.0 += trading;
        totals.1 += mtm;
        totals.2 += ok_n;
        totals.3 += failed_n;
        by_kind
            .entry(end_holdings.kind.clone())
            .or_default()
            .push((trading, mtm, ok_n, failed_n));
        rows.push(json!({
            "role": role,
            "kind": end_holdings.kind,
            "startValueUsd": start_value.round_dp(2).to_string(),
            "endValueUsd": end_value.round_dp(2).to_string(),
            "tradingPnlUsd": trading.round_dp(2).to_string(),
            "markToMarketPnlUsd": mtm.round_dp(2).to_string(),
            "solSpent": (start_holdings.sol - end_holdings.sol).round_dp(6).to_string(),
            "tokenDeltas": deltas,
            "transactionsLanded": ok_n,
            "transactionsFailed": failed_n,
            "trades": trades.get(role).cloned().unwrap_or(Value::Null),
        }));
    }
    let kinds: BTreeMap<String, Value> = by_kind
        .into_iter()
        .map(|(kind, mut rows)| (kind, kind_summary(&mut rows)))
        .collect();
    Ok(json!({
        "windowSeconds": window_s.round(),
        "slots": { "start": start.slot, "end": end.slot },
        "prices": { "start": start.prices, "end": end.prices },
        "byKind": kinds,
        "participants": rows,
        "flowTrades": trades.get("flow (all takers)").cloned().unwrap_or(Value::Null),
        "network": {
            "transactionsLanded": network_ok,
            "transactionsFailed": network_failed,
            "landedTps": (network_ok as f64 / window_s.max(1.0) * 100.0).round() / 100.0,
            "peakLandedTps": (peak * 10.0).round() / 10.0,
            "slotSeconds": (slot_seconds * 1000.0).round() / 1000.0,
            "voteTransactionsExcluded": scan.votes,
            "slotsSkipped": scan.skipped_slots,
            "note": "every non-vote transaction in the window's blocks (failed = landed with an error; preflight rejections never land); on a shared cluster this counts everyone's traffic",
        },
        "byVenue": by_venue,
        "totals": {
            "tradingPnlUsd": totals.0.round_dp(2).to_string(),
            "markToMarketPnlUsd": totals.1.round_dp(2).to_string(),
            "transactionsLanded": totals.2,
            "transactionsFailed": totals.3,
            "note": "transactions here are counted per signing wallet (a tx two participants sign counts for each; see `network` for unique)",
        },
        "warnings": warnings,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).expect("decimal")
    }

    #[test]
    fn value_errors_on_a_missing_price_but_ignores_zero_holdings() {
        let prices = BTreeMap::from([("USDC".to_owned(), d("1")), ("JUP".to_owned(), d("0.5"))]);
        let held = BTreeMap::from([("USDC".to_owned(), d("10")), ("JUP".to_owned(), d("4"))]);
        assert_eq!(value(&held, &prices).expect("priced"), d("12"));
        let unpriced = BTreeMap::from([("RAY".to_owned(), d("1"))]);
        assert!(value(&unpriced, &prices).is_err());
        let empty = BTreeMap::from([("RAY".to_owned(), Decimal::ZERO)]);
        assert_eq!(value(&empty, &prices).expect("nothing held"), Decimal::ZERO);
    }

    #[test]
    fn median_averages_the_middle_pair() {
        assert_eq!(median(&[]), Decimal::ZERO);
        assert_eq!(median(&[d("1"), d("2"), d("9")]), d("2"));
        assert_eq!(median(&[d("1"), d("2"), d("4"), d("9")]), d("3"));
    }

    #[test]
    fn kind_summary_sorts_and_counts() {
        let mut rows = vec![
            (d("5"), d("6"), 3, 1),
            (d("-2"), d("-1"), 1, 0),
            (d("0"), d("1"), 2, 2),
        ];
        let summary = kind_summary(&mut rows);
        assert_eq!(summary["participants"], 3);
        assert_eq!(summary["tradingPnlUsd"], "3");
        assert_eq!(summary["medianTradingPnlUsd"], "0");
        assert_eq!(summary["minTradingPnlUsd"], "-2");
        assert_eq!(summary["maxTradingPnlUsd"], "5");
        assert_eq!(summary["profitable"], 1);
        assert_eq!(summary["transactionsLanded"], 6);
        assert_eq!(summary["transactionsFailed"], 3);
    }

    #[test]
    fn peak_tps_uses_the_busiest_window_or_the_whole_short_one() {
        // 100 slots of 0.4 s, 50 landed in slot 60 only
        let per_slot = BTreeMap::from([(60, 50)]);
        let peak = peak_tps(&per_slot, 0, 100, 0.4);
        assert!((peak - 50.0 / (25.0 * 0.4)).abs() < 1e-9);
        // a 10-slot window: no longer 0
        let short = peak_tps(&BTreeMap::from([(5, 20)]), 0, 10, 0.4);
        assert!((short - 20.0 / (10.0 * 0.4)).abs() < 1e-9);
        assert!(peak_tps(&per_slot, 10, 10, 0.4).abs() < 1e-12);
    }

    fn two_venues() -> VenuePrograms {
        VenuePrograms(vec![
            ("raydium-clmm", Pubkey::new_from_array([1; 32])),
            ("manifest", Pubkey::new_from_array([2; 32])),
        ])
    }

    #[test]
    fn blocks_drop_votes_and_keep_signers_venues_and_errors() {
        let venues = two_venues();
        let (raydium, manifest) = (venues.0[0].1.to_string(), venues.0[1].1.to_string());
        let block = json!({ "transactions": [
            { "transaction": { "accountKeys": [
                { "pubkey": "validator", "signer": true },
                { "pubkey": VOTE_PROGRAM, "signer": false } ] }, "meta": { "err": null } },
            { "transaction": { "accountKeys": [
                { "pubkey": "taker", "signer": true },
                { "pubkey": raydium, "signer": false },
                { "pubkey": manifest, "signer": false } ] }, "meta": { "err": null } },
            { "transaction": { "accountKeys": [
                { "pubkey": "bot", "signer": true } ] },
              "meta": { "err": { "InstructionError": [2, { "Custom": 6018 }] } } },
        ]});
        let (txs, votes) = parse_block(&block, 7, &venues);
        assert_eq!(votes, 1);
        assert_eq!(
            txs,
            vec![
                BlockTx {
                    signers: vec!["taker".to_owned()],
                    ok: true,
                    slot: 7,
                    venues: 0b11,
                    error: None
                },
                BlockTx {
                    signers: vec!["bot".to_owned()],
                    ok: false,
                    slot: 7,
                    venues: 0,
                    error: Some("Custom(6018)".to_owned())
                },
            ]
        );
        assert_eq!(parse_block(&json!({}), 7, &venues), (vec![], 0));
    }

    #[test]
    fn error_labels_are_short() {
        assert_eq!(
            error_label(&json!({ "InstructionError": [4, { "Custom": 10 }] })),
            "Custom(10)"
        );
        assert_eq!(
            error_label(&json!({ "InstructionError": [0, "InvalidAccountData"] })),
            "InvalidAccountData"
        );
        assert_eq!(
            error_label(&json!({ "InstructionError": [1, { "BorshIoError": "x" }] })),
            "BorshIoError"
        );
        assert_eq!(
            error_label(&json!("BlockhashNotFound")),
            "BlockhashNotFound"
        );
        assert_eq!(
            error_label(&json!({ "InsufficientFundsForRent": { "account_index": 0 } })),
            "InsufficientFundsForRent"
        );
    }

    #[test]
    fn full_blocks_yield_compute_per_non_vote_transaction() {
        let venues = two_venues();
        let manifest = venues.0[1].1.to_string();
        let block = json!({ "transactions": [
            { "transaction": { "message": { "accountKeys": ["v", VOTE_PROGRAM] } },
              "meta": { "computeUnitsConsumed": 2100 } },
            { "transaction": { "message": { "accountKeys": ["t", manifest] } },
              "meta": { "computeUnitsConsumed": 9000 } },
            { "transaction": { "message": { "accountKeys": ["t"] } },
              "meta": { "computeUnitsConsumed": 150 } },
        ]});
        assert_eq!(block_compute(&block, &venues), vec![(0b10, 9000), (0, 150)]);
    }

    #[test]
    fn venue_breakdown_counts_rates_errors_and_compute_shares() {
        let venues = two_venues();
        let tx = |venues, ok: bool, error: Option<&str>| BlockTx {
            signers: vec![],
            ok,
            slot: 1,
            venues,
            error: error.map(str::to_owned),
        };
        let txs = [
            tx(0b01, true, None),
            tx(0b01, false, Some("Custom(6018)")),
            tx(0b11, true, None),
            tx(0b10, false, Some("Custom(10)")),
            tx(0, true, None),
        ];
        let units = [(0b01, 50_000), (0b11, 100_000), (0b10, 10_000), (0, 40_000)];
        let out = venue_breakdown(&txs, Some((10, 4, &units)), &venues);
        let raydium = &out["raydium-clmm"];
        assert_eq!(raydium["transactionsLanded"], 2);
        assert_eq!(raydium["transactionsFailed"], 1);
        assert_eq!(raydium["failureRatePct"], 33.33);
        assert_eq!(raydium["errors"]["Custom(6018)"], 1);
        assert_eq!(raydium["compute"]["meanUnits"], 75_000);
        assert_eq!(raydium["compute"]["sharePct"], 75.0);
        assert_eq!(out["manifest"]["compute"]["sharePct"], 55.0);
        assert_eq!(out["other"]["transactionsLanded"], 1);
        assert_eq!(out["other"]["compute"]["sharePct"], 20.0);
        assert_eq!(out["computeSample"]["meanUnitsPerSlot"], 50_000);
        assert!(
            venue_breakdown(&txs, None, &venues)["manifest"]
                .get("compute")
                .is_none()
        );
    }

    #[test]
    fn p95_is_nearest_rank() {
        assert_eq!(p95(&mut []), 0);
        assert_eq!(p95(&mut [5]), 5);
        let mut hundred: Vec<u64> = (1..=100).rev().collect();
        assert_eq!(p95(&mut hundred), 95);
    }

    #[test]
    fn manifest_holdings_add_locked_orders_and_refuse_overflow() {
        // a bid for 3 base atoms at 1.5 quote/base locks ceil(4.5) = 5
        let price = D18 * 3 / 2;
        let (base, quote) = manifest_holdings(
            10,
            100,
            [4_u64, 6].into_iter(),
            [(price, 3_u64)].into_iter(),
        )
        .expect("fits");
        assert_eq!((base, quote), (20, 105));
        assert!(manifest_holdings(u64::MAX, 0, [1_u64].into_iter(), std::iter::empty()).is_err());
        assert!(
            manifest_holdings(0, 0, std::iter::empty(), [(u128::MAX, 2_u64)].into_iter()).is_err()
        );
    }
}
