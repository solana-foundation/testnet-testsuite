//! Uninformed flow: random takers swapping through the router.
//!
//! Arrivals are Poisson (`rate_per_min`), notionals lognormal around
//! `median_usd` with occasional large blocks (`large_probability` ×
//! `large_multiplier`), pairs and directions random but constrained by what
//! the taker holds. Each trade asks the router for a transaction, signs it as
//! the taker, submits it, and measures landed versus quoted (slippage) and
//! versus the oracle (cost to the uninformed trader). Takers whose inventory
//! or fee SOL runs low are topped back up (test tokens only). The RNG is
//! seeded, so a run's trade sequence is replayable.
//!
//! Every trade ends in one [`Outcome`], exported as the `outcome` label of
//! `flow_swaps_total` next to the coarse `status` (landed/failed), so market
//! effects (unaffordable, preflight slippage, landed-failed) can be told
//! apart from infrastructure faults (expired, ambiguous RPC).
//!
//! Each landed swap is a `debug` line; `info` gets one summary a minute
//! (outcome counts), so a long run at thousands of swaps per second doesn't
//! write gigabytes of logs (a 2 h x1000 run wrote 6 GB at one line per swap).
//!
//! Notionals are sampled in `f64` (the distributions are float-only) and
//! rounded to micro-USD `Decimal` straight away; nothing downstream sees the
//! float.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine;
use metrics::{counter, gauge, histogram};
use mtm_chain::ChainClient;
use mtm_common::config::RpcConfig;
use mtm_dex_harness::cli::signer_from_environment;
use mtm_scenario::funding::{Funding, LAMPORTS_PER_SOL, top_up, wallet_balances};
use mtm_scenario::pricing::PriceBook;
use mtm_scenario::venue::{to_atoms, to_ui};
use mtm_scenario::{FlowConfig, Scenario, ScenarioConfig, Token};
use oracle_client::OracleClient;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use rand_distr::{Distribution, Exp, LogNormal, Uniform};
use router_client::{Execution, QuoteRequest, RouterClient, SwapMode};
use rust_decimal::Decimal;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use serde::Deserialize;
use solana_keypair::Keypair;
use solana_rpc_client_api::client_error::ErrorKind as RpcErrorKind;
use solana_rpc_client_api::config::RpcSendTransactionConfig;
use solana_signature::Signature;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
use tracing::{debug, error, info, warn};

#[derive(Debug, Deserialize)]
struct AppConfig {
    rpc: RpcConfig,
    flow: FlowServiceConfig,
    scenario: ScenarioConfig,
}

#[derive(Debug, Deserialize)]
struct FlowServiceConfig {
    metrics_addr: Option<SocketAddr>,
    #[serde(default = "default_faucet")]
    faucet_keypair_env: String,
    #[serde(default = "default_mint_authority")]
    mint_authority_keypair_env: String,
}

