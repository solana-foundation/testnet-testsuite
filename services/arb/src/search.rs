//! Cycle search over pool snapshots, and atomic execution.
//!
//! [`Searcher::scan`] refreshes the snapshots and plans at most one cycle
//! per idle bot; [`Searcher::execute`] submits a plan and accounts for it.
//! The two are separate so the main loop keeps scanning while earlier
//! cycles confirm.

use std::collections::{BTreeSet, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use futures::future::join_all;
use metrics::counter;
use mtm_chain::{ChainClient, SubmissionError, VersionedSendConfig};
use mtm_scenario::funding::{balances, wallet_balances};
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
use tracing::{debug, info, warn};

const ATA_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
/// Upper bound on compute for one cycle transaction.
const MAX_COMPUTE_UNIT_LIMIT: u32 = 1_400_000;
/// A reload slower than this keeps the cached snapshot (the pre-send
/// simulation still catches a stale one).
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(2);

/// Every pool's latest snapshot, shared by the scan and the executions it
/// planned (cheap to hand out: pools are behind their own `Arc`).
pub type Snapshots = Arc<Vec<Option<Arc<PoolSnapshot>>>>;

struct Bot {
    config: ArbBotConfig,
    wallet: Keypair,
    start_tokens: Vec<Token>,
}

/// One hop of a cycle: which pool, in which direction.
#[derive(Clone, Copy, Debug)]
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

/// A cycle one bot will try, with the state it was planned on.
pub struct Plan {
    pub bot: usize,
    candidate: Candidate,
    snapshots: Snapshots,
    prices: Arc<PriceBook>,
}

pub struct Searcher<'s> {
    chain: ChainClient,
    scenario: &'s Scenario,
    oracle: OracleClient,
    pools: Vec<(Venue, Pubkey)>,
    /// Last good snapshot per pool.
    cache: Mutex<Vec<Option<Arc<PoolSnapshot>>>>,
    /// Pools reported changed whose reload has not succeeded yet: a failed
    /// reload must not let the cache stick on stale state.
    dirty: Mutex<HashSet<Pubkey>>,
    bots: Vec<Bot>,
}

impl<'s> Searcher<'s> {
    pub fn new(
        chain: ChainClient,
        scenario: &'s Scenario,
        pools: Vec<(Venue, Pubkey)>,
        config: &ArbConfig,
    ) -> Result<Self> {
        let bots: Vec<Bot> = config
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
        // Register the accounting series at zero, so `increase()` sees the
        // first trade of each bot instead of dropping it.
        for bot in &bots {
            let name = bot.config.name.clone();
            counter!("arb_profit_micro_usd_total", "bot" => name.clone()).absolute(0);
            counter!("arb_loss_micro_usd_total", "bot" => name.clone()).absolute(0);
            counter!("arb_fees_lamports_total", "bot" => name).absolute(0);
        }
        Ok(Self {
            oracle: OracleClient::new(&scenario.config.oracle_url),
            chain,
            scenario,
            cache: Mutex::new(vec![None; pools.len()]),
            dirty: Mutex::new(HashSet::new()),
            pools,
            bots,
        })
    }

    pub fn pool_count(&self) -> usize {
        self.pools.len()
    }

    pub fn bot_count(&self) -> usize {
        self.bots.len()
    }

