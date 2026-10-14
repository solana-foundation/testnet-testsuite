//! Uninformed flow: random takers swapping through the router.
//!
//! Arrivals are Poisson (`rate_per_min`), notionals lognormal around
//! `median_usd` with occasional large blocks (`large_probability` ×
//! `large_multiplier`), pairs and directions random but constrained by what
//! the taker holds. Each trade asks the router for a transaction, signs it as
//! the taker, submits it, and measures landed versus quoted (slippage) and
//! versus the oracle (cost to the uninformed trader). Takers whose inventory
//! runs low are minted back up (test tokens only). The RNG is seeded, so a
//! run's trade sequence is replayable.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use metrics::{counter, histogram};
use mtm_chain::ChainClient;
use mtm_common::config::RpcConfig;
use mtm_dex_harness::cli::signer_from_environment;
use mtm_scenario::funding::{Funding, token_balance, top_up};
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
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
use tracing::{info, warn};

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
    // them all on one core (and the signer types are not Send, so they
    // cannot be spawned onto the multi-threaded runtime instead).
    let shards = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(takers.len());
    info!(
        takers = takers.len(),
        shards,
        rate_per_min = flow.rate_per_min,
        "flow started"
    );
    let rpc = &cfg.rpc;
    let (scenario, flow, takers) = (&scenario, &flow, &takers);
    tokio::task::block_in_place(|| {
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..shards)
                .map(|shard| {
                    let refill = refill.clone();
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
}

struct Trade<'t> {
    taker: usize,
    input: &'t Token,
    output: &'t Token,
    notional: f64,
    exact_out: bool,
}

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
            .map(|(index, taker)| self.taker_loop(index, taker, takers.len()));
        for result in futures::future::join_all(loops).await {
            result?;
        }
        Ok(())
    }

    async fn taker_loop(&self, index: usize, taker: &Keypair, takers: usize) -> Result<()> {
        let mut rng = ChaCha8Rng::seed_from_u64(self.flow.rng_seed.wrapping_add(index as u64));
        let arrivals =
            Exp::new(self.flow.rate_per_min / 60.0 / takers as f64).context("rate_per_min")?;
        let size = LogNormal::new(self.flow.median_usd.ln(), self.flow.sigma).context("sigma")?;
        let unit = Uniform::new(0.0_f64, 1.0).context("uniform")?;
        let pairs = self.flow.pairs.len();
        loop {
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
            match self.trade(taker, &trade).await {
                Ok(()) => counter!("flow_swaps_total", "pair" => label, "status" => "landed", "size" => size_label).increment(1),
                Err(error) => {
                    warn!(taker = index, %label, error = %format!("{error:#}"), "trade failed");
                    counter!("flow_swaps_total", "pair" => label, "status" => "failed", "size" => size_label).increment(1);
                }
            }
            if let Err(error) = self.refill(taker).await {
                warn!(taker = index, error = %format!("{error:#}"), "refill failed");
            }
        }
    }

    async fn trade(&self, taker: &Keypair, trade: &Trade<'_>) -> Result<()> {
        let prices = PriceBook::fetch(&self.oracle, self.scenario.config.max_price_age_ms).await?;
        let owner = taker.pubkey();
        // Sell whichever side of the pair the taker can afford.
        let (mut input, mut output) = (trade.input, trade.output);
        let notional = Decimal::from_f64(trade.notional).context("notional")?;
        let affordable = |token: &Token, balance: u64| -> Result<bool> {
            Ok(to_ui(token, balance) * prices.usd(token)? >= notional * Decimal::new(105, 2))
        };
        if !affordable(input, token_balance(&self.chain, &owner, input).await?)? {
            std::mem::swap(&mut input, &mut output);
            if !affordable(input, token_balance(&self.chain, &owner, input).await?)? {
                bail!("taker {} cannot afford ${notional}", trade.taker);
            }
        }
        let (mode, amount) = if trade.exact_out {
            (
                SwapMode::ExactOut,
                to_atoms(output, notional / prices.usd(output)?),
            )
        } else {
            (
                SwapMode::ExactIn,
                to_atoms(input, notional / prices.usd(input)?),
            )
        };
        let amount = amount.filter(|a| *a > 0).context("trade rounds to zero")?;
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
            .context("router quote")?;
        let bytes = base64::engine::general_purpose::STANDARD.decode(
            quote
                .tx_base64
                .as_deref()
                .context("router returned no transaction")?,
        )?;
        let unsigned: VersionedTransaction = wincode::deserialize(&bytes)?;
        let signed = VersionedTransaction::try_new(unsigned.message, &[taker])?;
        let in_before = token_balance(&self.chain, &owner, input).await?;
        let out_before = token_balance(&self.chain, &owner, output).await?;
        let signature = self
            .chain
            .rpc()
            .send_and_confirm_transaction(&signed)
            .await
            .context("submitting routed swap")?;
        let spent = in_before.saturating_sub(token_balance(&self.chain, &owner, input).await?);
        let received = token_balance(&self.chain, &owner, output)
            .await?
            .saturating_sub(out_before);

        let pair = format!("{}->{}", input.name, output.name);
        let spent_usd = to_ui(input, spent) * prices.usd(input)?;
        let received_usd = to_ui(output, received) * prices.usd(output)?;
        let cost_bps = if spent_usd.is_zero() {
            Decimal::ZERO
        } else {
            (spent_usd - received_usd) / spent_usd * Decimal::from(10_000)
        };
        // Exact-in slippage: landed output short of the quote; exact-out: input above it.
        let slippage_bps = if trade.exact_out {
            ratio_bps(spent, quote.in_amount)
        } else {
            ratio_bps(quote.out_amount, received)
        };
        histogram!("flow_cost_vs_oracle_bps", "pair" => pair.clone())
            .record(cost_bps.to_f64().unwrap_or_default());
        histogram!("flow_slippage_bps", "pair" => pair.clone()).record(slippage_bps);
        histogram!("flow_route_legs").record(quote.route.len() as f64);
        counter!("flow_notional_usd_total", "pair" => pair.clone())
            .increment(spent_usd.to_u64().unwrap_or_default());
        for leg in &quote.route {
            counter!("flow_route_legs_total", "venue" => leg.venue.clone()).increment(1);
        }
        info!(
            taker = trade.taker, %pair, mode = ?mode, %spent_usd, %received_usd, %cost_bps, slippage_bps,
            legs = quote.route.len(), %signature, "routed swap landed"
        );
        Ok(())
    }

    async fn refill(&self, taker: &Keypair) -> Result<()> {
        let Some((faucet, authority)) = &self.refill else {
            return Ok(());
        };
        let owner = taker.pubkey();
        let mut low = false;
        let mut targets = Vec::new();
        for (name, amount) in &self.flow.inventory {
            let token = self.scenario.token(name)?;
            let balance = to_ui(token, token_balance(&self.chain, &owner, token).await?);
            low |= balance < *amount * self.flow.refill_below_ratio;
            targets.push((token, *amount));
        }
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
            info!(taker = %owner, minted = ?outcome.minted, "taker inventory refilled");
        }
        Ok(())
    }
}

/// `(expected - actual) / expected` in basis points (positive = worse).
fn ratio_bps(expected: u64, actual: u64) -> f64 {
    if expected == 0 {
        return 0.0;
    }
    (expected as f64 - actual as f64) / expected as f64 * 10_000.0
}
