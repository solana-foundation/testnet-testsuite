//! `clob-ladder`: resting Manifest ladders around the oracle mid.
//!
//! The maker deposits its wallet into its seat once, then every tick:
//! reads its seat and orders, skews the mid against its inventory, and
//! atomically replaces the ladder when the mid has moved by `refresh_bps`
//! or any of its orders filled.

use std::cell::Cell;
use std::sync::Arc;

use anyhow::{Context, Result};
use manifest_dex_client::{ManifestClient, OrderSpec, Side, TokenAmount};
use mtm_scenario::Strategy;
use mtm_scenario::venue::to_ui;
use rust_decimal::{Decimal, RoundingStrategy};
use solana_commitment_config::CommitmentConfig;
use tracing::info;

use crate::maker::Ctx;

pub struct Clob {
    client: ManifestClient,
    /// Mid and `(sequence, base atoms)` of the ladder last placed.
    last: Option<(Decimal, Vec<(u64, u64)>)>,
    /// Whether the ladder was pulled for a stale oracle (and not re-quoted).
    pulled: bool,
}

impl Clob {
    pub async fn start(ctx: &Ctx) -> Result<Self> {
        let (slippage_bps, compute_units, priority_fee) = ctx.execution(0, 400_000);
        let client = ManifestClient::new(
            ctx.chain.clone(),
            ctx.scenario.programs.manifest,
            Arc::clone(&ctx.wallet) as _,
            slippage_bps,
            compute_units,
            priority_fee,
            CommitmentConfig::confirmed(),
        )?;
        client.claim_seat(ctx.pool).await.context("claiming seat")?;
        for token in [&ctx.base, &ctx.quote] {
            let atoms = ctx.wallet_balance(token).await?;
            if atoms > 0 {
                client
                    .deposit(
                        ctx.pool,
                        TokenAmount {
                            mint: token.mint,
                            amount: to_ui(token, atoms),
                        },
                    )
                    .await
                    .with_context(|| format!("depositing {}", token.name))?;
                info!(maker = %ctx.maker.name, token = %token.name, amount = %to_ui(token, atoms), "deposited into seat");
            }
        }
        Ok(Self {
            client,
            last: None,
            pulled: false,
        })
    }