    /// Refresh the snapshots and plan the best cycle for every bot not in
    /// `busy`. `changed`: pools the websocket reported (`None` reloads all).
    pub async fn scan(
        &self,
        trigger: &'static str,
        changed: Option<&HashSet<Pubkey>>,
        busy: &HashSet<usize>,
    ) -> Result<Vec<Plan>> {
        let snapshots = self.refresh(changed).await;
        let prices =
            Arc::new(PriceBook::fetch(&self.oracle, self.scenario.config.max_price_age_ms).await?);
        let idle: Vec<usize> = (0..self.bots.len())
            .filter(|bot| !busy.contains(bot))
            .collect();
        // Every idle bot's start-token balances in one batched read.
        let pairs: Vec<(Pubkey, &Token)> = idle
            .iter()
            .flat_map(|&bot| {
                let owner = self.bots[bot].wallet.pubkey();
                self.bots[bot]
                    .start_tokens
                    .iter()
                    .map(move |token| (owner, token))
            })
            .collect();
        let (held, _) = balances(&self.chain, &pairs, &[]).await?;
        let mut held = held.into_iter();
        let edges = edges(&snapshots);
        let mut plans = Vec::new();
        for bot_index in idle {
            let bot = &self.bots[bot_index];
            let held: Vec<u64> = held.by_ref().take(bot.start_tokens.len()).collect();
            let Some(candidate) = best_cycle(bot, &snapshots, &edges, &prices, &held)? else {
                continue;
            };
            counter!("arb_opportunities_total", "bot" => bot.config.name.clone(), "legs" => candidate.hops.len().to_string(), "trigger" => trigger).increment(1);
            plans.push(Plan {
                bot: bot_index,
                candidate,
                snapshots: Arc::clone(&snapshots),
                prices: Arc::clone(&prices),
            });
        }
        Ok(plans)
    }

    /// Reload changed, dirty and missing pools; keep the rest.
    async fn refresh(&self, changed: Option<&HashSet<Pubkey>>) -> Snapshots {
        let cached = self.cache.lock().expect("cache lock").clone();
        let dirty = {
            let mut dirty = self.dirty.lock().expect("dirty lock");
            dirty.extend(changed.into_iter().flatten().copied());
            dirty.clone()
        };
        let loads = self.pools.iter().zip(cached).map(|((venue, pool), cached)| {
            let reload = cached.is_none() || changed.is_none() || dirty.contains(pool);
            async move {
                if !reload {
                    return (cached, false);
                }
                let started = std::time::Instant::now();
                let load = PoolSnapshot::load(
                    &self.chain,
                    Scenario::venue_kind(*venue),
                    self.scenario.programs.of(*venue),
                    *pool,
                );
                // Without a cached snapshot there is nothing to fall back to: wait.
                let timeout = if cached.is_some() {
                    SNAPSHOT_TIMEOUT
                } else {
                    Duration::from_secs(60)
                };
                match tokio::time::timeout(timeout, load).await {
                    Ok(Ok(snapshot)) => {
                        debug!(venue = venue.as_str(), %pool, elapsed_ms = started.elapsed().as_millis(), "pool snapshot");
                        (Some(Arc::new(snapshot)), true)
                    }
                    Ok(Err(error)) => {
                        debug!(venue = venue.as_str(), %pool, %error, "pool snapshot failed");
                        counter!("arb_snapshot_failures_total", "reason" => "error").increment(1);
                        (cached, false)
                    }
                    Err(_) => {
                        counter!("arb_snapshot_failures_total", "reason" => "timeout").increment(1);
                        (cached, false)
                    }
                }
            }
        });
        let results = join_all(loads).await;
        {
            let mut dirty = self.dirty.lock().expect("dirty lock");
            for ((_, pool), (_, loaded)) in self.pools.iter().zip(&results) {
                if *loaded {
                    dirty.remove(pool);
                }
            }
        }
        let snapshots: Vec<Option<Arc<PoolSnapshot>>> =
            results.into_iter().map(|(snapshot, _)| snapshot).collect();
        self.cache
            .lock()
            .expect("cache lock")
            .clone_from(&snapshots);
        Arc::new(snapshots)
    }

    /// Build, simulate, send and account for one planned cycle. Never
    /// fails: every outcome is a metric. Returns the bot, now idle again.
    pub async fn execute(&self, plan: Plan) -> usize {
        let bot = &self.bots[plan.bot];
        if let Err(error) = self.try_execute(bot, &plan).await {
            warn!(bot = %bot.config.name, error = %format!("{error:#}"), "arb execution failed");
            counter!("arb_trades_total", "bot" => bot.config.name.clone(), "status" => "error")
                .increment(1);
        }
        plan.bot
    }

