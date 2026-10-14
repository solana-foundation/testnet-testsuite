//! Participant accounting: snapshots of everything each scenario wallet owns
//! (wallet tokens and SOL, Manifest seat balances and resting orders, LP
//! positions) valued at oracle prices, and a report comparing two snapshots
//! with on-chain transaction counts and trade counts from Prometheus.

use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
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

const D18: u128 = 1_000_000_000_000_000_000;
/// A maker's wallet token atoms (in `scenario.tokens` order) and its venue
/// holdings, read consistently.
type MakerRead = (Vec<u64>, Option<(String, BTreeMap<String, Decimal>)>);
/// Concurrent RPC reads per snapshot / report.
const CONCURRENCY: usize = 32;

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
    /// Where the tokens sit, for inspection.
    pub breakdown: BTreeMap<String, BTreeMap<String, Decimal>>,
}

fn participants(scenario: &Scenario) -> Vec<(String, String, Pubkey)> {
    let mut out: Vec<(String, String, Pubkey)> = scenario
        .config
        .makers
        .iter()
        .map(|maker| {
            let kind = match maker.strategy {
                Strategy::ClobLadder { .. } => "market maker (clob)",
                Strategy::ClmmRange { .. } => "liquidity provider",
                Strategy::PoolArb { .. } => "oracle arbitrageur",
            };
            (
                format!("maker/{}", maker.name),
                kind.to_owned(),
                scenario.maker_wallet(&maker.name).pubkey(),
            )
        })
        .collect();
    if let Some(arb) = &scenario.config.arb {
        out.extend(arb.bots.iter().map(|bot| {
            (
                format!("arb/{}", bot.name),
                "cross-venue arbitrage".to_owned(),
                scenario.arb_wallet(&bot.name).pubkey(),
            )
        }));
    }
    if let Some(flow) = &scenario.config.flow {
        out.extend((0..flow.takers).map(|i| {
            (
                format!("taker/{i}"),
                "uninformed taker".to_owned(),
                scenario.taker_wallet(i).pubkey(),
            )
        }));
    }
    out
}