    pub async fn tick(&mut self, ctx: &Ctx) -> Result<()> {
        let Strategy::ClobLadder {
            levels,
            spread_bps,
            step_bps,
            level_size,
            refresh_bps,
            skew_bps,
            target_base_ratio,
        } = ctx.maker.strategy.clone()
        else {
            unreachable!("clob maker with a non-clob strategy");
        };
        let params = LadderParams {
            levels: u32::from(levels),
            spread_bps,
            step_bps,
            level_size,
            skew_bps,
            target_base_ratio,
        };
        let prices = ctx.prices().await?;
        let mid = match prices.pair(&ctx.base, &ctx.quote) {
            Ok(mid) => mid,
            Err(error) => {
                // Orders quoted on an old price get picked off: pull them
                // once until the oracle is fresh again (also orders left
                // from before a restart).
                if !self.pulled {
                    self.client
                        .cancel_all_orders(ctx.pool)
                        .await
                        .context("pulling quotes on a stale oracle")?;
                    self.pulled = true;
                    self.last = None;
                    ctx.action("pull");
                }
                return Err(error);
            }
        };
        self.pulled = false;
        let snapshot = self.client.load_market(ctx.pool).await?;
        let seat = snapshot
            .book
            .seat(&ctx.wallet_pubkey())
            .context("maker seat disappeared")?;
        let mine: Vec<_> = snapshot.book.orders_of(seat.index).copied().collect();

        // Seat inventory = withdrawable + what our resting orders lock.
        let (base_atoms, quote_atoms) = snapshot.book.holdings(&seat)?;
        ctx.record_inventory(&prices, base_atoms, quote_atoms);
        let best_bid = snapshot.book.bids.first().map(|o| o.price_raw);
        let best_ask = snapshot.book.asks.first().map(|o| o.price_raw);
        let book_mid = match (best_bid, best_ask) {
            (Some(bid), Some(ask)) => mtm_scenario::venue::ui_price(
                &ctx.base,
                &ctx.quote,
                1_000_000_000,
                u64::try_from((bid + ask) / 2 / 1_000_000_000).unwrap_or(u64::MAX),
            ),
            _ => None,
        };
        ctx.record_prices(mid, book_mid);

        let placed: Vec<(u64, u64)> = mine
            .iter()
            .map(|o| (o.sequence_number, o.base_atoms))
            .collect();
        let stale = match &self.last {
            None => true,
            Some((last_mid, last_orders)) => {
                *last_orders != placed
                    || ((mid - last_mid) / last_mid).abs() * Decimal::from(10_000)
                        >= Decimal::from(refresh_bps)
            }
        };
        if !stale {
            return Ok(());
        }

        let decimals = (ctx.base.decimals, ctx.quote.decimals);
        if ladder(mid, base_atoms, quote_atoms, decimals, &params)
            .orders
            .is_empty()
        {
            return Ok(());
        }
        // Size from the snapshot the batch is built on (again on its
        // stale-cancel retry, after the fill that caused it), not this one.
        let lean = Cell::new(Decimal::ZERO);
        let outcome = self
            .client
            .replace_orders_with(ctx.pool, |snapshot, seat| {
                let Ok((base_atoms, quote_atoms)) = snapshot.book.holdings(seat) else {
                    return vec![];
                };
                let ladder = ladder(mid, base_atoms, quote_atoms, decimals, &params);
                lean.set(ladder.lean);
                ladder.orders
            })
            .await?;
        ctx.action("requote");
        metrics::gauge!("mm_skew_bps", "maker" => ctx.name()).set(
            rust_decimal::prelude::ToPrimitive::to_f64(&(lean.get() * Decimal::from(skew_bps)))
                .unwrap_or_default(),
        );
        self.last = Some((
            mid,
            outcome
                .resting
                .iter()
                .map(|o| (o.sequence_number, o.base_atoms))
                .collect(),
        ));
        Ok(())
    }
}

/// Share of each side's inventory the ladder quotes: the rest absorbs a
/// fill between the read and landing and the bids' per-order rounding.
const HEADROOM: Decimal = Decimal::from_parts(98, 0, 0, false, 2);
/// Smallest level, as a share of `level_size`, before outer levels are
/// dropped instead of shrinking further.
const MIN_LEVEL_SHARE: Decimal = Decimal::from_parts(25, 0, 0, false, 2);
/// Quote atoms reserved per bid: its lock rounds up by up to one, and the
/// client keeps one more as margin.
const BID_RESERVE_ATOMS: i64 = 2;

/// The `clob-ladder` strategy's shape.
#[derive(Debug, Clone)]
struct LadderParams {
    levels: u32,
    spread_bps: u32,
    step_bps: u32,
    level_size: Decimal,
    skew_bps: u32,
    target_base_ratio: Decimal,
}

#[derive(Debug)]
struct Ladder {
    orders: Vec<OrderSpec>,
    /// Inventory lean in [-1, 1]: positive is long base (quotes shift down).
    lean: Decimal,
}

