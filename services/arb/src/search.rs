//! Cycle search over pool snapshots and atomic execution.

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::future::join_all;
use metrics::counter;
use mtm_chain::ChainClient;
use mtm_scenario::funding::token_balance;
use mtm_scenario::pricing::PriceBook;
use mtm_scenario::venue::{to_atoms, to_ui};
use mtm_scenario::{ArbBotConfig, ArbConfig, Scenario, Token, Venue};
use mtm_venues::{LegQuote, PoolSnapshot};
use oracle_client::OracleClient;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_message::VersionedMessage;
use solana_message::v1::{self, TransactionConfig};
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
use tracing::{debug, info};

const ATA_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
/// Upper bound on compute for one cycle transaction.
const MAX_COMPUTE_UNIT_LIMIT: u32 = 1_400_000;
/// A reload slower than this keeps the cached snapshot (the pre-send
/// simulation still catches a stale one).
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(2);

struct Bot {
    config: ArbBotConfig,
    wallet: Keypair,
    start_tokens: Vec<Token>,
}

/// One hop of a cycle: which pool, in which direction.
#[derive(Clone, Copy)]
struct Hop {
    pool: usize,
    input: Pubkey,
    output: Pubkey,
}

struct Candidate {
    hops: Vec<Hop>,
    start: Token,
    amount_in: u64,
    amount_out: u64,
    profit_usd: Decimal,
}

pub struct Searcher<'s> {
    chain: ChainClient,
    scenario: &'s Scenario,
    oracle: OracleClient,
    pools: Vec<(Venue, Pubkey)>,
    /// Last good snapshot per pool, refreshed only for pools that changed.
    cache: Mutex<Vec<Option<PoolSnapshot>>>,
    bots: Vec<Bot>,
}

impl<'s> Searcher<'s> {
    pub fn new(
        chain: ChainClient,
        scenario: &'s Scenario,
        pools: Vec<(Venue, Pubkey)>,
        config: &ArbConfig,
    ) -> Result<Self> {
        let bots = config
            .bots
            .iter()
            .map(|bot| {
                Ok(Bot {
                    wallet: scenario.arb_wallet(&bot.name),
                    start_tokens: bot
                        .start_tokens
                        .iter()
                        .map(|name| scenario.token(name).cloned())
                        .collect::<Result<_>>()?,
                    config: bot.clone(),
                })
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            oracle: OracleClient::new(&scenario.config.oracle_url),
            chain,
            scenario,
            cache: Mutex::new(vec![None; pools.len()]),
            pools,
            bots,
        })
    }

    pub fn pool_count(&self) -> usize {
        self.pools.len()
    }

    /// `changed`: pools the websocket reported (`None` reloads every pool).
    pub async fn scan_and_execute(
        &self,
        trigger: &'static str,
        changed: Option<&HashSet<Pubkey>>,
    ) -> Result<()> {
        let cached = self.cache.lock().expect("cache lock").clone();
        let loads = self.pools.iter().zip(cached).map(|((venue, pool), cached)| async move {
            if cached.is_some() && changed.is_some_and(|changed| !changed.contains(pool)) {
                return cached;
            }
            let started = std::time::Instant::now();
            let load = PoolSnapshot::load(
                &self.chain,
                Scenario::venue_kind(*venue),
                self.scenario.programs.of(*venue),
                *pool,
            );
            // Without a cached snapshot there is nothing to fall back to: wait.
            let timeout = if cached.is_some() { SNAPSHOT_TIMEOUT } else { Duration::from_secs(60) };
            match tokio::time::timeout(timeout, load).await {
                Ok(Ok(snapshot)) => {
                    debug!(venue = venue.as_str(), %pool, elapsed_ms = started.elapsed().as_millis(), "pool snapshot");
                    Some(snapshot)
                }
                Ok(Err(error)) => {
                    debug!(venue = venue.as_str(), %pool, %error, "pool snapshot failed");
                    counter!("arb_snapshot_failures_total", "reason" => "error").increment(1);
                    cached
                }
                Err(_) => {
                    counter!("arb_snapshot_failures_total", "reason" => "timeout").increment(1);
                    cached
                }
            }
        });
        let snapshots: Vec<Option<PoolSnapshot>> = join_all(loads).await;
        self.cache
            .lock()
            .expect("cache lock")
            .clone_from(&snapshots);
        let prices = PriceBook::fetch(&self.oracle, self.scenario.config.max_price_age_ms).await?;
        // Bots race on the same snapshot, like independent searchers would:
        // usually one lands and the rest revert on their profit floor.
        let results = join_all(self.bots.iter().map(|bot| {
            let (snapshots, prices) = (&snapshots, &prices);
            async move {
                let Some(best) = self.best_cycle(bot, snapshots, prices).await? else {
                    return Ok(());
                };
                counter!("arb_opportunities_total", "bot" => bot.config.name.clone(), "legs" => best.hops.len().to_string(), "trigger" => trigger).increment(1);
                self.execute(bot, snapshots, prices, best).await
            }
        }))
        .await;
        results.into_iter().collect::<Result<Vec<()>>>().map(drop)
    }