    async fn try_execute(&self, bot: &Bot, plan: &Plan) -> Result<()> {
        let status = |status: &'static str| {
            counter!("arb_trades_total", "bot" => bot.config.name.clone(), "status" => status)
                .increment(1);
        };
        if bot.config.latency_ms > 0 {
            tokio::time::sleep(Duration::from_millis(bot.config.latency_ms)).await;
        }
        let (snapshots, prices, candidate) = (&plan.snapshots, &plan.prices, &plan.candidate);
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
            let snapshot = snapshots[hop.pool].as_deref().context("pool vanished")?;
            // A cycle sized at the edge of a book's depth can fail to requote
            // with slippage; treat it like a closed opportunity.
            let Ok(mut quote) = snapshot.quote(
                hop.input,
                hop.output,
                amount,
                true,
                bot.config.leg_slippage_bps,
            ) else {
                status("quote_failed");
                return Ok(());
            };
            if index == last {
                let Some(headroom) = headroom_bps(quote.out_amount, floor) else {
                    debug!(
                        expected = quote.out_amount,
                        floor, "cycle no longer clears the profit floor"
                    );
                    status("below_floor");
                    return Ok(());
                };
                // Pick the slippage whose minimum lands on the profit floor.
                let Ok(floored) = snapshot.quote(hop.input, hop.output, amount, true, headroom)
                else {
                    status("quote_failed");
                    return Ok(());
                };
                quote = floored;
            }
            amount = quote.min_out;
            legs.push((hop.pool, quote));
        }

        let mut instructions: Vec<Instruction> = Vec::new();
        let mut cu: u32 = 50_000;
        for (pool, quote) in &legs {
            let snapshot = snapshots[*pool].as_deref().context("pool vanished")?;
            instructions.extend(snapshot.swap_instructions(owner, quote)?);
            cu = cu.saturating_add(quote.cu_estimate);
        }
        let instructions = dedup_ata_creates(instructions);
        let route = self.route(snapshots, &legs);
        // Every token the cycle touches, so residues left by the
        // conservative chaining are accounted for too.
        let touched: Vec<&Token> = legs
            .iter()
            .flat_map(|(_, quote)| [quote.input_mint, quote.output_mint])
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter_map(|mint| self.scenario.token_by_mint(&mint))
            .collect();
        let before = wallet_balances(&self.chain, &owner, &touched, None).await?;
        let blockhash = self.chain.rpc().get_latest_blockhash().await?;
        let mut config = TransactionConfig::empty()
            .with_compute_unit_limit(cu.min(MAX_COMPUTE_UNIT_LIMIT))
            .with_loaded_accounts_data_size_limit(64 * 1024 * 1024);
        if bot.config.priority_fee_lamports > 0 {
            config = config.with_priority_fee(bot.config.priority_fee_lamports);
        }
        let message =
            v1::Message::try_compile_with_config(&owner, &instructions, blockhash, config)
                .context("compiling v1 message")?;
        let tx = VersionedTransaction::try_new(VersionedMessage::V1(message), &[&bot.wallet])?;
        let simulation = self.chain.rpc().simulate_transaction(&tx).await?;
        if let Some(error) = simulation.value.err {
            debug!(?error, route = ?route, "cycle simulation failed");
            status("simulation_failed");
            return Ok(());
        }
        // Already simulated: preflight would only run it a second time.
        let send = VersionedSendConfig {
            skip_preflight: true,
            ..VersionedSendConfig::default()
        };
        let (signature, slot) = match self.chain.send_versioned(&tx, send).await {
            Ok(outcome) => (outcome.signature, outcome.confirmation_slot),
            Err(SubmissionError::Execution {
                signature,
                slot,
                error,
            }) => {
                // Lost the race (another searcher closed the gap first): the
                // fee is still paid.
                debug!(%signature, %error, route = ?route, "cycle reverted on-chain");
                status("reverted");
                self.account(bot, &touched, &before, slot, prices).await?;
                return Ok(());
            }
            Err(error) => {
                let label = match &error {
                    SubmissionError::Expired { .. } => "expired",
                    SubmissionError::AmbiguousConfirmation { .. } => "ambiguous",
                    SubmissionError::Simulation { .. } => "simulation_failed",
                    _ => "send_failed",
                };
                debug!(%error, route = ?route, "cycle not landed");
                status(label);
                return Ok(());
            }
        };
        let (realized_usd, fee_lamports) =
            self.account(bot, &touched, &before, slot, prices).await?;
        counter!("arb_trades_total", "bot" => bot.config.name.clone(), "status" => "landed", "legs" => legs.len().to_string()).increment(1);
        metrics::gauge!("arb_last_profit_usd", "bot" => bot.config.name.clone())
            .set(realized_usd.to_f64().unwrap_or_default());
        info!(
            bot = %bot.config.name, route = %route.join(" "), input = %to_ui(start, candidate.amount_in),
            expected_profit_usd = %candidate.profit_usd.round_dp(4), realized_usd = %realized_usd.round_dp(4),
            fee_lamports, token = %start.name, expected_out = %to_ui(start, candidate.amount_out), %signature, slot,
            "arbitrage cycle landed"
        );
        Ok(())
    }

    /// Value every touched token's change at oracle prices (gains and
    /// losses on separate counters) and the SOL spent, from a read pinned
    /// to the transaction's slot.
    async fn account(
        &self,
        bot: &Bot,
        touched: &[&Token],
        before: &mtm_scenario::funding::WalletBalances,
        slot: u64,
        prices: &PriceBook,
    ) -> Result<(Decimal, u64)> {
        let owner = bot.wallet.pubkey();
        let after = wallet_balances(&self.chain, &owner, touched, Some(slot)).await?;
        let mut realized_usd = Decimal::ZERO;
        for ((token, was), now) in touched.iter().zip(&before.tokens).zip(&after.tokens) {
            let delta = to_ui(token, *now) - to_ui(token, *was);
            realized_usd += delta * prices.usd(token)?;
        }
        let fee_lamports = before.lamports.saturating_sub(after.lamports);
        let name = bot.config.name.clone();
        let micro = |usd: Decimal| {
            (usd * Decimal::from(1_000_000))
                .round()
                .to_u64()
                .unwrap_or_default()
        };
        if realized_usd.is_sign_negative() {
            counter!("arb_loss_micro_usd_total", "bot" => name.clone())
                .increment(micro(-realized_usd));
        } else {
            counter!("arb_profit_micro_usd_total", "bot" => name.clone())
                .increment(micro(realized_usd));
        }
        counter!("arb_fees_lamports_total", "bot" => name).increment(fee_lamports);
        Ok((realized_usd, fee_lamports))
    }

    fn route(
        &self,
        snapshots: &[Option<Arc<PoolSnapshot>>],
        legs: &[(usize, LegQuote)],
    ) -> Vec<String> {
        let name = |mint: &Pubkey| {
            self.scenario
                .token_by_mint(mint)
                .map_or("?".into(), |t| t.name.clone())
        };
        legs.iter()
            .map(|(pool, quote)| {
                format!(
                    "{}:{}→{}",
                    snapshots[*pool]
                        .as_deref()
                        .map_or("?", |s| s.kind().as_str()),
                    name(&quote.input_mint),
                    name(&quote.output_mint)
                )
            })
            .collect()
    }
}