fn default_faucet() -> String {
    "KEYPAIR_FAUCET".to_owned()
}
fn default_mint_authority() -> String {
    "KEYPAIR_TOKEN_MINT_AUTHORITY".to_owned()
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let _ = dotenvy::dotenv();
    let cfg: AppConfig = mtm_common::config::load().map_err(|error| anyhow::anyhow!("{error}"))?;
    mtm_telemetry::init("flow", cfg.flow.metrics_addr)?;
    let chain = ChainClient::new(&cfg.rpc.http_url, &cfg.rpc.commitment)?;
    let scenario = Scenario::resolve(cfg.scenario, &chain).await?;
    let flow = scenario
        .config
        .flow
        .clone()
        .context("the profile has no [scenario.flow] section")?;
    let refill = match (
        signer_from_environment(&cfg.flow.faucet_keypair_env),
        signer_from_environment(&cfg.flow.mint_authority_keypair_env),
    ) {
        (Ok(faucet), Ok(authority)) => Some((faucet, authority)),
        _ => {
            warn!("faucet or mint authority unset: taker inventory will not be refilled");
            None
        }
    };
    let takers: Vec<Keypair> = (0..flow.takers).map(|i| scenario.taker_wallet(i)).collect();
    anyhow::ensure!(!takers.is_empty(), "the flow needs at least one taker");
    // Takers are sharded over OS threads, each with its own single-threaded
    // runtime and clients: one join_all over thousands of takers would run
    // them all on one core. They cannot be spawned onto the multi-threaded
    // runtime instead because `top_up` (and `ChainClient::submit` under it)
    // hold `&dyn Signer`, which is not `Sync`, across awaits, so the trade
    // futures are not `Send`.
    let shards = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(takers.len());
    info!(
        takers = takers.len(),
        shards,
        rate_per_min = flow.rate_per_min,
        "flow started"
    );
    let tally = std::sync::Arc::new(Tally::default());
    tokio::spawn(summarize(std::sync::Arc::clone(&tally)));
    let rpc = &cfg.rpc;
    let (scenario, flow, takers) = (&scenario, &flow, &takers);
    tokio::task::block_in_place(|| {
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..shards)
                .map(|shard| {
                    let refill = refill.clone();
                    let tally = std::sync::Arc::clone(&tally);
                    scope.spawn(move || -> Result<()> {
                        let runtime = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()?;
                        runtime.block_on(async {
                            let engine = Flow {
                                router: RouterClient::new(&scenario.config.router_url),
                                oracle: OracleClient::new(&scenario.config.oracle_url),
                                chain: ChainClient::new(&rpc.http_url, &rpc.commitment)?,
                                scenario,
                                flow,
                                refill,
                                tally,
                            };
                            engine.run(takers, shard, shards).await
                        })
                    })
                })
                .collect();
            for handle in handles {
                handle
                    .join()
                    .map_err(|_| anyhow::anyhow!("flow shard panicked"))??;
            }
            Ok(())
        })
    })
}

struct Flow<'a> {
    router: RouterClient,
    oracle: OracleClient,
    chain: ChainClient,
    scenario: &'a Scenario,
    flow: &'a FlowConfig,
    refill: Option<(
        std::sync::Arc<dyn Signer + Send + Sync>,
        std::sync::Arc<dyn Signer + Send + Sync>,
    )>,
    tally: std::sync::Arc<Tally>,
}

/// Trade outcomes since the last summary, shared by every shard.
#[derive(Default)]
struct Tally(std::sync::Mutex<std::collections::BTreeMap<&'static str, u64>>);

impl Tally {
    fn add(&self, outcome: &'static str) {
        if let Ok(mut counts) = self.0.lock() {
            *counts.entry(outcome).or_default() += 1;
        }
    }

    fn take(&self) -> std::collections::BTreeMap<&'static str, u64> {
        self.0
            .lock()
            .map(|mut c| std::mem::take(&mut *c))
            .unwrap_or_default()
    }
}

/// One `info` line a minute: every trade outcome since the last one.
async fn summarize(tally: std::sync::Arc<Tally>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(60));
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let counts = tally.take();
        let trades: u64 = counts.values().sum();
        let outcomes = counts
            .iter()
            .map(|(outcome, n)| format!("{outcome}={n}"))
            .collect::<Vec<_>>()
            .join(" ");
        info!(trades, %outcomes, "flow, last 60 s");
    }
}

struct Trade<'t> {
    taker: usize,
    input: &'t Token,
    output: &'t Token,
    /// USD, rounded to micro-USD
    notional: Decimal,
    exact_out: bool,
}

/// How a trade ended; the `outcome` label of `flow_swaps_total`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Landed,
    /// neither side of the pair covers the notional
    Unaffordable,
    /// the oracle had no fresh price
    NoPrice,
    /// the router had no route, or did not answer
    QuoteFailed,
    /// the RPC's preflight simulation rejected the swap (typically slippage)
    PreflightRejected,
    /// the RPC refused the transaction for another reason
    SendFailed,
    /// the swap landed and failed on-chain
    LandedFailed,
    /// the blockhash expired with no status observed
    Expired,
    /// the status could not be determined (RPC errors or the deadline):
    /// the swap may or may not have landed
    RpcAmbiguous,
    /// anything else (decode, signing, local errors)
    Error,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Landed => "landed",
            Self::Unaffordable => "unaffordable",
            Self::NoPrice => "no_price",
            Self::QuoteFailed => "quote_failed",
            Self::PreflightRejected => "preflight_rejected",
            Self::SendFailed => "send_failed",
            Self::LandedFailed => "landed_failed",
            Self::Expired => "expired",
            Self::RpcAmbiguous => "rpc_ambiguous",
            Self::Error => "error",
        }
    }
}

struct TradeError {
    outcome: Outcome,
    error: anyhow::Error,
}