    /// Every 2- and 3-hop cycle from each start token, sized by ternary search.
    async fn best_cycle(
        &self,
        bot: &Bot,
        snapshots: &[Option<PoolSnapshot>],
        prices: &PriceBook,
    ) -> Result<Option<Candidate>> {
        let owner = bot.wallet.pubkey();
        let edges: Vec<(usize, Pubkey, Pubkey)> = snapshots
            .iter()
            .enumerate()
            .filter_map(|(index, snapshot)| Some((index, snapshot.as_ref()?.mints())))
            .flat_map(|(index, (a, b))| [(index, a, b), (index, b, a)])
            .collect();
        let mut best: Option<Candidate> = None;
        for start in &bot.start_tokens {
            let held = token_balance(&self.chain, &owner, start).await?;
            let usd = prices.usd(start)?;
            let cap = to_atoms(start, bot.config.max_trade_usd / usd)
                .unwrap_or(0)
                .min(held / 100 * 95);
            if cap == 0 {
                continue;
            }
            for cycle in cycles(&edges, start.mint, bot.config.max_legs) {
                let Some((amount_in, amount_out)) = optimize(snapshots, &cycle, cap) else {
                    continue;
                };
                let profit_usd = to_ui(start, amount_out - amount_in) * usd;
                if best.as_ref().is_none_or(|b| profit_usd > b.profit_usd) {
                    best = Some(Candidate {
                        hops: cycle,
                        start: start.clone(),
                        amount_in,
                        amount_out,
                        profit_usd,
                    });
                }
            }
        }
        Ok(best.filter(|candidate| candidate.profit_usd >= bot.config.min_profit_usd))
    }

