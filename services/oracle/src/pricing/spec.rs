//! Instrument registry config types. Design: docs/instrument-pricing.md.

use mtm_common::Symbol;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct InstrumentConfig {
    /// Instrument key. Defaults to `mint` — for custom testnet tokens the
    /// symbol IS the mint address (avoids duplicate-ticker confusion).
    pub symbol: Option<Symbol>,
    /// Testnet mint address (base58), when the instrument is a real token.
    pub mint: Option<String>,
    /// Output quantization exponent (`mantissa * 10^expo`).
    #[serde(default = "default_expo")]
    pub expo: i32,
    pub base: BaseSourceConfig,
    #[serde(default)]
    pub transforms: Vec<TransformConfig>,
}

fn default_expo() -> i32 {
    -8
}

impl InstrumentConfig {
    pub fn resolved_symbol(&self) -> Option<Symbol> {
        self.symbol
            .clone()
            .or_else(|| self.mint.as_ref().map(Symbol::new))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BaseSourceConfig {
    /// Passthrough of a real feed (1:1 majors).
    Underlying { feed: Symbol },
    /// `base_feed / quote_feed`.
    Cross { base: Symbol, quote: Symbol },
    /// Weighted sum of feeds, optionally rebased so the first evaluation
    /// equals `rebase`.
    Basket {
        legs: Vec<BasketLeg>,
        rebase: Option<String>,
    },
    /// Constant target (test stablecoins) — pair with a noise transform.
    Peg { target: String },
    /// Geometric Brownian motion on its own clock; fully synthetic.
    Gbm {
        initial: String,
        daily_vol_bps: u32,
        #[serde(default)]
        daily_drift_bps: i32,
        seed: u64,
        #[serde(default = "default_tick_ms")]
        tick_ms: u64,
    },
}

fn default_tick_ms() -> u64 {
    1000
}

#[derive(Debug, Clone, Deserialize)]
pub struct BasketLeg {
    pub feed: Symbol,
    pub weight: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TransformConfig {
    Scale {
        factor: String,
        offset: Option<String>,
    },
    Invert,
    Beta {
        beta: f64,
        anchor: String,
        initial: String,
    },
    Lag { ms: u64 },
    Noise {
        sigma_bps: u32,
        halflife_s: u64,
        seed: u64,
    },
}

impl BaseSourceConfig {
    pub fn kind_name(&self) -> &'static str {
        match self {
            BaseSourceConfig::Underlying { .. } => "underlying",
            BaseSourceConfig::Cross { .. } => "cross",
            BaseSourceConfig::Basket { .. } => "basket",
            BaseSourceConfig::Peg { .. } => "peg",
            BaseSourceConfig::Gbm { .. } => "gbm",
        }
    }
}

impl TransformConfig {
    pub fn kind_name(&self) -> &'static str {
        match self {
            TransformConfig::Scale { .. } => "scale",
            TransformConfig::Invert => "invert",
            TransformConfig::Beta { .. } => "beta",
            TransformConfig::Lag { .. } => "lag",
            TransformConfig::Noise { .. } => "noise",
        }
    }
}
