//! Participant accounting: snapshots of everything each scenario wallet owns
//! (wallet tokens and SOL, Manifest seat balances and resting orders, LP
//! positions) valued at oracle prices, and a report comparing two snapshots
//! with on-chain transaction counts and trade counts from Prometheus.

use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use mtm_chain::ChainClient;
use mtm_scenario::funding::token_balance;
use mtm_scenario::pricing::PriceBook;
use mtm_scenario::venue::to_ui;
use mtm_scenario::{Scenario, Strategy, Venue};
use oracle_client::OracleClient;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
use solana_signature::Signature;
use solana_signer::Signer;
use solana_signer::null_signer::NullSigner;

const D18: u128 = 1_000_000_000_000_000_000;

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

pub async fn snapshot(
    chain: &ChainClient,
    scenario: &Scenario,
    state_dir: &Path,
) -> Result<Snapshot> {
    let oracle = OracleClient::new(&scenario.config.oracle_url);
    let book = PriceBook::fetch(&oracle, scenario.config.max_price_age_ms).await?;
    let mut prices = BTreeMap::new();
    for token in scenario.tokens.values() {
        prices.insert(token.name.clone(), book.usd(token)?);
    }
    let slot = chain.rpc().get_slot().await?;
    let mut holdings = Vec::new();
    for (role, kind, wallet) in participants(scenario) {
        let mut breakdown: BTreeMap<String, BTreeMap<String, Decimal>> = BTreeMap::new();
        let mut wallet_tokens = BTreeMap::new();
        for token in scenario.tokens.values() {
            wallet_tokens.insert(
                token.name.clone(),
                to_ui(token, token_balance(chain, &wallet, token).await?),
            );
        }
        breakdown.insert("wallet".into(), wallet_tokens);
        if let Some(name) = role.strip_prefix("maker/") {
            let maker = scenario
                .config
                .makers
                .iter()
                .find(|m| m.name == name)
                .context("maker vanished")?;
            if let Some(venue_tokens) =
                venue_holdings(chain, scenario, maker, wallet, state_dir).await?
            {
                breakdown.insert(maker.venue.as_str().to_owned(), venue_tokens);
            }
        }
        let mut tokens: BTreeMap<String, Decimal> = BTreeMap::new();
        for place in breakdown.values() {
            for (name, amount) in place {
                *tokens.entry(name.clone()).or_default() += *amount;
            }
        }
        let lamports = chain.rpc().get_balance(&wallet).await?;
        holdings.push(Holdings {
            role,
            kind,
            wallet: wallet.to_string(),
            tokens,
            sol: Decimal::from(lamports) / Decimal::from(1_000_000_000_u64),
            breakdown,
        });
    }
    Ok(Snapshot {
        time_us: mtm_common::time::now_us(),
        slot,
        prices,
        participants: holdings,
    })
}

/// Tokens a maker holds inside its venue (Manifest seat + resting orders,
/// or its open LP position).
async fn venue_holdings(
    chain: &ChainClient,
    scenario: &Scenario,
    maker: &mtm_scenario::MakerConfig,
    wallet: Pubkey,
    state_dir: &Path,
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
            #[derive(Deserialize)]
            struct Position {
                mint: String,
            }
            let Ok(bytes) = std::fs::read(state_dir.join(format!("{}.json", maker.name))) else {
                return Ok(None);
            };
            let position: Position = serde_json::from_slice(&bytes)?;
            let mint = Pubkey::from_str(&position.mint)?;
            let reader: Arc<dyn Signer + Send + Sync> = Arc::new(NullSigner::new(&wallet));
            let program = scenario.programs.of(venue);
            let confirmed = CommitmentConfig::confirmed();
            let (mint_a, mint_b, amount_a, amount_b) = match venue {
                Venue::OrcaWhirlpool => {
                    let value = orca_whirlpool_client::OrcaWhirlpoolClient::new(
                        chain.clone(),
                        program,
                        reader,
                        0,
                        200_000,
                        0,
                        confirmed,
                    )?
                    .position_value(mint)
                    .await?;
                    (value.mint_a, value.mint_b, value.amount_a, value.amount_b)
                }
                Venue::RaydiumClmm => {
                    let value = raydium_clmm_client::RaydiumClmmClient::new(
                        chain.clone(),
                        program,
                        reader,
                        0,
                        200_000,
                        0,
                        confirmed,
                    )?
                    .position_value(mint)
                    .await?;
                    (value.mint_a, value.mint_b, value.amount_a, value.amount_b)
                }
                Venue::Manifest => return Ok(None),
            };
            for (mint, amount) in [(mint_a, amount_a), (mint_b, amount_b)] {
                let token = scenario
                    .token_by_mint(&mint)
                    .context("position mint is not a scenario token")?;
                out.insert(token.name.clone(), to_ui(token, amount));
            }
        }
        _ => return Ok(None),
    }
    Ok(Some(out))
}