/// The ladder for a seat holding `base_atoms` / `quote_atoms` (decimals
/// `(base, quote)`) around `mid` (quote per base). Each side quotes at most
/// [`HEADROOM`] of its inventory (bids less [`BID_RESERVE_ATOMS`] each), so
/// a batch funded from the same holdings always fits; a side too thin for
/// every level at [`MIN_LEVEL_SHARE`] of `level_size` drops its outer
/// levels first.
fn ladder(
    mid: Decimal,
    base_atoms: u64,
    quote_atoms: u64,
    (base_decimals, quote_decimals): (u8, u8),
    params: &LadderParams,
) -> Ladder {
    let ui = |atoms: u64, decimals: u8| {
        Decimal::from_i128_with_scale(i128::from(atoms), u32::from(decimals))
    };
    let base = ui(base_atoms, base_decimals);
    let quote = ui(quote_atoms, quote_decimals);
    let value = base * mid + quote;
    let ratio = if value.is_zero() {
        params.target_base_ratio
    } else {
        base * mid / value
    };
    // Long base → shift quotes down to sell it; short base → shift up.
    let target = params.target_base_ratio;
    let span = target.max(Decimal::ONE - target);
    let lean = if span.is_zero() {
        Decimal::ZERO
    } else {
        ((ratio - target) / span).clamp(-Decimal::ONE, Decimal::ONE)
    };
    let skewed =
        mid * (Decimal::ONE - lean * Decimal::from(params.skew_bps) / Decimal::from(10_000));

    let levels = params.levels.max(1);
    // Significant figures, not decimal places: 8 places is ~5 bps at
    // BONK's price, enough to collapse levels onto one price.
    let round = |price: Decimal| price.round_sf(8).unwrap_or(price);
    let prices: Vec<(Decimal, Decimal)> = (0..levels)
        .map(|level| {
            let offset =
                Decimal::from(params.spread_bps + level * params.step_bps) / Decimal::from(10_000);
            (
                round(skewed * (Decimal::ONE - offset)),
                round(skewed * (Decimal::ONE + offset)),
            )
        })
        .collect();
    let truncate = |size: Decimal| {
        size.max(Decimal::ZERO)
            .round_dp_with_strategy(u32::from(base_decimals), RoundingStrategy::ToZero)
    };
    let min_size = params.level_size * MIN_LEVEL_SHARE;
    // Sizes for the first `n` levels of a side, widening the ladder only
    // while every level stays at the minimum size.
    let side = |size_at: &dyn Fn(u32, usize) -> Decimal| -> Vec<Decimal> {
        for n in (1..=levels).rev() {
            let sizes: Vec<Decimal> = (0..n as usize)
                .map(|level| truncate(params.level_size.min(size_at(n, level))))
                .collect();
            if n == 1 || sizes.iter().all(|size| *size >= min_size) {
                return sizes;
            }
        }
        vec![]
    };
    let ask_budget = base * HEADROOM;
    let asks = side(&|n, _| ask_budget / Decimal::from(n));
    let bid_budget = |n: u32| {
        quote * HEADROOM - Decimal::new(BID_RESERVE_ATOMS * i64::from(n), u32::from(quote_decimals))
    };
    let bids = side(&|n, level| {
        let price = prices[level].0;
        if price <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        bid_budget(n) / Decimal::from(n) / price
    });

    let mut orders = Vec::new();
    for (level, (bid, ask)) in prices.iter().enumerate() {
        for (side, price, size) in [
            (Side::Bid, *bid, bids.get(level)),
            (Side::Ask, *ask, asks.get(level)),
        ] {
            if let Some(&size) = size
                && size > Decimal::ZERO
                && price > Decimal::ZERO
            {
                orders.push(OrderSpec {
                    side,
                    price,
                    base_amount: size,
                });
            }
        }
    }
    Ladder { orders, lean }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use rust_decimal::prelude::ToPrimitive;

    use super::*;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).expect("decimal")
    }

    fn params(levels: u32, level_size: &str) -> LadderParams {
        LadderParams {
            levels,
            spread_bps: 10,
            step_bps: 10,
            level_size: d(level_size),
            skew_bps: 50,
            target_base_ratio: d("0.5"),
        }
    }

    /// Quote atoms the bids lock (rounded up, plus the client's margin) and
    /// base atoms the asks lock.
    fn locks(ladder: &Ladder, (base_decimals, quote_decimals): (u8, u8)) -> (u64, u64) {
        let scale = |decimals: u8| Decimal::from(10_u64.pow(u32::from(decimals)));
        let (mut base, mut quote) = (0_u64, 0_u64);
        for order in &ladder.orders {
            match order.side {
                Side::Ask => {
                    base += (order.base_amount * scale(base_decimals))
                        .to_u64()
                        .expect("atoms");
                }
                Side::Bid => {
                    let lock = (order.price * order.base_amount * scale(quote_decimals)).ceil();
                    quote += lock.to_u64().expect("atoms") + 1;
                }
            }
        }
        (base, quote)
    }

    #[test]
    fn a_drained_side_keeps_headroom_and_fits() {
        // The review's case: 10 USDC of quote over 3 levels near 0.5.
        let ladder = ladder(d("0.4990"), 0, 10_000_000, (6, 6), &params(3, "10"));
        let bids: Vec<_> = ladder
            .orders
            .iter()
            .filter(|o| o.side == Side::Bid)
            .collect();
        assert_eq!(bids.len(), 3);
        let (_, quote) = locks(&ladder, (6, 6));
        assert!(quote <= 9_800_000, "{quote} past the 98% headroom");
        assert!(quote > 9_700_000, "{quote}: the side is still used");
        assert!(ladder.orders.iter().all(|o| o.side == Side::Bid));
    }

    #[test]
    fn outer_levels_go_before_sizes_shrink_below_the_minimum() {
        // 1,000 base per level wanted; 1,000 base held → 980 quotable.
        // Five levels would be 196 each (< 250 minimum): three of 326.
        let ladder = ladder(d("1"), 1_000_000_000, 0, (6, 6), &params(5, "1000"));
        let asks: Vec<_> = ladder
            .orders
            .iter()
            .filter(|o| o.side == Side::Ask)
            .collect();
        assert_eq!(asks.len(), 3);
        assert!(asks.iter().all(|o| o.base_amount == d("326.666666")));
        // A side too thin for even one minimum level still quotes one.
        let ladder = ladder_with(d("1"), 100_000_000, 0);
        assert_eq!(ladder.orders.len(), 1);
        assert_eq!(ladder.orders[0].base_amount, d("98"));
        // A well-funded side quotes every level at full size.
        let ladder = ladder_with(d("1"), 100_000_000_000, 100_000_000_000);
        assert_eq!(ladder.orders.len(), 10);
        assert!(ladder.orders.iter().all(|o| o.base_amount == d("1000")));
    }

    fn ladder_with(mid: Decimal, base_atoms: u64, quote_atoms: u64) -> Ladder {
        ladder(mid, base_atoms, quote_atoms, (6, 6), &params(5, "1000"))
    }

    #[test]
    fn ladders_never_lock_more_than_the_seat_holds() {
        let mut state = 0x2545_F491_4F6C_DD1D_u64;
        let mut next = move |bound: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % bound.max(1)
        };
        for _ in 0..3_000 {
            let decimals = [(6, 6), (9, 6), (5, 6), (6, 9), (8, 6)][next(5) as usize];
            let base_atoms = next(10_u64.pow(14));
            let quote_atoms = next(10_u64.pow(14));
            // Prices from ~1e-5 (BONK) to ~2e4, several significant figures.
            let mid = Decimal::new(1 + next(99_999) as i64, 0)
                * Decimal::new(1, u32::try_from(next(10)).expect("small"));
            let mut p = params(1 + next(6) as u32, "1");
            p.level_size = Decimal::new(
                1 + next(1_000_000) as i64,
                u32::try_from(next(4)).expect("small"),
            );
            p.spread_bps = next(200) as u32;
            p.step_bps = next(100) as u32;
            p.skew_bps = next(300) as u32;
            let ladder = ladder(mid, base_atoms, quote_atoms, decimals, &p);
            let (base, quote) = locks(&ladder, decimals);
            assert!(base <= base_atoms, "asks {base} > {base_atoms}");
            assert!(
                quote <= quote_atoms,
                "bids {quote} > {quote_atoms} at {mid}"
            );
            assert!(ladder.orders.len() <= 2 * p.levels as usize);
        }
    }
}