trait OutcomeContext<T> {
    fn outcome(self, outcome: Outcome) -> std::result::Result<T, TradeError>;
}

impl<T, E: Into<anyhow::Error>> OutcomeContext<T> for std::result::Result<T, E> {
    fn outcome(self, outcome: Outcome) -> std::result::Result<T, TradeError> {
        self.map_err(|error| TradeError {
            outcome,
            error: error.into(),
        })
    }
}

impl<T> OutcomeContext<T> for Option<T> {
    fn outcome(self, outcome: Outcome) -> std::result::Result<T, TradeError> {
        self.ok_or_else(|| TradeError {
            outcome,
            error: anyhow::anyhow!("missing value"),
        })
    }
}

fn fail<T>(outcome: Outcome, error: anyhow::Error) -> std::result::Result<T, TradeError> {
    Err(TradeError { outcome, error })
}

/// How long to poll a sent swap's status, and how often.
const CONFIRM_DEADLINE: Duration = Duration::from_secs(90);
const STATUS_POLL: Duration = Duration::from_millis(400);
/// Check blockhash validity every this many status polls.
const BLOCKHASH_CHECK_EVERY: u32 = 5;
/// A status RPC failing this many polls in a row ends the wait.
const MAX_STATUS_ERRORS: u32 = 5;