fn value(tokens: &BTreeMap<String, Decimal>, prices: &BTreeMap<String, Decimal>) -> Decimal {
    tokens
        .iter()
        .map(|(name, amount)| *amount * prices.get(name).copied().unwrap_or_default())
        .sum()
}

/// Successful and failed transactions signed by or mentioning `wallet` in
/// `(after_slot, until_slot]`.
async fn transactions(
    chain: &ChainClient,
    wallet: &Pubkey,
    after_slot: u64,
    until_slot: u64,
) -> Result<(u64, u64)> {
    let (mut ok, mut failed) = (0, 0);
    let mut before: Option<Signature> = None;
    loop {
        let page = chain
            .rpc()
            .get_signatures_for_address_with_config(
                wallet,
                GetConfirmedSignaturesForAddress2Config {
                    before,
                    until: None,
                    limit: Some(1_000),
                    commitment: Some(CommitmentConfig::confirmed()),
                },
            )
            .await?;
        let Some(last) = page.last() else { break };
        let oldest = last.slot;
        before = Some(Signature::from_str(&last.signature)?);
        for status in &page {
            if status.slot > after_slot && status.slot <= until_slot {
                if status.err.is_none() {
                    ok += 1;
                } else {
                    failed += 1;
                }
            }
        }
        if oldest <= after_slot || page.len() < 1_000 {
            break;
        }
    }
    Ok((ok, failed))
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
    chain: &ChainClient,
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

    let mut rows = Vec::new();
    let mut totals = (Decimal::ZERO, Decimal::ZERO, 0_u64, 0_u64);
    for end_holdings in &end.participants {
        let Some(start_holdings) = start
            .participants
            .iter()
            .find(|p| p.role == end_holdings.role)
        else {
            continue;
        };
        let wallet = Pubkey::from_str(&end_holdings.wallet)?;
        let (ok, failed) = transactions(chain, &wallet, start.slot, end.slot).await?;
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
        totals.0 += trading;
        totals.1 += mtm;
        totals.2 += ok;
        totals.3 += failed;
        rows.push(json!({
            "role": end_holdings.role,
            "kind": end_holdings.kind,
            "startValueUsd": value(&start_holdings.tokens, &start.prices).round_dp(2).to_string(),
            "endValueUsd": value(&end_holdings.tokens, &end.prices).round_dp(2).to_string(),
            "tradingPnlUsd": trading.round_dp(2).to_string(),
            "markToMarketPnlUsd": mtm.round_dp(2).to_string(),
            "solSpent": (start_holdings.sol - end_holdings.sol).round_dp(6).to_string(),
            "tokenDeltas": deltas,
            "transactionsLanded": ok,
            "transactionsFailed": failed,
            "trades": trades.get(&end_holdings.role).cloned().unwrap_or(Value::Null),
        }));
    }
    Ok(json!({
        "windowSeconds": window_s.round(),
        "slots": { "start": start.slot, "end": end.slot },
        "prices": { "start": start.prices, "end": end.prices },
        "participants": rows,
        "flowTrades": trades.get("flow (all takers)").cloned().unwrap_or(Value::Null),
        "totals": {
            "tradingPnlUsd": totals.0.round_dp(2).to_string(),
            "markToMarketPnlUsd": totals.1.round_dp(2).to_string(),
            "transactionsLanded": totals.2,
            "transactionsFailed": totals.3,
            "note": "transactions are counted per wallet (a tx touching two participants counts for each); minted refills inside the window would show as PnL, so run with taker refills disabled",
        },
    }))
}
