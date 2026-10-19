//! A scenario described by its parameters (assets, venues, makers, takers,
//! arb bots) instead of a fixed scale: [`Spec`] renders into a config
//! overlay ([`render`]) and estimates the load it offers ([`estimate`]).
//!
//! The named scales (`x10` … `x2000`, `assets`) are [`presets`]; a preset
//! renders to the same overlay `bootstrap/scenario/scale.py` used to write.
//! Overlays layer over the target profile (`MTM_PROFILE=scenario-local,
//! scale-<name>`), so a spec only says what differs from it.

pub mod estimate;
#[cfg(feature = "fetch")]
pub mod prices;
pub mod render;
#[cfg(feature = "validate")]
pub mod validate;

use std::collections::BTreeSet;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Tokens the scenario can run, in overlay order: USDC (always present,
/// every pool's quote and the router's bridge), the base profile's JUP and
/// RAY, then the asset-scale tokens. Each needs a mint keypair in Doppler
/// (`KEYPAIR_TOKEN_<NAME>_MINT`), a Binance `<NAME>USDT` feed and the
/// mainnet mint whose metadata the token bootstrap mirrors.
pub const CATALOG: [(&str, &str); 11] = [
    ("USDC", "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"),
    ("JUP", "JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN"),
    ("RAY", "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R"),
    ("BONK", "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263"),
    ("WIF", "EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm"),
    ("PYTH", "HZ1JovNiVvGrGNiiYvEozEVgZ58xaU3RKwX8eACQBCt3"),
    ("JTO", "jtojtomepa8beP8AuQc6eXt5FriJwfFMwQx2v2f9mCL"),
    ("ORCA", "orcaEKTdK7LKz57vaAYr9QeNsVEPfiu6QeMU1kektZE"),
    ("RENDER", "rndrizKT3MK1iimdxRdWabcF7Zg7AR5T4nud4EkHBof"),
    ("W", "85VBFQZC9TZkfaptBWjvUw7YbZjy52A6mjtPGjstQAmQ"),
    ("TNSR", "TNSRxcUxoT9xBG3de7PiJyTDYu7kskLqcpddxnEJAS6"),
];

/// The tokens the base profiles (`scenario-local`, `scenario-testnet`)
/// already configure, besides USDC.
pub const BASE_TOKENS: [&str; 2] = ["JUP", "RAY"];

/// Most makers of one kind on one pool: each index has its own parameters.
pub const MAX_PER_POOL: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Venue {
    Manifest,
    OrcaWhirlpool,
    RaydiumClmm,
}

impl Venue {
    /// Overlay order.
    pub const ALL: [Venue; 3] = [Venue::Manifest, Venue::OrcaWhirlpool, Venue::RaydiumClmm];

    pub fn as_str(self) -> &'static str {
        match self {
            Venue::Manifest => "manifest",
            Venue::OrcaWhirlpool => "orca-whirlpool",
            Venue::RaydiumClmm => "raydium-clmm",
        }
    }
}