impl Flow<'_> {
    /// Every taker is an independent Poisson process at `rate / takers`
    /// with its own seeded RNG, so slow trades never delay other arrivals and
    /// each taker's trade sequence still replays. This shard runs every
    /// `shards`-th taker.
    async fn run(&self, takers: &[Keypair], shard: usize, shards: usize) -> Result<()> {
        let loops = takers
            .iter()
            .enumerate()
            .filter(|(index, _)| index % shards == shard)
            .map(|(index, taker)| self.supervise(index, taker, takers.len()));
        futures::future::join_all(loops).await;
        Ok(())
    }

    /// Keeps one taker running: a loop that errors out is logged and
    /// restarted (with a fresh RNG, so its sequence replays from the start)
    /// instead of silently dropping its share of the load.
    async fn supervise(&self, index: usize, taker: &Keypair, takers: usize) {
        loop {
            gauge!("flow_takers_alive").increment(1.0);
            let result = self.taker_loop(index, taker, takers).await;
            gauge!("flow_takers_alive").decrement(1.0);
            let Err(error) = result;
            error!(taker = index, error = %format!("{error:#}"), "taker loop died; restarting");
            counter!("flow_taker_restarts_total").increment(1);
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    async fn taker_loop(
        &self,
        index: usize,
        taker: &Keypair,
        takers: usize,
    ) -> Result<std::convert::Infallible> {
        let mut rng = ChaCha8Rng::seed_from_u64(self.flow.rng_seed.wrapping_add(index as u64));
        let arrivals =
            Exp::new(self.flow.rate_per_min / 60.0 / takers as f64).context("rate_per_min")?;
        let size = LogNormal::new(self.flow.median_usd.ln(), self.flow.sigma).context("sigma")?;
        let unit = Uniform::new(0.0_f64, 1.0).context("uniform")?;
        let pairs = self.flow.pairs.len();
        loop {
            // The 10-minute cap only bites at very low per-taker rates; it
            // keeps a single unlucky draw from idling a taker for hours.
            let wait: f64 = arrivals.sample(&mut rng);
            tokio::time::sleep(Duration::from_secs_f64(wait.min(600.0))).await;
            let pair =
                &self.flow.pairs[((unit.sample(&mut rng) * pairs as f64) as usize).min(pairs - 1)];
            let flip = unit.sample(&mut rng) < 0.5;
            let mut notional: f64 = size.sample(&mut rng);
            let large = unit.sample(&mut rng) < self.flow.large_probability;
            if large {
                notional *= self.flow.large_multiplier;
            }
            let notional = Decimal::from_f64(notional)
                .with_context(|| format!("notional {notional} is not representable"))?
                .round_dp(6);
            let exact_out = unit.sample(&mut rng) < self.flow.exact_out_probability;
            let (a, b) = (self.scenario.token(&pair.0)?, self.scenario.token(&pair.1)?);
            let (input, output) = if flip { (b, a) } else { (a, b) };
            let trade = Trade {
                taker: index,
                input,
                output,
                notional,
                exact_out,
            };
            let label = format!("{}/{}", pair.0, pair.1);
            let size_label = if large { "large" } else { "normal" };
            let (status, outcome) = match self.trade(taker, &trade).await {
                Ok(()) => ("landed", Outcome::Landed),
                Err(TradeError { outcome, error }) => {
                    // Routine market outcomes are metrics, not log lines: at
                    // high rates a warn per failed trade floods the logs.
                    if outcome == Outcome::Error {
                        warn!(taker = index, %label, error = %format!("{error:#}"), "trade failed");
                    } else {
                        debug!(taker = index, %label, outcome = outcome.as_str(), error = %format!("{error:#}"), "trade failed");
                    }
                    ("failed", outcome)
                }
            };
            self.tally.add(outcome.as_str());
            counter!(
                "flow_swaps_total",
                "pair" => label,
                "status" => status,
                "outcome" => outcome.as_str(),
                "size" => size_label
            )
            .increment(1);
            if let Err(error) = self.refill(taker).await {
                warn!(taker = index, error = %format!("{error:#}"), "refill failed");
            }
        }
    }

    async fn trade(
        &self,
        taker: &Keypair,
        trade: &Trade<'_>,
    ) -> std::result::Result<(), TradeError> {
        let prices = PriceBook::fetch(&self.oracle, self.scenario.config.max_price_age_ms)
            .await
            .outcome(Outcome::NoPrice)?;
        let owner = taker.pubkey();
        // One read of both sides, before the quote: it decides the
        // direction and is the baseline for the fill, and nothing sits
        // between the quote and the send.
        let before = wallet_balances(&self.chain, &owner, &[trade.input, trade.output], None)
            .await
            .outcome(Outcome::RpcAmbiguous)?;
        let notional = trade.notional;
        let affordable = |token: &Token, balance: u64| -> std::result::Result<bool, TradeError> {
            Ok(
                to_ui(token, balance) * prices.usd(token).outcome(Outcome::NoPrice)?
                    >= notional * Decimal::new(105, 2),
            )
        };
        // Sell whichever side of the pair the taker can afford.
        let (input, output, in_before, out_before) = if affordable(trade.input, before.tokens[0])? {
            (
                trade.input,
                trade.output,
                before.tokens[0],
                before.tokens[1],
            )
        } else if affordable(trade.output, before.tokens[1])? {
            (
                trade.output,
                trade.input,
                before.tokens[1],
                before.tokens[0],
            )
        } else {
            return fail(
                Outcome::Unaffordable,
                anyhow::anyhow!("taker {} cannot afford ${notional}", trade.taker),
            );
        };
        let (input_usd, output_usd) = (
            prices.usd(input).outcome(Outcome::NoPrice)?,
            prices.usd(output).outcome(Outcome::NoPrice)?,
        );
        let (mode, amount) = if trade.exact_out {
            (SwapMode::ExactOut, to_atoms(output, notional / output_usd))
        } else {
            (SwapMode::ExactIn, to_atoms(input, notional / input_usd))
        };
        let Some(amount) = amount.filter(|a| *a > 0) else {
            return fail(Outcome::Error, anyhow::anyhow!("trade rounds to zero"));
        };
        let quote = self
            .router
            .quote(&QuoteRequest {
                input_mint: input.mint.to_string(),
                output_mint: output.mint.to_string(),
                amount,
                mode,
                slippage_bps: self.flow.slippage_bps,
                execution: Execution::Direct,
                user: Some(owner.to_string()),
            })
            .await
            .context("router quote")
            .outcome(Outcome::QuoteFailed)?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(
                quote
                    .tx_base64
                    .as_deref()
                    .context("router returned no transaction")
                    .outcome(Outcome::QuoteFailed)?,
            )
            .outcome(Outcome::Error)?;
        let unsigned: VersionedTransaction =
            wincode::deserialize(&bytes).outcome(Outcome::Error)?;
        let signed =
            VersionedTransaction::try_new(unsigned.message, &[taker]).outcome(Outcome::Error)?;
        let (signature, slot) = self.send_and_confirm(&signed).await?;

        // Pinned to the confirmation slot: a lagging node cannot hand back
        // the pre-swap balances and record a phantom 100% slippage.
        let after = match wallet_balances(&self.chain, &owner, &[input, output], Some(slot)).await {
            Ok(after) => after,
            Err(error) => {
                // The swap landed; only its fill is unknown.
                counter!("flow_fill_read_errors_total").increment(1);
                debug!(taker = trade.taker, %signature, error = %format!("{error:#}"), "fill read failed");
                return Ok(());
            }
        };
        let spent = in_before.saturating_sub(after.tokens[0]);
        let received = after.tokens[1].saturating_sub(out_before);
        let fee_lamports = before.lamports.saturating_sub(after.lamports);

        let pair = format!("{}->{}", input.name, output.name);
        let spent_usd = to_ui(input, spent) * input_usd;
        let received_usd = to_ui(output, received) * output_usd;
        let cost_bps = if spent_usd.is_zero() {
            Decimal::ZERO
        } else {
            (spent_usd - received_usd) / spent_usd * Decimal::from(10_000)
        };
        // Both modes are normalised by the quoted amount (positive = worse):
        // exact-in by output short of the quote, exact-out by input above it.
        let slippage_bps = if trade.exact_out {
            -ratio_bps(quote.in_amount, spent)
        } else {
            ratio_bps(quote.out_amount, received)
        };
        histogram!("flow_cost_vs_oracle_bps", "pair" => pair.clone())
            .record(cost_bps.to_f64().unwrap_or_default());
        histogram!("flow_slippage_bps", "pair" => pair.clone()).record(slippage_bps);
        histogram!("flow_route_legs").record(quote.route.len() as f64);
        counter!("flow_notional_micro_usd_total", "pair" => pair.clone())
            .increment(micro_usd(spent_usd));
        counter!("flow_fee_lamports_total").increment(fee_lamports);
        for leg in &quote.route {
            counter!("flow_route_legs_total", "venue" => leg.venue.clone()).increment(1);
        }
        debug!(
            taker = trade.taker, %pair, mode = ?mode, %spent_usd, %received_usd, %cost_bps, slippage_bps,
            fee_lamports, legs = quote.route.len(), %signature, slot, "routed swap landed"
        );
        Ok(())
    }

    /// Sends with preflight (the RPC's simulation catches stale quotes
    /// before they cost a fee) and polls the status until the client's
    /// commitment. Returns the signature and the slot it landed in.
    async fn send_and_confirm(
        &self,
        transaction: &VersionedTransaction,
    ) -> std::result::Result<(Signature, u64), TradeError> {
        let rpc = self.chain.rpc();
        let commitment = rpc.commitment();
        let signature = transaction.signatures[0];
        match rpc
            .send_transaction_with_config(
                transaction,
                RpcSendTransactionConfig {
                    preflight_commitment: Some(commitment.commitment),
                    ..RpcSendTransactionConfig::default()
                },
            )
            .await
        {
            Ok(_) => {}
            Err(error) if error.kind().get_transaction_error().is_some() => {
                return fail(Outcome::PreflightRejected, error.into());
            }
            // The request may have reached the node: wait for a status
            // rather than calling it failed.
            Err(error) if is_transport_error(error.kind()) => {
                debug!(%signature, error = %error, "send response lost; polling status");
            }
            Err(error) => return fail(Outcome::SendFailed, error.into()),
        }
        let blockhash = *transaction.message.recent_blockhash();
        let started = Instant::now();
        let (mut polls, mut errors) = (0_u32, 0_u32);
        loop {
            tokio::time::sleep(STATUS_POLL).await;
            polls += 1;
            if started.elapsed() >= CONFIRM_DEADLINE {
                return fail(
                    Outcome::RpcAmbiguous,
                    anyhow::anyhow!("{signature}: no confirmation within {CONFIRM_DEADLINE:?}"),
                );
            }
            match rpc.get_signature_statuses(&[signature]).await {
                Ok(response) => {
                    errors = 0;
                    if let Some(status) = response.value.into_iter().next().flatten() {
                        if let Some(error) = status.err {
                            return fail(
                                Outcome::LandedFailed,
                                anyhow::anyhow!(
                                    "{signature} failed in slot {}: {error:?}",
                                    status.slot
                                ),
                            );
                        }
                        if status.satisfies_commitment(commitment) {
                            return Ok((signature, status.slot));
                        }
                        // Seen but not yet at our commitment: it cannot
                        // expire any more, so skip the blockhash check.
                        continue;
                    }
                }
                Err(error) => {
                    errors += 1;
                    if errors >= MAX_STATUS_ERRORS {
                        return fail(Outcome::RpcAmbiguous, error.into());
                    }
                }
            }
            if polls.is_multiple_of(BLOCKHASH_CHECK_EVERY)
                && let Ok(false) = rpc.is_blockhash_valid(&blockhash, commitment).await
            {
                // One last look: it may have landed just before expiry, in
                // which case the next poll classifies it.
                if let Ok(response) = rpc.get_signature_statuses(&[signature]).await
                    && response.value.first().is_some_and(Option::is_some)
                {
                    continue;
                }
                return fail(
                    Outcome::Expired,
                    anyhow::anyhow!("{signature}: blockhash expired with no status"),
                );
            }
        }
    }

    async fn refill(&self, taker: &Keypair) -> Result<()> {
        let Some((faucet, authority)) = &self.refill else {
            return Ok(());
        };
        let owner = taker.pubkey();
        let targets = self
            .flow
            .inventory
            .iter()
            .map(|(name, amount)| Ok((self.scenario.token(name)?, *amount)))
            .collect::<Result<Vec<_>>>()?;
        let tokens: Vec<&Token> = targets.iter().map(|(token, _)| *token).collect();
        let balances = wallet_balances(&self.chain, &owner, &tokens, None).await?;
        let ratio = self.flow.refill_below_ratio;
        // Fee SOL counts too: a taker out of lamports fails every trade.
        let sol = Decimal::from(balances.lamports) / Decimal::from(LAMPORTS_PER_SOL);
        let low = sol < self.flow.sol * ratio
            || targets
                .iter()
                .zip(&balances.tokens)
                .any(|((token, amount), balance)| to_ui(token, *balance) < *amount * ratio);
        if low {
            let outcome = top_up(
                &self.chain,
                faucet.as_ref(),
                authority.as_ref(),
                &owner,
                self.flow.sol,
                &targets,
                Funding::TopUp,
            )
            .await?;
            counter!("flow_refills_total").increment(1);
            info!(taker = %owner, lamports = outcome.lamports_sent, minted = ?outcome.minted, "taker inventory refilled");
        }
        Ok(())
    }
}

fn is_transport_error(error: &RpcErrorKind) -> bool {
    matches!(
        error,
        RpcErrorKind::Io(_) | RpcErrorKind::Reqwest(_) | RpcErrorKind::Middleware(_)
    )
}

/// USD to whole micro-USD for the notional counter (counters are integers;
/// whole USD would drop most of a small trade's volume).
fn micro_usd(usd: Decimal) -> u64 {
    (usd * Decimal::from(1_000_000))
        .round()
        .to_u64()
        .unwrap_or_default()
}

/// `(expected - actual) / expected` in basis points (positive = worse).
fn ratio_bps(expected: u64, actual: u64) -> f64 {
    if expected == 0 {
        return 0.0;
    }
    (expected as f64 - actual as f64) / expected as f64 * 10_000.0
}

#[cfg(test)]
mod tests {
    #[test]
    fn tally_counts_outcomes_and_resets_on_take() {
        let tally = Tally::default();
        tally.add(Outcome::Landed.as_str());
        tally.add(Outcome::Landed.as_str());
        tally.add(Outcome::NoPrice.as_str());
        let counts = tally.take();
        assert_eq!(counts.get("landed"), Some(&2));
        assert_eq!(counts.get("no_price"), Some(&1));
        assert!(tally.take().is_empty());
    }

    use super::*;

    #[test]
    fn slippage_is_normalised_by_the_quote() {
        // exact-in: quoted 1000 out, received 990 -> 100 bps worse
        assert!((ratio_bps(1_000, 990) - 100.0).abs() < 1e-9);
        // exact-out: quoted 1000 in, spent 1010 -> 100 bps worse
        assert!((-ratio_bps(1_000, 1_010) - 100.0).abs() < 1e-9);
        assert!(ratio_bps(0, 5).abs() < 1e-9);
    }

    #[test]
    fn micro_usd_rounds_instead_of_truncating() {
        assert_eq!(micro_usd(Decimal::new(123_456_789, 7)), 12_345_679);
        assert_eq!(micro_usd(Decimal::new(4, 1)), 400_000);
        assert_eq!(micro_usd(Decimal::new(-1, 0)), 0);
    }
}