/// Directed edges `(pool, input, output)` over every loaded pool.
fn edges(snapshots: &[Option<Arc<PoolSnapshot>>]) -> Vec<(usize, Pubkey, Pubkey)> {
    snapshots
        .iter()
        .enumerate()
        .filter_map(|(index, snapshot)| Some((index, snapshot.as_deref()?.mints())))
        .flat_map(|(index, (a, b))| [(index, a, b), (index, b, a)])
        .collect()
}

/// The most profitable 2- or 3-hop cycle from any of the bot's start
/// tokens (`held`: their balances, in order), above its profit floor.
fn best_cycle(
    bot: &Bot,
    snapshots: &[Option<Arc<PoolSnapshot>>],
    edges: &[(usize, Pubkey, Pubkey)],
    prices: &PriceBook,
    held: &[u64],
) -> Result<Option<Candidate>> {
    let mut best: Option<Candidate> = None;
    for (start, held) in bot.start_tokens.iter().zip(held) {
        let usd = prices.usd(start)?;
        let cap = to_atoms(start, bot.config.max_trade_usd / usd)
            .unwrap_or(0)
            .min(held / 100 * 95);
        if cap == 0 {
            continue;
        }
        for cycle in cycles(edges, start.mint, bot.config.max_legs) {
            let output = |x| run(snapshots, &cycle, x, bot.config.leg_slippage_bps);
            let Some((amount_in, amount_out)) = optimize(cap, output) else {
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

/// What executing the cycle on `amount` returns: every leg but the last
/// spends the previous leg's slippage-adjusted minimum (as `execute`
/// chains them), the last leg's expected output is the result. `None` if
/// any hop fails or rounds to nothing.
fn run(
    snapshots: &[Option<Arc<PoolSnapshot>>],
    cycle: &[Hop],
    amount: u64,
    leg_slippage_bps: u16,
) -> Option<u64> {
    let mut amount = amount;
    let last = cycle.len().checked_sub(1)?;
    for (index, hop) in cycle.iter().enumerate() {
        let snapshot = snapshots[hop.pool].as_deref()?;
        let slippage = if index == last { 0 } else { leg_slippage_bps };
        let quote = snapshot
            .quote(hop.input, hop.output, amount, true, slippage)
            .ok()?;
        amount = if index == last {
            quote.out_amount
        } else {
            quote.min_out
        };
        if amount == 0 {
            return None;
        }
    }
    Some(amount)
}

/// Profit-maximizing input in `[1, cap]` for `output(input)`, as
/// `(input, output)` when profitable.
///
/// Profit is roughly concave, but rounding makes it jagged at small sizes
/// and the optimum can sit far below the cap, so a single ternary search
/// from the cap can miss it: probe log-spaced sizes first, then
/// ternary-search the bracket around the best probe.
fn optimize(cap: u64, output: impl Fn(u64) -> Option<u64>) -> Option<(u64, u64)> {
    let profit =
        |x: u64| -> i128 { output(x).map_or(i128::MIN / 2, |out| out as i128 - x as i128) };
    // cap, cap/2, cap/4, ..., 1
    let probes: Vec<u64> =
        std::iter::successors(Some(cap), |x| (*x > 1).then_some(x / 2)).collect();
    let (best, _) = probes
        .iter()
        .enumerate()
        .map(|(index, x)| (index, profit(*x)))
        .max_by_key(|(_, p)| *p)?;
    // The optimum lies between the best probe's neighbours.
    let high = if best == 0 { cap } else { probes[best - 1] };
    let low = probes.get(best + 1).copied().unwrap_or(1).max(1);
    // Stop at 0.01% of the bracket: finer sizing is worth less than a
    // fresher scan.
    let tolerance = ((high - low) / 10_000).max(3);
    let (mut lo, mut hi) = (low, high);
    for _ in 0..64 {
        if hi - lo < tolerance {
            break;
        }
        let third = (hi - lo) / 3;
        let (m1, m2) = (lo + third, hi - third);
        if profit(m1) < profit(m2) {
            lo = m1;
        } else {
            hi = m2;
        }
    }
    let best = [probes[best], lo, lo + (hi - lo) / 2, hi]
        .into_iter()
        .max_by_key(|x| profit(*x))?;
    let out = output(best)?;
    (out > best).then_some((best, out))
}

/// Slippage (bps of `expected`) whose minimum output is `floor`, or `None`
/// when the expected output does not clear the floor.
fn headroom_bps(expected: u64, floor: u64) -> Option<u16> {
    if expected < floor || expected == 0 {
        return None;
    }
    let bps = u128::from(expected - floor) * 10_000 / u128::from(expected);
    Some(bps.min(10_000) as u16)
}

/// Legs on different venues may each create the same associated token
/// account (the cycle's intermediate token); keep the first create per
/// account.
fn dedup_ata_creates(instructions: Vec<Instruction>) -> Vec<Instruction> {
    let mut seen = HashSet::new();
    instructions
        .into_iter()
        .filter(|ix| {
            ix.program_id != ATA_PROGRAM_ID || seen.insert(ix.accounts.get(1).map(|m| m.pubkey))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use solana_instruction::AccountMeta;

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

    /// A constant-product round trip with a price gap: output of x after
    /// buying on a cheap pool and selling on a rich one.
    fn gap(x: u64, depth: f64, edge: f64) -> Option<u64> {
        let x = x as f64;
        let bought = depth * x / (depth + x) * edge;
        let sold = depth * bought / (depth + bought);
        Some(sold as u64)
    }

    #[test]
    fn optimize_finds_an_optimum_far_below_the_cap() {
        // A 1% gap in pools 1e6 deep peaks around 2.5e3, far below a 1e9
        // cap: the old cap/1000 probe (1e6) was already unprofitable.
        let found = optimize(1_000_000_000, |x| gap(x, 1e6, 1.01));
        let (input, output) = found.expect("profitable cycle");
        let best = (1..20_000_u64)
            .map(|x| gap(x, 1e6, 1.01).unwrap_or(0) as i128 - x as i128)
            .max()
            .unwrap_or_default();
        let got = output as i128 - input as i128;
        assert!(got * 100 >= best * 95, "got {got}, best {best}");
    }

    #[test]
    fn optimize_rejects_unprofitable_cycles() {
        assert!(optimize(1_000_000, |x| gap(x, 1e6, 0.99)).is_none());
        assert!(optimize(1_000_000, |_| None).is_none());
        assert!(optimize(1, Some).is_none());
    }

    #[test]
    fn headroom_lands_the_minimum_on_the_floor() {
        assert_eq!(headroom_bps(10_000, 9_900), Some(100));
        assert_eq!(headroom_bps(10_000, 10_000), Some(0));
        assert_eq!(headroom_bps(9_999, 10_000), None);
        assert_eq!(headroom_bps(0, 0), None);
        // the minimum the venue derives from it never undercuts the floor
        let (expected, floor) = (1_234_567_u64, 1_200_000_u64);
        let bps = u64::from(headroom_bps(expected, floor).expect("clears"));
        assert!(expected - expected * bps / 10_000 >= floor);
    }

    #[test]
    fn duplicate_ata_creates_are_dropped() {
        let (ata, other) = (Pubkey::new_unique(), Pubkey::new_unique());
        let create = |account: Pubkey| Instruction {
            program_id: ATA_PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(Pubkey::default(), true),
                AccountMeta::new(account, false),
            ],
            data: vec![1],
        };
        let swap = Instruction {
            program_id: Pubkey::new_unique(),
            accounts: vec![],
            data: vec![],
        };
        let out = dedup_ata_creates(vec![
            create(ata),
            swap.clone(),
            create(ata),
            create(other),
            swap,
        ]);
        assert_eq!(out.len(), 4);
        assert_eq!(
            out.iter()
                .filter(|ix| ix.program_id == ATA_PROGRAM_ID)
                .count(),
            2
        );
    }
}