/// A scenario's parameters. Everything the overlay holds follows from these
/// plus prices; serde defaults let a client send only what it changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Spec {
    /// Assets besides USDC (catalog names; rendered in catalog order).
    pub tokens: Vec<String>,
    /// Venues every `<token>/USDC` pair is listed on.
    #[serde(default = "all_venues")]
    pub venues: Vec<Venue>,
    /// Extra Orca pools between two non-USDC tokens, `[base, quote]`.
    #[serde(default)]
    pub crosses: Vec<(String, String)>,
    pub makers: Makers,
    pub takers: Takers,
    #[serde(default)]
    pub arb: Arb,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Makers {
    /// Manifest books: ladders per book (0..=3).
    pub ladders: u8,
    /// Orca / Raydium pools: LP ranges per pool (0..=3).
    pub ranges: u8,
    /// Orca / Raydium pools: oracle pool-arbs per pool (0..=3).
    pub pool_arbs: u8,
    /// Tokens whose pools get at most one maker of each kind (thin books
    /// for newly listed assets).
    #[serde(default)]
    pub thin_tokens: Vec<String>,
    /// Maker inventory multiplier over the base profile's. `None` sizes it
    /// to the flow ([`Spec::auto_capital`]).
    #[serde(default)]
    pub capital: Option<Decimal>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Takers {
    pub count: u32,
    /// Trades per taker per minute (Poisson arrivals).
    #[serde(default = "default_trades_per_min")]
    pub trades_per_min: f64,
    /// Median trade notional in USD (lognormal with `sigma`).
    #[serde(default = "default_median_usd")]
    pub median_usd: f64,
    #[serde(default = "default_sigma")]
    pub sigma: f64,
    #[serde(default = "default_large_probability")]
    pub large_probability: f64,
    #[serde(default = "default_large_multiplier")]
    pub large_multiplier: f64,
    #[serde(default = "default_exact_out_probability")]
    pub exact_out_probability: f64,
    #[serde(default = "default_slippage_bps")]
    pub slippage_bps: u16,
    /// SOL per taker wallet; `None` is 2 below 500 takers, else 0.5.
    #[serde(default)]
    pub sol: Option<Decimal>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Arb {
    /// Cross-venue searchers, cycling through five fee/latency profiles.
    pub bots: u32,
}

fn all_venues() -> Vec<Venue> {
    Venue::ALL.to_vec()
}
fn default_trades_per_min() -> f64 {
    5.0
}
fn default_median_usd() -> f64 {
    150.0
}
fn default_sigma() -> f64 {
    1.0
}
fn default_large_probability() -> f64 {
    0.08
}
fn default_large_multiplier() -> f64 {
    40.0
}
fn default_exact_out_probability() -> f64 {
    0.1
}
fn default_slippage_bps() -> u16 {
    150
}

impl Takers {
    pub fn new(count: u32) -> Self {
        Self {
            count,
            trades_per_min: default_trades_per_min(),
            median_usd: default_median_usd(),
            sigma: default_sigma(),
            large_probability: default_large_probability(),
            large_multiplier: default_large_multiplier(),
            exact_out_probability: default_exact_out_probability(),
            slippage_bps: default_slippage_bps(),
            sol: None,
        }
    }
}

/// One pool the overlay creates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pool {
    pub venue: Venue,
    pub base: String,
    pub quote: String,
}

impl Spec {
    /// Every token, USDC first, in catalog order.
    pub fn all_tokens(&self) -> Vec<&'static str> {
        CATALOG
            .iter()
            .map(|(name, _)| *name)
            .filter(|name| *name == "USDC" || self.tokens.iter().any(|t| t == name))
            .collect()
    }

    /// Tokens outside the base profile's: the overlay then lists the token
    /// set and oracle feeds itself, and bootstrap mints them.
    pub fn extra_tokens(&self) -> Vec<&'static str> {
        self.all_tokens()
            .into_iter()
            .filter(|t| *t != "USDC" && !BASE_TOKENS.contains(t))
            .collect()
    }

    pub fn redefines_tokens(&self) -> bool {
        let base: BTreeSet<&str> = BASE_TOKENS.into_iter().collect();
        let mine: BTreeSet<&str> = self
            .all_tokens()
            .into_iter()
            .filter(|t| *t != "USDC")
            .collect();
        mine != base
    }

    /// `<token>/USDC` on each venue (overlay venue order), then the crosses on Orca.
    pub fn pools(&self) -> Vec<Pool> {
        let mut pools = Vec::new();
        for token in self.all_tokens().into_iter().filter(|t| *t != "USDC") {
            for venue in Venue::ALL.into_iter().filter(|v| self.venues.contains(v)) {
                pools.push(Pool {
                    venue,
                    base: token.to_owned(),
                    quote: "USDC".to_owned(),
                });
            }
        }
        for (base, quote) in &self.crosses {
            pools.push(Pool {
                venue: Venue::OrcaWhirlpool,
                base: base.clone(),
                quote: quote.clone(),
            });
        }
        pools
    }

    /// Makers of each kind on a pool: (ladders, ranges, pool-arbs).
    pub fn makers_on(&self, pool: &Pool) -> (u8, u8, u8) {
        let thin = |n: u8| {
            if self.makers.thin_tokens.contains(&pool.base) {
                n.min(1)
            } else {
                n
            }
        };
        match pool.venue {
            Venue::Manifest => (thin(self.makers.ladders), 0, 0),
            _ => (0, thin(self.makers.ranges), thin(self.makers.pool_arbs)),
        }
    }

    pub fn maker_count(&self) -> u32 {
        self.pools()
            .iter()
            .map(|p| {
                let (l, r, a) = self.makers_on(p);
                u32::from(l) + u32::from(r) + u32::from(a)
            })
            .sum()
    }

    /// Pairs the takers trade: each `<token>/USDC` with a pool, the crosses,
    /// and consecutive extra tokens paired with each other (no pool: the
    /// router bridges them through USDC).
    pub fn pairs(&self) -> Vec<(String, String)> {
        let mut pairs: Vec<(String, String)> = Vec::new();
        if !self.venues.is_empty() {
            for token in self.all_tokens().into_iter().filter(|t| *t != "USDC") {
                pairs.push((token.to_owned(), "USDC".to_owned()));
            }
        }
        pairs.extend(self.crosses.iter().cloned());
        let extra = self.extra_tokens();
        for chunk in extra.chunks_exact(2) {
            pairs.push((chunk[0].to_owned(), chunk[1].to_owned()));
        }
        pairs
    }

    /// Offered swaps per second if every taker kept its Poisson schedule.
    pub fn offered_swaps_per_s(&self) -> f64 {
        f64::from(self.takers.count) * self.takers.trades_per_min / 60.0
    }

    /// Maker capital sized to the flow: the x100 … x2000 presets give ×1 per
    /// 825 offered swaps a minute over the 7 base pools at a $150 median
    /// (×10 at x100, ×150 at x1500). Spread over more pools, each needs
    /// less. Never below ×3, the x10 preset's.
    pub fn auto_capital(&self) -> Decimal {
        let pools = self.pools().len().max(1) as f64;
        let per_min = self.offered_swaps_per_s() * 60.0;
        let capital = per_min / 825.0 * 7.0 / pools * self.takers.median_usd / 150.0;
        Decimal::from(capital.round().max(3.0) as i64)
    }

    pub fn capital(&self) -> Decimal {
        self.makers.capital.unwrap_or_else(|| self.auto_capital())
    }

    pub fn taker_sol(&self) -> Decimal {
        self.takers.sol.unwrap_or(if self.takers.count < 500 {
            Decimal::TWO
        } else {
            Decimal::new(5, 1)
        })
    }

    /// Overlay name for a spec that is not a preset: `c-` and 8 hex digits
    /// of its hash, so composing the same spec twice reuses the file.
    pub fn custom_name(&self) -> String {
        let json = serde_json::to_vec(self).unwrap_or_default();
        let digest = Sha256::digest(&json);
        let hex: String = digest[..4].iter().map(|b| format!("{b:02x}")).collect();
        format!("c-{hex}")
    }

    /// What makes a spec unrenderable.
    pub fn check(&self) -> Result<(), String> {
        let known = |t: &str| CATALOG.iter().any(|(name, _)| *name == t);
        let mut seen = BTreeSet::new();
        for token in &self.tokens {
            if token == "USDC" {
                return Err("tokens: USDC is always included; list the others".into());
            }
            if !known(token) {
                return Err(format!(
                    "tokens: {token} is not in the catalog (each token needs a mint keypair in Doppler and a Binance feed)"
                ));
            }
            if !seen.insert(token.as_str()) {
                return Err(format!("tokens: {token} is listed twice"));
            }
        }
        if self.tokens.is_empty() {
            return Err("tokens: list at least one token besides USDC".into());
        }
        if self.venues.is_empty() && self.crosses.is_empty() {
            return Err("venues: no pools (pick a venue or a cross pool)".into());
        }
        let mut crosses = BTreeSet::new();
        for (base, quote) in &self.crosses {
            for t in [base, quote] {
                if t == "USDC" || !self.tokens.contains(t) {
                    return Err(format!(
                        "crosses: {base}/{quote} needs two listed non-USDC tokens"
                    ));
                }
            }
            if base == quote {
                return Err(format!("crosses: {base}/{quote} pairs a token with itself"));
            }
            let key = if base < quote {
                (base, quote)
            } else {
                (quote, base)
            };
            if !crosses.insert(key) {
                return Err(format!("crosses: {base}/{quote} is listed twice"));
            }
        }
        for t in &self.makers.thin_tokens {
            if !self.tokens.contains(t) {
                return Err(format!("makers.thin_tokens: {t} is not a listed token"));
            }
        }
        let m = &self.makers;
        if m.ladders > MAX_PER_POOL || m.ranges > MAX_PER_POOL || m.pool_arbs > MAX_PER_POOL {
            return Err(format!(
                "makers: at most {MAX_PER_POOL} of each kind per pool"
            ));
        }
        if m.capital.is_some_and(|c| c <= Decimal::ZERO) {
            return Err("makers.capital must be positive".into());
        }
        let t = &self.takers;
        if t.count == 0 {
            return Err("takers.count must be positive".into());
        }
        if !(t.trades_per_min.is_finite() && t.trades_per_min > 0.0 && t.trades_per_min <= 600.0) {
            return Err("takers.trades_per_min must be in (0, 600]".into());
        }
        if !(t.median_usd.is_finite() && t.median_usd > 0.0) {
            return Err("takers.median_usd must be positive".into());
        }
        if self.arb.bots > 1_000 {
            return Err("arb.bots: at most 1000".into());
        }
        if self.pairs().is_empty() {
            return Err("takers: no pairs to trade".into());
        }
        Ok(())
    }

    /// The spec in canonical form: tokens in catalog order, venues in
    /// overlay order, so equal scenarios hash alike.
    pub fn normalized(mut self) -> Self {
        let order = |t: &String| {
            CATALOG
                .iter()
                .position(|(name, _)| name == t)
                .unwrap_or(usize::MAX)
        };
        self.tokens.sort_by_key(order);
        self.venues.sort();
        self.venues.dedup();
        self.makers.thin_tokens.sort_by_key(order);
        self.makers.thin_tokens.dedup();
        self
    }
}

