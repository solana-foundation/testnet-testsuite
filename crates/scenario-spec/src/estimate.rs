//! What a [`Spec`] should put on chain: expected transactions per second by
//! actor, compute per block, the hottest pool, and which limit gives first.
//!
//! The model:
//! - **Takers** run closed loops: wait (Poisson, rate λ), then trade and
//!   wait for confirmation (d seconds), so each offers λ / (1 + λ·d) trades a
//!   second, less the share skipped as unaffordable. d is the router quote,
//!   balance reads and settle time, which grows with slot time.
//! - **Makers**: Manifest ladders requote on most ticks under heavy flow;
//!   LP ranges and pool-arbs send a few hundredths of a transaction a second.
//! - **Arb bots** trade on opportunities the flow creates: small.
//! - **Venue shares** are emergent (the router picks the best price) and
//!   calibrated; a cross pair trades on its Orca pool, a pair without a
//!   pool bridges through USDC (2 legs).
//!
//! [`Calibration::default`] is fitted to the 2026-10-17 x1500 runs on
//! solana-test-validator (420 ms and 186 ms slots); refit it from a measured
//! run as the scenario changes.

use serde::{Deserialize, Serialize};

use crate::{Spec, Venue};

/// The chain the scenario runs on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chain {
    pub name: String,
    pub slot_ms: f64,
    /// Block compute limit (SIMD-0286: 100M).
    pub block_cu_limit: f64,
    /// Compute any one writable account may take per block (SIMD-0306: 40% of the block).
    pub account_cu_limit: f64,
    /// Compute a local validator executes per second of slot time on this
    /// host; `None` on a shared cluster.
    pub host_cu_per_s: Option<f64>,
    /// Shared cluster: other users' traffic competes for the same blocks.
    pub shared: bool,
}

impl Chain {
    /// solana-test-validator with 1024-slot epochs (200 ms slots after the first).
    pub fn local() -> Self {
        Self {
            name: "local (200 ms slots)".into(),
            slot_ms: 190.0,
            block_cu_limit: 100e6,
            account_cu_limit: 40e6,
            host_cu_per_s: Some(147e6),
            shared: false,
        }
    }

    /// solana-test-validator on the default epoch (runs before 2026-10-17).
    pub fn local_400ms() -> Self {
        Self {
            name: "local (420 ms slots)".into(),
            slot_ms: 423.0,
            ..Self::local()
        }
    }

    pub fn testnet() -> Self {
        Self {
            name: "testnet".into(),
            slot_ms: 200.0,
            block_cu_limit: 100e6,
            account_cu_limit: 40e6,
            host_cu_per_s: None,
            shared: true,
        }
    }

    pub fn presets() -> Vec<Chain> {
        vec![Self::local(), Self::local_400ms(), Self::testnet()]
    }
}

/// Measured constants the model scales from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Calibration {
    /// Per-trade time outside settlement: quote, balance reads, signing (s).
    pub trade_overhead_s: f64,
    /// Settle time = fixed + slots × slot time (s): 0.469 s at 423 ms, 0.411 s at 186 ms.
    pub settle_fixed_s: f64,
    pub settle_slots: f64,
    /// Share of trades skipped because the taker cannot afford them.
    pub unaffordable_ratio: f64,
    /// Compute per swap leg (CU).
    pub cu_per_leg: f64,
    /// Share of Manifest ladder ticks that send a requote.
    pub ladder_requote_ratio: f64,
    /// Transactions per second per LP range or pool-arb maker.
    pub amm_maker_tx_per_s: f64,
    /// Transactions per second per arb bot.
    pub arb_bot_tx_per_s: f64,
    /// Share of `<token>/USDC` legs each venue wins (renormalized over the venues listed).
    pub venue_share: Vec<(Venue, f64)>,
    /// p99 block compute over the mean: bursts reach a limit before the average does.
    pub burst: f64,
    /// Wallet top-ups in flight during bootstrap (`SCENARIO_FUNDING_CONCURRENCY`).
    pub funding_concurrency: f64,
}

