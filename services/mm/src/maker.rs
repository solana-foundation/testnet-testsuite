//! Per-maker context, the tick loop, and the metrics every strategy shares.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use metrics::{counter, gauge, histogram};
use mtm_chain::ChainClient;
use mtm_scenario::funding::token_balance;
use mtm_scenario::pricing::PriceBook;
use mtm_scenario::venue::to_ui;
use mtm_scenario::{MakerConfig, Scenario, Strategy, Token};
use oracle_client::OracleClient;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use tracing::warn;

use crate::{arb, clmm, clob};

pub struct Ctx {
    pub chain: ChainClient,
    pub scenario: Arc<Scenario>,
    pub maker: MakerConfig,
    pub wallet: Arc<Keypair>,
    pub base: Token,
    pub quote: Token,
    pub pool: Pubkey,
    pub oracle: OracleClient,
    pub state_dir: PathBuf,
}

impl Ctx {
    pub async fn new(
        chain: ChainClient,
        scenario: Arc<Scenario>,
        maker: MakerConfig,
        state_dir: PathBuf,
    ) -> Result<Self> {
        let pool = scenario
            .pool_address(&chain, maker.venue, &maker.base, &maker.quote)
            .await?
            .with_context(|| {
                format!(
                    "maker {}: no {} pool for {}/{}; run scenario bootstrap",
                    maker.name,
                    maker.venue.as_str(),
                    maker.base,
                    maker.quote
                )
            })?;
        Ok(Self {
            wallet: Arc::new(scenario.maker_wallet(&maker.name)),
            base: scenario.token(&maker.base)?.clone(),
            quote: scenario.token(&maker.quote)?.clone(),
            oracle: OracleClient::new(&scenario.config.oracle_url),
            pool,
            chain,
            scenario,
            maker,
            state_dir,
        })
    }

    pub fn wallet_pubkey(&self) -> Pubkey {
        self.wallet.pubkey()
    }

    pub fn name(&self) -> String {
        self.maker.name.clone()
    }

    pub async fn prices(&self) -> Result<PriceBook> {
        PriceBook::fetch(&self.oracle, self.scenario.config.max_price_age_ms).await
    }

    /// Wallet balance of one of the maker's tokens, in atoms.
    pub async fn wallet_balance(&self, token: &Token) -> Result<u64> {
        token_balance(&self.chain, &self.wallet_pubkey(), token).await
    }

    /// Publish wallet (plus any venue-held) inventory and its USD value.
    pub fn record_inventory(&self, prices: &PriceBook, base_atoms: u64, quote_atoms: u64) {
        let maker = self.name();
        let base = to_ui(&self.base, base_atoms);
        let quote = to_ui(&self.quote, quote_atoms);
        gauge!("mm_inventory", "maker" => maker.clone(), "token" => self.base.name.clone())
            .set(base.to_f64().unwrap_or_default());
        gauge!("mm_inventory", "maker" => maker.clone(), "token" => self.quote.name.clone())
            .set(quote.to_f64().unwrap_or_default());
        if let (Ok(base_usd), Ok(quote_usd)) = (prices.usd(&self.base), prices.usd(&self.quote)) {
            gauge!("mm_inventory_value_usd", "maker" => maker).set(
                (base * base_usd + quote * quote_usd)
                    .to_f64()
                    .unwrap_or_default(),
            );
        }
    }

    /// Publish the venue price next to the oracle price and their gap.
    pub fn record_prices(&self, oracle: Decimal, venue: Option<Decimal>) {
        let maker = self.name();
        let pair = format!("{}/{}", self.base.name, self.quote.name);
        gauge!("mm_oracle_price", "pair" => pair).set(oracle.to_f64().unwrap_or_default());
        if let Some(venue) = venue {
            gauge!("mm_venue_price", "maker" => maker.clone(), "venue" => self.maker.venue.as_str())
                .set(venue.to_f64().unwrap_or_default());
            gauge!("mm_deviation_bps", "maker" => maker, "venue" => self.maker.venue.as_str())
                .set(deviation_bps(venue, oracle).to_f64().unwrap_or_default());
        }
    }

    pub fn action(&self, action: &'static str) {
        counter!("mm_actions_total", "maker" => self.name(), "venue" => self.maker.venue.as_str(), "action" => action)
            .increment(1);
    }
}

/// `(venue - oracle) / oracle` in basis points.
pub fn deviation_bps(venue: Decimal, oracle: Decimal) -> Decimal {
    if oracle.is_zero() {
        return Decimal::ZERO;
    }
    (venue - oracle) / oracle * Decimal::from(10_000)
}

enum State {
    Clob(Box<clob::Clob>),
    Clmm(Box<clmm::Clmm>),
    Arb(arb::Arb),
}

/// Tick forever. Errors are logged and counted; the next tick retries.
pub async fn run(ctx: Ctx) {
    let mut state = loop {
        let built = match &ctx.maker.strategy {
            Strategy::ClobLadder { .. } => clob::Clob::start(&ctx)
                .await
                .map(|s| State::Clob(Box::new(s))),
            Strategy::ClmmRange { .. } => clmm::Clmm::start(&ctx).map(|s| State::Clmm(Box::new(s))),
            Strategy::PoolArb { .. } => Ok(State::Arb(arb::Arb)),
        };
        match built {
            Ok(state) => break state,
            Err(error) => {
                warn!(maker = %ctx.maker.name, error = %format!("{error:#}"), "maker setup failed; retrying");
                counter!("mm_errors_total", "maker" => ctx.name(), "stage" => "setup").increment(1);
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    };
    let mut ticker = tokio::time::interval(Duration::from_millis(ctx.maker.interval_ms));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let started = Instant::now();
        // A panic in venue code must not end this maker: count it and retry.
        let tick = async {
            match &mut state {
                State::Clob(clob) => clob.tick(&ctx).await,
                State::Clmm(clmm) => clmm.tick(&ctx).await,
                State::Arb(arb) => arb.tick(&ctx).await,
            }
        };
        let result = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(tick))
            .await
            .unwrap_or_else(|_| {
                counter!("mm_panics_total", "maker" => ctx.name()).increment(1);
                Err(anyhow::anyhow!("maker tick panicked"))
            });
        histogram!("mm_tick_seconds", "maker" => ctx.name())
            .record(started.elapsed().as_secs_f64());
        if let Err(error) = result {
            warn!(maker = %ctx.maker.name, error = %format!("{error:#}"), "maker tick failed");
            counter!("mm_errors_total", "maker" => ctx.name(), "stage" => "tick").increment(1);
        }
    }
}