/// The named scales: `x10` … `x2000` grow takers by an order of magnitude
/// and makers slowly (their capital grows instead); `assets` is x100's
/// load over all 11 tokens, with one maker of each kind on the new ones.
pub fn presets() -> Vec<(&'static str, Spec)> {
    let base = |takers: u32, bots: u32, density: u8, capital: i64| Spec {
        tokens: BASE_TOKENS.iter().map(|t| (*t).to_owned()).collect(),
        venues: all_venues(),
        crosses: vec![("JUP".to_owned(), "RAY".to_owned())],
        makers: Makers {
            ladders: density,
            ranges: density,
            pool_arbs: density,
            thin_tokens: Vec::new(),
            capital: Some(Decimal::from(capital)),
        },
        takers: Takers::new(takers),
        arb: Arb { bots },
    };
    let new: Vec<String> = CATALOG[3..].iter().map(|(t, _)| (*t).to_owned()).collect();
    let mut assets = base(1650, 14, 3, 10);
    assets.tokens.extend(new.iter().cloned());
    assets
        .crosses
        .extend(new.iter().map(|t| (t.clone(), "JUP".to_owned())));
    assets.makers.thin_tokens = new;
    vec![
        ("x10", base(140, 6, 2, 3)),
        ("x100", base(1650, 14, 3, 10)),
        ("assets", assets),
        ("x200", base(3300, 20, 3, 20)),
        ("x500", base(8250, 30, 3, 50)),
        ("x750", base(12375, 35, 3, 75)),
        ("x1000", base(16500, 40, 3, 100)),
        ("x1500", base(24750, 45, 3, 150)),
        ("x2000", base(33000, 50, 3, 200)),
    ]
}