impl Default for Calibration {
    fn default() -> Self {
        Self {
            trade_overhead_s: 0.14,
            settle_fixed_s: 0.365,
            settle_slots: 0.245,
            unaffordable_ratio: 0.008,
            cu_per_leg: 52_500.0,
            ladder_requote_ratio: 0.65,
            amm_maker_tx_per_s: 0.0012,
            arb_bot_tx_per_s: 0.0007,
            venue_share: vec![
                (Venue::RaydiumClmm, 0.77),
                (Venue::Manifest, 0.22),
                (Venue::OrcaWhirlpool, 0.01),
            ],
            burst: 1.4,
            funding_concurrency: 32.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Limit {
    /// The local validator's execution rate.
    Host,
    Block,
    /// The busiest pool's per-account compute.
    Account,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Headroom {
    pub limit: Limit,
    /// Mean load over the limit (1 = saturated on average).
    pub utilisation: f64,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VenueLoad {
    pub venue: Venue,
    pub legs_per_s: f64,
    pub share: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PoolLoad {
    pub pool: String,
    pub legs_per_s: f64,
    pub cu_per_block: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bootstrap {
    pub transactions: f64,
    pub seconds: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sol {
    pub fees_per_hour: f64,
    /// Token-account rent across every wallet.
    pub rent: f64,
    /// SOL the wallets are funded with.
    pub float: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Estimate {
    pub chain: Chain,
    pub takers: u32,
    pub makers: u32,
    pub bots: u32,
    pub participants: u32,
    pub pools: u32,
    pub capital: String,
    /// Takers × trades per minute / 60.
    pub offered_swaps_per_s: f64,
    /// Seconds per trade (overhead + settle).
    pub trade_seconds: f64,
    pub taker_tx_per_s: f64,
    pub maker_tx_per_s: f64,
    pub arb_tx_per_s: f64,
    /// Expected transactions per second, before any limit.
    pub tps: f64,
    /// Expected transactions per second once the tightest limit applies.
    pub tps_capped: f64,
    pub cu_per_s: f64,
    pub cu_per_block: f64,
    pub headroom: Vec<Headroom>,
    /// The limit that gives first, when one is over 1.
    pub bottleneck: Option<Limit>,
    pub venues: Vec<VenueLoad>,
    /// Busiest pools first.
    pub hottest: Vec<PoolLoad>,
    pub bootstrap: Bootstrap,
    pub sol: Sol,
    pub warnings: Vec<String>,
}

const LAMPORTS_PER_SOL: f64 = 1e9;
const SIGNATURE_FEE_LAMPORTS: f64 = 5_000.0;
const TOKEN_ACCOUNT_RENT_SOL: f64 = 0.002_039_28;

fn f(d: rust_decimal::Decimal) -> f64 {
    rust_decimal::prelude::ToPrimitive::to_f64(&d).unwrap_or(0.0)
}

/// Per-taker trades per second at arrival rate `lambda` (1/s).
fn closed_loop(lambda: f64, trade_seconds: f64) -> f64 {
    lambda / (1.0 + lambda * trade_seconds)
}

impl Calibration {
    pub fn trade_seconds(&self, chain: &Chain) -> f64 {
        self.trade_overhead_s + self.settle_fixed_s + self.settle_slots * chain.slot_ms / 1000.0
    }

    fn shares(&self, venues: &[Venue]) -> Vec<(Venue, f64)> {
        let listed: Vec<(Venue, f64)> = self
            .venue_share
            .iter()
            .copied()
            .filter(|(v, _)| venues.contains(v))
            .collect();
        let total: f64 = listed.iter().map(|(_, s)| s).sum();
        if total <= 0.0 {
            let n = venues.len().max(1) as f64;
            return venues.iter().map(|v| (*v, 1.0 / n)).collect();
        }
        listed.into_iter().map(|(v, s)| (v, s / total)).collect()
    }
}

pub fn estimate(spec: &Spec, chain: &Chain, cal: &Calibration) -> Estimate {
    let slot_s = chain.slot_ms / 1000.0;
    let pools = spec.pools();
    let pairs = spec.pairs();
    let makers = spec.maker_count();
    let bots = spec.arb.bots;
    let takers = spec.takers.count;
    let mut warnings = Vec::new();

    let lambda = spec.takers.trades_per_min / 60.0;
    let d = cal.trade_seconds(chain);
    let per_taker = closed_loop(lambda, d);
    let taker_tx = f64::from(takers) * per_taker * (1.0 - cal.unaffordable_ratio);
    if lambda * d > 0.2 {
        warnings.push(format!(
            "each taker spends {:.0}% of its time in a trade: more trades per taker add less than proportional load; add takers instead",
            100.0 * lambda * d / (1.0 + lambda * d)
        ));
    }

    let mut ladder_ticks = 0.0;
    let mut amm_makers = 0u32;
    for pool in &pools {
        let (ladders, ranges, arbs) = spec.makers_on(pool);
        // Ladder k ticks every 2000 + 500·k ms.
        ladder_ticks += (0..ladders)
            .map(|k| 1000.0 / (2000.0 + 500.0 * f64::from(k)))
            .sum::<f64>();
        amm_makers += u32::from(ranges) + u32::from(arbs);
    }
    let maker_tx =
        ladder_ticks * cal.ladder_requote_ratio + f64::from(amm_makers) * cal.amm_maker_tx_per_s;
    let arb_tx = f64::from(bots) * cal.arb_bot_tx_per_s;
    let tps = taker_tx + maker_tx + arb_tx;

    // Legs per pool: pairs are drawn uniformly.
    let per_pair = taker_tx / pairs.len().max(1) as f64;
    let shares = cal.shares(&spec.venues);
    let mut pool_legs = vec![0.0; pools.len()];
    let usdc_legs = |token: &str, legs: f64, pool_legs: &mut Vec<f64>| {
        for (venue, share) in &shares {
            if let Some(i) = pools
                .iter()
                .position(|p| p.venue == *venue && p.base == token && p.quote == "USDC")
            {
                pool_legs[i] += legs * share;
            }
        }
    };
    let mut bridged = 0;
    for (a, b) in &pairs {
        if b == "USDC" {
            usdc_legs(a, per_pair, &mut pool_legs);
        } else if let Some(i) = pools
            .iter()
            .position(|p| (p.base == *a && p.quote == *b) || (p.base == *b && p.quote == *a))
        {
            pool_legs[i] += per_pair;
        } else {
            bridged += 1;
            usdc_legs(a, per_pair, &mut pool_legs);
            usdc_legs(b, per_pair, &mut pool_legs);
        }
    }
    if bridged > 0 {
        warnings.push(format!(
            "{bridged} pair(s) have no pool: the router bridges them through USDC (2 legs, about twice the compute)"
        ));
    }
    let legs: f64 = pool_legs.iter().sum();
    let swap_cu = legs * cal.cu_per_leg;
    // Maker and arb transactions cost about a swap leg each.
    let cu_per_s = swap_cu + (maker_tx + arb_tx) * cal.cu_per_leg;
    let cu_per_block = cu_per_s * slot_s;

    let mut venues: Vec<VenueLoad> = Venue::ALL
        .into_iter()
        .map(|venue| {
            let legs_per_s: f64 = pools
                .iter()
                .zip(&pool_legs)
                .filter(|(p, _)| p.venue == venue)
                .map(|(_, l)| l)
                .sum();
            VenueLoad {
                venue,
                legs_per_s,
                share: if legs > 0.0 { legs_per_s / legs } else { 0.0 },
            }
        })
        .collect();
    venues.retain(|v| v.legs_per_s > 0.0);
    let mut hottest: Vec<PoolLoad> = pools
        .iter()
        .zip(&pool_legs)
        .map(|(p, l)| PoolLoad {
            pool: format!("{} {}/{}", p.venue.as_str(), p.base, p.quote),
            legs_per_s: *l,
            cu_per_block: l * cal.cu_per_leg * slot_s,
        })
        .collect();
    hottest.sort_by(|a, b| b.legs_per_s.total_cmp(&a.legs_per_s));
    hottest.truncate(5);

    let mut headroom = Vec::new();
    if let Some(host) = chain.host_cu_per_s {
        headroom.push(Headroom {
            limit: Limit::Host,
            utilisation: cu_per_s / host,
            detail: format!(
                "{} of ~{} CU/s this host's validator executes",
                cu(cu_per_s),
                cu(host)
            ),
        });
    }
    headroom.push(Headroom {
        limit: Limit::Block,
        utilisation: cu_per_block / chain.block_cu_limit,
        detail: format!(
            "{} of {} CU per block",
            cu(cu_per_block),
            cu(chain.block_cu_limit)
        ),
    });
    if let Some(hot) = hottest.first() {
        headroom.push(Headroom {
            limit: Limit::Account,
            utilisation: hot.cu_per_block / chain.account_cu_limit,
            detail: format!(
                "{}: {} of {} CU per block",
                hot.pool,
                cu(hot.cu_per_block),
                cu(chain.account_cu_limit)
            ),
        });
    }
    let tightest = headroom
        .iter()
        .max_by(|a, b| a.utilisation.total_cmp(&b.utilisation))
        .cloned();
    let bottleneck = tightest
        .as_ref()
        .filter(|h| h.utilisation >= 1.0)
        .map(|h| h.limit);
    let tps_capped = match &tightest {
        Some(h) if h.utilisation > 1.0 => tps / h.utilisation,
        _ => tps,
    };
    if let Some(h) = tightest
        .as_ref()
        .filter(|h| h.utilisation * cal.burst >= 1.0 && h.utilisation < 1.0)
    {
        warnings.push(format!(
            "busy blocks will reach the {} limit ({:.0}% on average, bursts ~{:.1}× that)",
            limit_name(h.limit),
            100.0 * h.utilisation,
            cal.burst
        ));
    }
    if chain.shared {
        warnings.push(
            "testnet is shared: this is the load we offer; what lands also depends on other traffic and the leader schedule"
                .into(),
        );
    }
    if spec.makers.capital.is_some() {
        let auto = spec.auto_capital();
        if spec.capital() * rust_decimal::Decimal::TWO < auto {
            warnings.push(format!(
                "maker capital ×{} is under half the flow-sized ×{auto}: books and pools may run dry",
                spec.capital()
            ));
        }
    }
    if spec.makers.ladders == 0 && spec.venues.contains(&Venue::Manifest) {
        warnings.push("Manifest books have no ladders: they stay empty".into());
    }
    if spec.makers.ranges == 0 && spec.venues.iter().any(|v| *v != Venue::Manifest) {
        warnings.push("AMM pools have no LP ranges: only the seed liquidity trades".into());
    }

    // Bootstrap: one top-up per wallet, 9 instructions per transaction
    // (a SOL transfer plus create-ATA + mint per token).
    let taker_tokens = spec.all_tokens().len() as f64;
    let top_up_txs = |tokens: f64| ((1.0 + 2.0 * tokens) / 9.0).ceil();
    let bot_tokens = 2.0;
    let funding = f64::from(takers) * top_up_txs(taker_tokens)
        + f64::from(makers) * top_up_txs(2.0)
        + f64::from(bots) * top_up_txs(bot_tokens);
    // Pools: Orca adds tick arrays; clob makers claim a seat and deposit.
    let setup = pools.len() as f64 * 4.0 + f64::from(makers) * 2.0;
    let wallets = f64::from(takers + makers + bots);
    let top_up_s = d + 0.05 * (1.0 + taker_tokens);
    let bootstrap = Bootstrap {
        transactions: funding + setup,
        seconds: wallets / cal.funding_concurrency * top_up_s + pools.len() as f64 * 2.0 * d,
    };

    let sol = Sol {
        fees_per_hour: tps * SIGNATURE_FEE_LAMPORTS * 3600.0 / LAMPORTS_PER_SOL,
        rent: (f64::from(takers) * taker_tokens
            + f64::from(makers) * 2.0
            + f64::from(bots) * bot_tokens)
            * TOKEN_ACCOUNT_RENT_SOL,
        float: f64::from(takers) * f(spec.taker_sol())
            + f64::from(makers) * 2.0
            + f64::from(bots) * 3.0,
    };

    Estimate {
        chain: chain.clone(),
        takers,
        makers,
        bots,
        participants: takers + makers + bots,
        pools: pools.len() as u32,
        capital: spec.capital().to_string(),
        offered_swaps_per_s: spec.offered_swaps_per_s(),
        trade_seconds: d,
        taker_tx_per_s: taker_tx,
        maker_tx_per_s: maker_tx,
        arb_tx_per_s: arb_tx,
        tps,
        tps_capped,
        cu_per_s,
        cu_per_block,
        headroom,
        bottleneck,
        venues,
        hottest,
        bootstrap,
        sol,
        warnings,
    }
}

/// Compute in K / M with sensible precision.
fn cu(value: f64) -> String {
    if value >= 10e6 {
        format!("{:.0}M", value / 1e6)
    } else if value >= 1e6 {
        format!("{:.1}M", value / 1e6)
    } else {
        format!("{:.0}K", value / 1e3)
    }
}

fn limit_name(limit: Limit) -> &'static str {
    match limit {
        Limit::Host => "host execution",
        Limit::Block => "block compute",
        Limit::Account => "per-pool compute",
    }
}

/// Takers needed for `target_tps` with everything else in `spec` fixed
/// (makers and bots add their own small share). `None` when makers and
/// bots alone already exceed it.
pub fn takers_for(spec: &Spec, chain: &Chain, cal: &Calibration, target_tps: f64) -> Option<u32> {
    let mut probe = spec.clone();
    probe.takers.count = 1;
    let base = estimate(&probe, chain, cal);
    let fixed = base.maker_tx_per_s + base.arb_tx_per_s;
    let per_taker = base.taker_tx_per_s;
    if target_tps <= fixed || per_taker <= 0.0 {
        return None;
    }
    Some(((target_tps - fixed) / per_taker).ceil() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preset;

    #[test]
    fn x1500_matches_the_overnight_run() {
        // 2026-10-17 00:12–08:12, 4.3 at 423 ms slots: 1,946 swaps/s landed
        // (1,962 attempted), 1.6 maker tx/s, 42.8M CU per block.
        let e = estimate(
            &preset("x1500").expect("preset"),
            &Chain::local_400ms(),
            &Calibration::default(),
        );
        assert!(
            (e.taker_tx_per_s - 1946.0).abs() < 20.0,
            "{}",
            e.taker_tx_per_s
        );
        assert!((e.maker_tx_per_s - 1.6).abs() < 0.2, "{}", e.maker_tx_per_s);
        assert!(
            (e.cu_per_block / 1e6 - 42.8).abs() < 2.5,
            "{}",
            e.cu_per_block
        );
        assert_eq!(e.participants, 24_831);
        assert_eq!(e.bottleneck, None);
    }

    #[test]
    fn x1500_matches_the_200ms_rerun() {
        // 2026-10-17 10:58–11:28, 4.4 + testnet features at 186 ms: 1,970
        // swaps/s, 19.7M CU per block.
        let chain = Chain {
            slot_ms: 186.0,
            ..Chain::local()
        };
        let e = estimate(
            &preset("x1500").expect("preset"),
            &chain,
            &Calibration::default(),
        );
        assert!(
            (e.taker_tx_per_s - 1970.0).abs() < 20.0,
            "{}",
            e.taker_tx_per_s
        );
        assert!(
            (e.cu_per_block / 1e6 - 19.7).abs() < 1.5,
            "{}",
            e.cu_per_block
        );
    }

    #[test]
    fn hottest_pools_follow_the_measured_split() {
        // Raydium took ~505 swaps/s per USDC pool, the Orca cross ~650.
        let e = estimate(
            &preset("x1500").expect("preset"),
            &Chain::local(),
            &Calibration::default(),
        );
        assert!(
            e.hottest[0].pool.starts_with("orca-whirlpool JUP/RAY"),
            "{:?}",
            e.hottest
        );
        assert!(
            e.hottest[1].pool.starts_with("raydium-clmm"),
            "{:?}",
            e.hottest
        );
        let share: f64 = e.venues.iter().map(|v| v.share).sum();
        assert!((share - 1.0).abs() < 1e-9);
    }

    #[test]
    fn the_host_caps_a_large_enough_spec() {
        let mut spec = preset("x2000").expect("preset");
        spec.takers.count *= 3;
        let e = estimate(&spec, &Chain::local(), &Calibration::default());
        assert_eq!(e.bottleneck, Some(Limit::Host));
        assert!(e.tps_capped < e.tps);
    }

    #[test]
    fn takers_for_inverts_the_estimate() {
        let spec = preset("x1500").expect("preset");
        let (chain, cal) = (Chain::local(), Calibration::default());
        let n = takers_for(&spec, &chain, &cal, 1000.0).expect("reachable");
        let mut sized = spec.clone();
        sized.takers.count = n;
        let tps = estimate(&sized, &chain, &cal).tps;
        assert!((1000.0..1000.2).contains(&tps), "{n} takers → {tps}");
    }
}
