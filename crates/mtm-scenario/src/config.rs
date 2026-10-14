//! The `[scenario]` section of a layered config profile
//! (`config/scenario-local.toml`, `config/scenario-testnet.toml`).
//!
//! Addresses never live in the file: tokens and programs name the
//! environment variables (Doppler secrets) that hold them, so one profile
//! replays against any deployment.

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct ScenarioConfig {
    /// Env var holding the secret that every maker and taker wallet is
    /// derived from. Unset falls back to `dev_seed` (local only).
    #[serde(default = "default_seed_env")]
    pub seed_env: String,
    #[serde(default)]
    pub dev_seed: Option<String>,
    #[serde(default = "default_oracle_url")]
    pub oracle_url: String,
    #[serde(default = "default_router_url")]
    pub router_url: String,
    /// Oracle prices older than this are treated as missing.
    #[serde(default = "default_max_price_age_ms")]
    pub max_price_age_ms: u64,
    pub programs: ProgramsConfig,
    pub tokens: Vec<TokenConfig>,
    pub pools: Vec<PoolConfig>,
    #[serde(default)]
    pub makers: Vec<MakerConfig>,
    #[serde(default)]
    pub flow: Option<FlowConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProgramsConfig {
    /// Env var holding the Raydium CLMM program keypair or public key.
    pub raydium_clmm: String,
    pub orca_whirlpool: String,
    /// Env var holding the WhirlpoolsConfig keypair or public key.
    pub orca_whirlpools_config: String,
    pub manifest: String,
    #[serde(default)]
    pub raydium_amm_config_index: u16,
    #[serde(default = "default_orca_tick_spacing")]
    pub orca_tick_spacing: u16,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenConfig {
    pub name: String,
    /// Env var holding the mint public key.
    pub mint_env: String,
    /// Oracle symbol quoting this token in USD.
    pub oracle_symbol: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum Venue {
    RaydiumClmm,
    OrcaWhirlpool,
    Manifest,
}

impl Venue {
    pub fn as_str(self) -> &'static str {
        match self {
            Venue::RaydiumClmm => "raydium-clmm",
            Venue::OrcaWhirlpool => "orca-whirlpool",
            Venue::Manifest => "manifest",
        }
    }
}

/// One pool or market the scenario keeps alive.
#[derive(Debug, Clone, Deserialize)]
pub struct PoolConfig {
    pub venue: Venue,
    pub base: String,
    pub quote: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MakerConfig {
    pub name: String,
    pub venue: Venue,
    pub base: String,
    pub quote: String,
    pub strategy: Strategy,
    /// Target wallet balances (UI units by token name), topped up by bootstrap.
    pub inventory: BTreeMap<String, Decimal>,
    /// SOL kept in the wallet for fees and rent.
    #[serde(default = "default_sol")]
    pub sol: Decimal,
    #[serde(default = "default_interval_ms")]
    pub interval_ms: u64,
    /// Transaction settings for this maker's venue client; unset fields keep
    /// the strategy's defaults. A priority fee keeps quotes and re-centres
    /// landing in full blocks, where fee-less makers lose to taker flow.
    #[serde(default)]
    pub execution: MakerExecution,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct MakerExecution {
    /// Venue slippage tolerance (LP deposits/withdrawals, swaps).
    pub slippage_bps: Option<u16>,
    pub compute_unit_limit: Option<u32>,
    #[serde(default)]
    pub priority_fee_micro_lamports: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Strategy {
    /// Manifest: a ladder of resting bids and asks around the oracle mid,
    /// re-quoted when the mid moves, skewed toward a target inventory.
    ClobLadder {
        levels: u8,
        /// Distance of the first level from the (skewed) mid.
        spread_bps: u32,
        /// Extra distance per further level.
        step_bps: u32,
        /// Base UI units per level (capped by seat inventory).
        level_size: Decimal,
        /// Re-quote once the mid moves this far from the quoted mid.
        refresh_bps: u32,
        /// Mid shift at a fully one-sided inventory (0 = no skew).
        #[serde(default)]
        skew_bps: u32,
        /// Target share of seat value held in base.
        #[serde(default = "default_target_ratio")]
        target_base_ratio: Decimal,
    },
    /// Raydium CLMM / Orca: one concentrated position centred on the pool
    /// price, re-centred when the price leaves the inner band.
    ClmmRange {
        width_bps: u32,
        recenter_bps: u32,
        /// USD value deposited into each position (capped by the wallet).
        position_usd: Decimal,
    },
    /// Any venue: swap against the pool whenever its price strays from the
    /// oracle by more than `threshold_bps`, pushing it back toward oracle.
    PoolArb {
        threshold_bps: u32,
        /// Largest single trade, in USD.
        max_trade_usd: Decimal,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct FlowConfig {
    pub takers: u32,
    /// Mean arrivals per minute across all takers (Poisson).
    pub rate_per_min: f64,
    /// Median trade notional in USD (lognormal).
    pub median_usd: f64,
    pub sigma: f64,
    /// Probability that a trade is a large block.
    #[serde(default)]
    pub large_probability: f64,
    #[serde(default = "default_large_multiplier")]
    pub large_multiplier: f64,
    #[serde(default)]
    pub exact_out_probability: f64,
    #[serde(default = "default_slippage_bps")]
    pub slippage_bps: u16,
    /// Pairs traded, by token name.
    pub pairs: Vec<(String, String)>,
    pub inventory: BTreeMap<String, Decimal>,
    #[serde(default = "default_sol")]
    pub sol: Decimal,
    /// Mint taker inventory back up when a token falls below this share of
    /// its target (requires the mint authority; test tokens only).
    #[serde(default = "default_refill_ratio")]
    pub refill_below_ratio: Decimal,
    #[serde(default)]
    pub rng_seed: u64,
}

fn default_seed_env() -> String {
    "SCENARIO_SEED".to_owned()
}
fn default_oracle_url() -> String {
    "http://127.0.0.1:8080".to_owned()
}
fn default_router_url() -> String {
    "http://127.0.0.1:8081".to_owned()
}
fn default_max_price_age_ms() -> u64 {
    30_000
}
fn default_orca_tick_spacing() -> u16 {
    64
}
impl MakerConfig {
    /// Parameter ranges a strategy needs to make sense.
    pub fn validate(&self) -> anyhow::Result<()> {
        let fail = |reason: &str| anyhow::bail!("maker {}: {reason}", self.name);
        if self.execution.slippage_bps.is_some_and(|bps| bps > 10_000) {
            return fail("execution.slippage_bps cannot exceed 10000");
        }
        match &self.strategy {
            Strategy::ClobLadder {
                levels,
                target_base_ratio,
                ..
            } => {
                if *levels == 0 {
                    return fail("clob-ladder needs at least one level");
                }
                if *target_base_ratio < Decimal::ZERO || *target_base_ratio > Decimal::ONE {
                    return fail("target_base_ratio must be between 0 and 1");
                }
            }
            Strategy::ClmmRange {
                width_bps,
                recenter_bps,
                ..
            } => {
                // Past half the width the price leaves the range before a
                // re-centre triggers, and the position sits out of range.
                if u64::from(*recenter_bps) * 2 >= u64::from(*width_bps) {
                    return fail("recenter_bps must be below half of width_bps");
                }
            }
            Strategy::PoolArb { .. } => {}
        }
        Ok(())
    }
}

fn default_sol() -> Decimal {
    Decimal::TWO
}
fn default_interval_ms() -> u64 {
    3_000
}
fn default_target_ratio() -> Decimal {
    Decimal::new(5, 1)
}
fn default_large_multiplier() -> f64 {
    50.0
}
fn default_slippage_bps() -> u16 {
    100
}
fn default_refill_ratio() -> Decimal {
    Decimal::new(25, 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn maker(strategy: Strategy) -> MakerConfig {
        MakerConfig {
            name: "m".to_owned(),
            venue: Venue::OrcaWhirlpool,
            base: "A".to_owned(),
            quote: "B".to_owned(),
            strategy,
            inventory: BTreeMap::new(),
            sol: Decimal::ONE,
            interval_ms: 1_000,
            execution: MakerExecution::default(),
        }
    }

    fn range(width_bps: u32, recenter_bps: u32) -> Strategy {
        Strategy::ClmmRange {
            width_bps,
            recenter_bps,
            position_usd: Decimal::ONE,
        }
    }

    #[test]
    fn recenter_must_trigger_inside_the_range() {
        assert!(maker(range(600, 200)).validate().is_ok());
        assert!(maker(range(600, 300)).validate().is_err());
    }

    #[test]
    fn ladder_parameters_are_checked() {
        let ladder = |levels, ratio| Strategy::ClobLadder {
            levels,
            spread_bps: 10,
            step_bps: 5,
            level_size: Decimal::ONE,
            refresh_bps: 10,
            skew_bps: 0,
            target_base_ratio: ratio,
        };
        assert!(maker(ladder(3, Decimal::new(5, 1))).validate().is_ok());
        assert!(maker(ladder(0, Decimal::new(5, 1))).validate().is_err());
        assert!(maker(ladder(3, Decimal::new(15, 1))).validate().is_err());
    }

    #[test]
    fn execution_overrides_are_bounded() {
        let mut m = maker(range(600, 200));
        m.execution.slippage_bps = Some(10_001);
        assert!(m.validate().is_err());
    }
}