    async fn execute(
        &self,
        bot: &Bot,
        snapshots: &[Option<PoolSnapshot>],
        prices: &PriceBook,
        candidate: Candidate,
    ) -> Result<()> {
        let owner = bot.wallet.pubkey();
        let start = &candidate.start;
        let min_profit =
            to_atoms(start, bot.config.min_profit_usd / prices.usd(start)?).unwrap_or(0);
        let floor = candidate.amount_in.saturating_add(min_profit.max(1));
        // Conservative chaining: each leg spends the previous leg's minimum;
        // the last leg's minimum is the profit floor.
        let mut amount = candidate.amount_in;
        let mut legs: Vec<(usize, LegQuote)> = Vec::new();
        let last = candidate.hops.len() - 1;
        for (index, hop) in candidate.hops.iter().enumerate() {
            let snapshot = snapshots[hop.pool].as_ref().context("pool vanished")?;
            // A cycle sized at the edge of a book's depth can fail to requote
            // with slippage; treat it like a closed opportunity.
            let Ok(mut quote) = snapshot.quote(
                hop.input,
                hop.output,
                amount,
                true,
                bot.config.leg_slippage_bps,
            ) else {
                counter!("arb_trades_total", "bot" => bot.config.name.clone(), "status" => "quote_failed").increment(1);
                return Ok(());
            };
            if index == last {
                if quote.out_amount < floor {
                    debug!(
                        expected = quote.out_amount,
                        floor, "cycle no longer clears the profit floor"
                    );
                    counter!("arb_trades_total", "bot" => bot.config.name.clone(), "status" => "below_floor").increment(1);
                    return Ok(());
                }
                // Pick the slippage whose minimum lands on the profit floor.
                let headroom =
                    (quote.out_amount - floor) as u128 * 10_000 / quote.out_amount as u128;
                quote = snapshot.quote(
                    hop.input,
                    hop.output,
                    amount,
                    true,
                    headroom.min(10_000) as u16,
                )?;
            }
            amount = quote.min_out;
            legs.push((hop.pool, quote));
        }

        let mut seen = HashSet::new();
        let mut instructions: Vec<Instruction> = Vec::new();
        let mut cu: u32 = 50_000;
        for (pool, quote) in &legs {
            let snapshot = snapshots[*pool].as_ref().context("pool vanished")?;
            for ix in snapshot.swap_instructions(owner, quote)? {
                if ix.program_id == ATA_PROGRAM_ID
                    && !seen.insert(ix.accounts.get(1).map(|m| m.pubkey))
                {
                    continue;
                }
                instructions.push(ix);
            }
            cu = cu.saturating_add(quote.cu_estimate);
        }
        let route: Vec<String> = legs
            .iter()
            .map(|(pool, quote)| {
                let snapshot = snapshots[*pool].as_ref();
                let name = |mint: &Pubkey| {
                    self.scenario
                        .token_by_mint(mint)
                        .map_or("?".into(), |t| t.name.clone())
                };
                format!(
                    "{}:{}→{}",
                    snapshot.map_or("?", |s| s.kind().as_str()),
                    name(&quote.input_mint),
                    name(&quote.output_mint)
                )
            })
            .collect();
        let before = token_balance(&self.chain, &owner, start).await?;
        let blockhash = self.chain.rpc().get_latest_blockhash().await?;
        let config = TransactionConfig::empty()
            .with_compute_unit_limit(cu.min(MAX_COMPUTE_UNIT_LIMIT))
            .with_loaded_accounts_data_size_limit(64 * 1024 * 1024);
        let message =
            v1::Message::try_compile_with_config(&owner, &instructions, blockhash, config)
                .context("compiling v1 message")?;
        let tx = VersionedTransaction::try_new(VersionedMessage::V1(message), &[&bot.wallet])?;
        let simulation = self.chain.rpc().simulate_transaction(&tx).await?;
        if let Some(error) = simulation.value.err {
            debug!(?error, route = ?route, "cycle simulation failed");
            counter!("arb_trades_total", "bot" => bot.config.name.clone(), "status" => "simulation_failed").increment(1);
            return Ok(());
        }
        let signature = match self.chain.rpc().send_and_confirm_transaction(&tx).await {
            Ok(signature) => signature,
            Err(error) => {
                // Lost the race (another searcher closed the gap first).
                debug!(%error, route = ?route, "cycle transaction failed");
                counter!("arb_trades_total", "bot" => bot.config.name.clone(), "status" => "reverted").increment(1);
                return Ok(());
            }
        };
        let after = token_balance(&self.chain, &owner, start).await?;
        let realized = to_ui(start, after) - to_ui(start, before);
        let realized_usd = realized * prices.usd(start)?;
        counter!("arb_trades_total", "bot" => bot.config.name.clone(), "status" => "landed", "legs" => legs.len().to_string()).increment(1);
        metrics::gauge!("arb_last_profit_usd", "bot" => bot.config.name.clone())
            .set(realized_usd.to_f64().unwrap_or_default());
        counter!("arb_profit_micro_usd_total", "bot" => bot.config.name.clone()).increment(
            (realized_usd * Decimal::from(1_000_000))
                .to_u64()
                .unwrap_or_default(),
        );
        info!(
            bot = %bot.config.name, route = %route.join(" "), input = %to_ui(start, candidate.amount_in),
            expected_profit_usd = %candidate.profit_usd.round_dp(4), realized_usd = %realized_usd.round_dp(4),
            token = %start.name, expected_out = %to_ui(start, candidate.amount_out), %signature,
            "arbitrage cycle landed"
        );
        Ok(())
    }
}