pub async fn snapshot(chain: &ChainClient, scenario: &Scenario) -> Result<Snapshot> {
    let oracle = OracleClient::new(&scenario.config.oracle_url);
    let book = PriceBook::fetch(&oracle, scenario.config.max_price_age_ms).await?;
    let mut prices = BTreeMap::new();
    for token in scenario.tokens.values() {
        prices.insert(token.name.clone(), book.usd(token)?);
    }
    let people = participants(scenario);
    let tokens: Vec<&Token> = scenario.tokens.values().collect();
    // Every wallet balance in a few batched reads, so the snapshot is close
    // to a single point in time even with thousands of participants.
    let pairs: Vec<(Pubkey, &Token)> = people
        .iter()
        .flat_map(|(_, _, wallet)| tokens.iter().map(move |token| (*wallet, *token)))
        .collect();
    let wallets: Vec<Pubkey> = people.iter().map(|(_, _, wallet)| *wallet).collect();
    let (token_atoms, lamports) = balances(chain, &pairs, &wallets).await?;
    // Venue holdings (seats, orders, positions) only exist for makers. A
    // maker moving funds between wallet and venue mid-read (a re-centre)
    // would be counted twice or not at all, so each maker is read
    // optimistically: wallet, venue, wallet again, retried until the wallet
    // did not change around the venue read.
    let makers: Vec<Option<MakerRead>> =
        futures::stream::iter(people.iter().map(|(role, _, wallet)| {
            let tokens = &tokens;
            async move {
                let Some(name) = role.strip_prefix("maker/") else {
                    return Ok(None);
                };
                let maker = scenario
                    .config
                    .makers
                    .iter()
                    .find(|m| m.name == name)
                    .context("maker vanished")?;
                let pairs: Vec<(Pubkey, &Token)> =
                    tokens.iter().map(|token| (*wallet, *token)).collect();
                let mut before = balances(chain, &pairs, &[]).await?.0;
                for _ in 0..5 {
                    let venue = venue_holdings(chain, scenario, maker, *wallet)
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
        .map(|(index, ((role, kind, wallet), maker))| {
            let (atoms, venue) = maker.unwrap_or_else(|| {
                (
                    token_atoms[index * tokens.len()..(index + 1) * tokens.len()].to_vec(),
                    None,
                )
            });
            let mut breakdown: BTreeMap<String, BTreeMap<String, Decimal>> = BTreeMap::new();
            breakdown.insert(
                "wallet".into(),
                tokens
                    .iter()
                    .zip(&atoms)
                    .map(|(token, atoms)| (token.name.clone(), to_ui(token, *atoms)))
                    .collect(),
            );
            breakdown.extend(venue);
            let mut totals: BTreeMap<String, Decimal> = BTreeMap::new();
            for place in breakdown.values() {
                for (name, amount) in place {
                    *totals.entry(name.clone()).or_default() += *amount;
                }
            }
            Holdings {
                role,
                kind,
                wallet: wallet.to_string(),
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
/// or its open LP position).
async fn venue_holdings(
    chain: &ChainClient,
    scenario: &Scenario,
    maker: &mtm_scenario::MakerConfig,
    wallet: Pubkey,
) -> Result<Option<BTreeMap<String, Decimal>>> {
    let base = scenario.token(&maker.base)?;
    let quote = scenario.token(&maker.quote)?;
    let mut out = BTreeMap::new();
    match (&maker.strategy, maker.venue) {
        (Strategy::ClobLadder { .. }, Venue::Manifest) => {
            let Some(market) = scenario
                .pool_address(chain, maker.venue, &maker.base, &maker.quote)
                .await?
            else {
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
            let (mut base_atoms, mut quote_atoms) = (
                u128::from(seat.base_withdrawable),
                u128::from(seat.quote_withdrawable),
            );
            // Resting asks lock base; resting bids lock quote at their price.
            for ask in snapshot
                .book
                .asks
                .iter()
                .filter(|o| o.trader_index == seat.index)
            {
                base_atoms += u128::from(ask.base_atoms);
            }
            for bid in snapshot
                .book
                .bids
                .iter()
                .filter(|o| o.trader_index == seat.index)
            {
                quote_atoms += (bid.price_raw * u128::from(bid.base_atoms)).div_ceil(D18);
            }
            out.insert(
                base.name.clone(),
                to_ui(base, u64::try_from(base_atoms).unwrap_or(u64::MAX)),
            );
            out.insert(
                quote.name.clone(),
                to_ui(quote, u64::try_from(quote_atoms).unwrap_or(u64::MAX)),
            );
        }
        (Strategy::ClmmRange { .. }, venue) => {
            // Positions are found from the position NFTs the wallet holds
            // (not the mm's state file, which is briefly absent while a
            // position is re-centred).
            let nfts = position_nfts(chain, &wallet).await?;
            let reader: Arc<dyn Signer + Send + Sync> = Arc::new(NullSigner::new(&wallet));
            let program = scenario.programs.of(venue);
            let confirmed = CommitmentConfig::confirmed();
            for mint in nfts {
                let value = match venue {
                    Venue::OrcaWhirlpool => orca_whirlpool_client::OrcaWhirlpoolClient::new(
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
                    .map(|v| (v.mint_a, v.mint_b, v.amount_a, v.amount_b))
                    .map_err(anyhow::Error::from),
                    Venue::RaydiumClmm => raydium_clmm_client::RaydiumClmmClient::new(
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
                    .map(|v| (v.mint_a, v.mint_b, v.amount_a, v.amount_b))
                    .map_err(anyhow::Error::from),
                    Venue::Manifest => return Ok(None),
                };
                // Not a position of this venue (or already closed).
                let (mint_a, mint_b, amount_a, amount_b) = match value {
                    Ok(value) => value,
                    Err(error) => {
                        tracing::debug!(maker = %maker.name, %mint, error = %format!("{error:#}"), "NFT is not a valued position");
                        continue;
                    }
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

/// Mints of the NFTs (amount 1, 0 decimals) `owner` holds under either
/// token program: candidate CLMM position mints. Read at the client's
/// commitment like every other snapshot read (the RPC default, finalized,
/// lags confirmed by ~32 slots on a real validator: recently opened
/// positions would be missing and recently closed ones still listed).
async fn position_nfts(chain: &ChainClient, owner: &Pubkey) -> Result<Vec<Pubkey>> {
    let commitment = chain.rpc().commitment().commitment.to_string();
    let mut mints = Vec::new();
    for program in [
        "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
        "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
    ] {
        let response: Value = reqwest::Client::new()
            .post(chain.rpc().url())
            .json(&json!({
                "jsonrpc": "2.0", "id": 1, "method": "getTokenAccountsByOwner",
                "params": [owner.to_string(), { "programId": program }, { "encoding": "jsonParsed", "commitment": commitment }],
            }))
            .send()
            .await?
            .json()
            .await?;
        let empty = vec![];
        for account in response["result"]["value"].as_array().unwrap_or(&empty) {
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

fn value(tokens: &BTreeMap<String, Decimal>, prices: &BTreeMap<String, Decimal>) -> Decimal {
    tokens
        .iter()
        .map(|(name, amount)| *amount * prices.get(name).copied().unwrap_or_default())
        .sum()
}

/// Every transaction in `(after_slot, until_slot]`, read block by block:
/// (signer wallets, succeeded, slot). Block scans are linear in the
/// window, unlike per-wallet signature lookups, which a local validator
/// answers by scanning its whole history.
/// Consensus votes: a real validator (solana-test-validator) lands one per
/// slot; Surfpool has none. Excluded so the two compare.
const VOTE_PROGRAM: &str = "Vote111111111111111111111111111111111111111";

/// The window's non-vote transactions as (signers, ok, slot), plus the
/// number of vote transactions skipped.
async fn block_transactions(
    rpc_url: &str,
    after_slot: u64,
    until_slot: u64,
) -> Result<(Vec<(Vec<String>, bool, u64)>, u64)> {
    let client = reqwest::Client::new();
    let blocks = futures::stream::iter((after_slot + 1..=until_slot).map(|slot| {
        let client = &client;
        async move {
            let response: Value = client
                .post(rpc_url)
                .json(&json!({
                    "jsonrpc": "2.0", "id": 1, "method": "getBlock",
                    "params": [slot, {
                        "encoding": "json", "transactionDetails": "accounts", "rewards": false,
                        "maxSupportedTransactionVersion": 1, "commitment": "confirmed",
                    }],
                }))
                .send()
                .await?
                .json()
                .await?;
            // Skipped slots answer with an error or a null block.
            let block = &response["result"];
            let empty = vec![];
            Ok::<_, anyhow::Error>(
                block["transactions"]
                    .as_array()
                    .unwrap_or(&empty)
                    .iter()
                    .map(|tx| {
                        let keys = tx["transaction"]["accountKeys"]
                            .as_array()
                            .unwrap_or(&empty);
                        let vote = keys
                            .iter()
                            .any(|key| key["pubkey"].as_str() == Some(VOTE_PROGRAM));
                        let signers = keys
                            .iter()
                            .filter(|key| key["signer"].as_bool() == Some(true))
                            .filter_map(|key| key["pubkey"].as_str().map(str::to_owned))
                            .collect();
                        (vote, (signers, tx["meta"]["err"].is_null(), slot))
                    })
                    .collect::<Vec<_>>(),
            )
        }
    }))
    .buffered(CONCURRENCY)
    .collect::<Vec<_>>()
    .await;
    let (mut out, mut votes) = (Vec::new(), 0);
    for block in blocks {
        for (vote, tx) in block? {
            if vote {
                votes += 1;
            } else {
                out.push(tx);
            }
        }
    }
    Ok((out, votes))
}

async fn prometheus(
    url: &str,
    query: &str,
    at_s: f64,
) -> Result<Vec<(BTreeMap<String, String>, f64)>> {
    let response: Value = reqwest::Client::new()
        .get(format!("{url}/api/v1/query"))
        .query(&[("query", query), ("time", &at_s.to_string())])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let empty = vec![];
    Ok(response["data"]["result"]
        .as_array()
        .unwrap_or(&empty)
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
                .unwrap_or(0.0);
            (labels, value)
        })
        .collect())
}

pub async fn report(
    rpc_url: &str,
    start: &Snapshot,
    end: &Snapshot,
    prometheus_url: Option<&str>,
) -> Result<Value> {
    let window_s = (end.time_us - start.time_us) as f64 / 1e6;
    let end_s = end.time_us as f64 / 1e6;
    let range = format!("{}s", window_s.round().max(1.0) as u64);

    // Trades per participant from service metrics (counted over the window).
    let mut trades: BTreeMap<String, Value> = BTreeMap::new();
    if let Some(url) = prometheus_url {
        for (labels, n) in prometheus(
            url,
            &format!("sum by (maker, action) (increase(mm_actions_total[{range}]))"),
            end_s,
        )
        .await?
        {
            let role = format!("maker/{}", labels.get("maker").cloned().unwrap_or_default());
            trades.entry(role).or_insert_with(|| json!({}))
                [labels.get("action").cloned().unwrap_or_default()] = json!(n.round());
        }
        for (labels, n) in prometheus(
            url,
            &format!("sum by (bot, status) (increase(arb_trades_total[{range}]))"),
            end_s,
        )
        .await?
        {
            let role = format!("arb/{}", labels.get("bot").cloned().unwrap_or_default());
            trades.entry(role).or_insert_with(|| json!({}))
                [labels.get("status").cloned().unwrap_or_default()] = json!(n.round());
        }
        let mut flow = json!({});
        for (labels, n) in prometheus(
            url,
            &format!("sum by (status, size) (increase(flow_swaps_total[{range}]))"),
            end_s,
        )
        .await?
        {
            flow[format!(
                "{}_{}",
                labels.get("status").cloned().unwrap_or_default(),
                labels.get("size").cloned().unwrap_or_default()
            )] = json!(n.round());
        }
        trades.insert("flow (all takers)".into(), flow);
    }

    let pairs: Vec<(&Holdings, &Holdings)> = end
        .participants
        .iter()
        .filter_map(|e| Some((start.participants.iter().find(|p| p.role == e.role)?, e)))
        .collect();
    let (transactions, votes) = block_transactions(rpc_url, start.slot, end.slot).await?;
    // Per signer wallet: (landed, failed); per slot: landed count.
    let mut signed: HashMap<&str, (u64, u64)> = HashMap::new();
    let mut per_slot: BTreeMap<u64, u64> = BTreeMap::new();
    let (mut network_ok, mut network_failed) = (0_u64, 0_u64);
    for (signers, ok, slot) in &transactions {
        if *ok {
            network_ok += 1;
            *per_slot.entry(*slot).or_default() += 1;
        } else {
            network_failed += 1;
        }
        for signer in signers {
            let entry = signed.entry(signer.as_str()).or_default();
            if *ok {
                entry.0 += 1;
            } else {
                entry.1 += 1;
            }
        }
    }
    // Busiest ~10 s: 25 consecutive slots, at the window's mean slot time.
    const PEAK_SLOTS: u64 = 25;
    let slot_seconds = window_s / (end.slot.saturating_sub(start.slot)).max(1) as f64;
    let peak_tps = (start.slot + 1..=end.slot.saturating_sub(PEAK_SLOTS - 1))
        .map(|first| {
            (first..first + PEAK_SLOTS)
                .filter_map(|slot| per_slot.get(&slot))
                .sum::<u64>()
        })
        .max()
        .map_or(0.0, |landed| {
            landed as f64 / (PEAK_SLOTS as f64 * slot_seconds)
        });
    let mut rows = Vec::new();
    let mut by_kind: BTreeMap<String, Vec<(Decimal, Decimal, u64, u64)>> = BTreeMap::new();
    let mut totals = (Decimal::ZERO, Decimal::ZERO, 0_u64, 0_u64);
    for (start_holdings, end_holdings) in &pairs {
        // Trading PnL values both inventories at end prices, removing the
        // market move; mark-to-market PnL includes it.
        let trading =
            value(&end_holdings.tokens, &end.prices) - value(&start_holdings.tokens, &end.prices);
        let mtm =
            value(&end_holdings.tokens, &end.prices) - value(&start_holdings.tokens, &start.prices);
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
            "role": end_holdings.role,
            "kind": end_holdings.kind,
            "startValueUsd": value(&start_holdings.tokens, &start.prices).round_dp(2).to_string(),
            "endValueUsd": value(&end_holdings.tokens, &end.prices).round_dp(2).to_string(),
            "tradingPnlUsd": trading.round_dp(2).to_string(),
            "markToMarketPnlUsd": mtm.round_dp(2).to_string(),
            "solSpent": (start_holdings.sol - end_holdings.sol).round_dp(6).to_string(),
            "tokenDeltas": deltas,
            "transactionsLanded": ok_n,
            "transactionsFailed": failed_n,
            "trades": trades.get(&end_holdings.role).cloned().unwrap_or(Value::Null),
        }));
    }
    let kinds: BTreeMap<String, Value> = by_kind
        .into_iter()
        .map(|(kind, mut rows)| {
            rows.sort_by_key(|row| row.0);
            let n = rows.len();
            let sum: Decimal = rows.iter().map(|row| row.0).sum();
            let profitable = rows.iter().filter(|row| row.0 > Decimal::ZERO).count();
            (
                kind,
                json!({
                    "participants": n,
                    "tradingPnlUsd": sum.round_dp(2).to_string(),
                    "markToMarketPnlUsd": rows.iter().map(|row| row.1).sum::<Decimal>().round_dp(2).to_string(),
                    "meanTradingPnlUsd": (sum / Decimal::from(n.max(1))).round_dp(2).to_string(),
                    "medianTradingPnlUsd": rows[n / 2].0.round_dp(2).to_string(),
                    "minTradingPnlUsd": rows[0].0.round_dp(2).to_string(),
                    "maxTradingPnlUsd": rows[n - 1].0.round_dp(2).to_string(),
                    "profitable": profitable,
                    "transactionsLanded": rows.iter().map(|row| row.2).sum::<u64>(),
                    "transactionsFailed": rows.iter().map(|row| row.3).sum::<u64>(),
                }),
            )
        })
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
            "peakLandedTps": (peak_tps * 10.0).round() / 10.0,
            "slotSeconds": (slot_seconds * 1000.0).round() / 1000.0,
            "voteTransactionsExcluded": votes,
            "note": "every non-vote transaction in the window's blocks (failed = landed with an error; preflight rejections never land)",
        },
        "totals": {
            "tradingPnlUsd": totals.0.round_dp(2).to_string(),
            "markToMarketPnlUsd": totals.1.round_dp(2).to_string(),
            "transactionsLanded": totals.2,
            "transactionsFailed": totals.3,
            "note": "transactions here are counted per signing wallet (a tx two participants sign counts for each; see `network` for unique); minted refills inside the window would show as PnL, so run with taker refills disabled",
        },
    }))
}