pub fn preset(name: &str) -> Option<Spec> {
    presets()
        .into_iter()
        .find(|(n, _)| *n == name)
        .map(|(_, s)| s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_are_valid() {
        for (name, spec) in presets() {
            spec.check().unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(
                spec.clone().normalized(),
                spec,
                "{name} is not in canonical form"
            );
        }
    }

    #[test]
    fn x1500_counts_match_the_old_overlay() {
        // config/scale-x1500.toml: 24750 takers, 45 bots, 36 makers, 3 assets, 7 pools.
        let spec = preset("x1500").expect("preset");
        assert_eq!(spec.pools().len(), 7);
        assert_eq!(spec.maker_count(), 36);
        let assets = preset("assets").expect("preset");
        assert_eq!(assets.pools().len(), 39);
        assert_eq!(assets.all_tokens().len(), 11);
    }

    #[test]
    fn auto_capital_tracks_the_presets() {
        for (name, spec) in presets() {
            if name == "assets" || name == "x10" {
                continue;
            }
            let mut auto = spec.clone();
            auto.makers.capital = None;
            assert_eq!(auto.capital(), spec.capital(), "{name}");
        }
        let mut small = preset("x10").expect("preset");
        small.makers.capital = None;
        assert_eq!(small.capital(), Decimal::from(3));
    }

    #[test]
    fn bad_specs_are_refused() {
        let ok = preset("x10").expect("preset");
        let broken: [fn(&mut Spec); 7] = [
            |s| s.tokens.push("DOGE".into()),
            |s| s.tokens.push("JUP".into()),
            |s| s.tokens.push("USDC".into()),
            |s| s.crosses.push(("JUP".into(), "USDC".into())),
            |s| s.makers.ladders = 4,
            |s| s.takers.count = 0,
            |s| s.makers.thin_tokens.push("BONK".into()),
        ];
        for breaker in broken {
            let mut spec = ok.clone();
            breaker(&mut spec);
            assert!(spec.check().is_err(), "{spec:?}");
        }
    }

    #[test]
    fn custom_names_are_stable() {
        let spec = preset("x10").expect("preset");
        assert_eq!(spec.custom_name(), spec.clone().custom_name());
        let mut other = spec.clone();
        other.takers.count += 1;
        assert_ne!(spec.custom_name(), other.custom_name());
    }
}