/// Cycles from `start` back to `start` with distinct pools, 2..=max hops.
fn cycles(edges: &[(usize, Pubkey, Pubkey)], start: Pubkey, max: usize) -> Vec<Vec<Hop>> {
    let mut out = Vec::new();
    let hop = |(pool, input, output): &(usize, Pubkey, Pubkey)| Hop {
        pool: *pool,
        input: *input,
        output: *output,
    };
    for first in edges.iter().filter(|e| e.1 == start) {
        for second in edges.iter().filter(|e| e.1 == first.2 && e.0 != first.0) {
            if second.2 == start {
                out.push(vec![hop(first), hop(second)]);
            } else if max >= 3 {
                for third in edges.iter().filter(|e| {
                    e.1 == second.2 && e.2 == start && e.0 != first.0 && e.0 != second.0
                }) {
                    out.push(vec![hop(first), hop(second), hop(third)]);
                }
            }
        }
    }
    out
}

/// Exact-in output of a whole cycle (no slippage), or `None` if any hop fails.
fn run(snapshots: &[Option<PoolSnapshot>], cycle: &[Hop], amount: u64) -> Option<u64> {
    let mut amount = amount;
    for hop in cycle {
        let snapshot = snapshots[hop.pool].as_ref()?;
        amount = snapshot
            .quote(hop.input, hop.output, amount, true, 0)
            .ok()?
            .out_amount;
        if amount == 0 {
            return None;
        }
    }
    Some(amount)
}

/// Profit-maximizing input in `[1, cap]` (profit is concave in the input):
/// ternary search, then `(input, output)` if it is profitable.
fn optimize(snapshots: &[Option<PoolSnapshot>], cycle: &[Hop], cap: u64) -> Option<(u64, u64)> {
    let profit = |x: u64| -> i128 {
        run(snapshots, cycle, x).map_or(i128::MIN / 2, |out| out as i128 - x as i128)
    };
    // Cheap rejection: a small probe must already be profitable.
    let probe = (cap / 1_000).max(1);
    if profit(probe) <= 0 {
        return None;
    }
    // Stop at 0.01% of the cap: finer sizing is worth less than a fresher scan.
    let tolerance = (cap / 10_000).max(3);
    let (mut low, mut high) = (probe, cap);
    for _ in 0..48 {
        if high - low < tolerance {
            break;
        }
        let third = (high - low) / 3;
        let (m1, m2) = (low + third, high - third);
        if profit(m1) < profit(m2) {
            low = m1;
        } else {
            high = m2;
        }
    }
    let best = [low, low + (high - low) / 2, high]
        .into_iter()
        .max_by_key(|x| profit(*x))?;
    let out = run(snapshots, cycle, best)?;
    (out > best).then_some((best, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumerates_two_and_three_hop_cycles() {
        let (a, b, c) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        // pools: 0 = a/b, 1 = a/b (other venue), 2 = b/c, 3 = c/a
        let edges = vec![
            (0, a, b),
            (0, b, a),
            (1, a, b),
            (1, b, a),
            (2, b, c),
            (2, c, b),
            (3, c, a),
            (3, a, c),
        ];
        let two_or_three = cycles(&edges, a, 3);
        let twos = two_or_three.iter().filter(|c| c.len() == 2).count();
        let threes = two_or_three.iter().filter(|c| c.len() == 3).count();
        // a→b→a over (0,1) and (1,0); a→b→c→a over pool 0 or 1, and the reverse a→c→b→a
        assert_eq!(twos, 2);
        assert_eq!(threes, 4);
        assert!(cycles(&edges, a, 2).iter().all(|c| c.len() == 2));
        for cycle in &two_or_three {
            assert_eq!(cycle.first().map(|h| h.input), Some(a));
            assert_eq!(cycle.last().map(|h| h.output), Some(a));
        }
    }
}
