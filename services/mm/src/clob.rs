//! `clob-ladder`: resting Manifest ladders around the oracle mid.
//!
//! The maker deposits its wallet into its seat once, then every tick:
//! reads its seat and orders, skews the mid against its inventory, and
//! atomically replaces the ladder when the mid has moved by `refresh_bps`
//! or any of its orders filled.

use std::sync::Arc;

use anyhow::{Context, Result};
use manifest_dex_client::{ManifestClient, OrderSpec, Side, TokenAmount};
use mtm_scenario::Strategy;
use mtm_scenario::pricing::PriceBook;
use mtm_scenario::venue::to_ui;
use rust_decimal::{Decimal, RoundingStrategy};
use solana_commitment_config::CommitmentConfig;
use tracing::info;

use crate::maker::Ctx;

const D18: u128 = 1_000_000_000_000_000_000;

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

    /// Hedge the seat when its inventory strays from the target: withdraw
    /// what is not locked in orders, hedge it in the wallet, deposit back.
    async fn hedge(
        &mut self,
        ctx: &Ctx,
        prices: &PriceBook,
        seat: &manifest_dex_client::Seat,
        base_atoms: u64,
        quote_atoms: u64,
    ) -> Result<bool> {
        let held = crate::hedge::Inventory {
            base_total: to_ui(&ctx.base, base_atoms),
            quote_total: to_ui(&ctx.quote, quote_atoms),
            base_free: to_ui(&ctx.base, seat.base_withdrawable),
            quote_free: to_ui(&ctx.quote, seat.quote_withdrawable),
        };
        let Some(plan) = crate::hedge::check(ctx, prices, held)? else {
            return Ok(false);
        };
        self.client
            .withdraw_all(ctx.pool)
            .await
            .context("withdrawing the seat to hedge")?;
        crate::hedge::execute(ctx, &plan).await?;
        for token in [&ctx.base, &ctx.quote] {
            let atoms = ctx.wallet_balance(token).await?;
            if atoms > 0 {
                self.client
                    .deposit(
                        ctx.pool,
                        TokenAmount {
                            mint: token.mint,
                            amount: to_ui(token, atoms),
                        },
                    )
                    .await
                    .with_context(|| format!("re-depositing {} after a hedge", token.name))?;
            }
        }
        Ok(true)
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
        let mut base_atoms = seat.base_withdrawable;
        let mut quote_atoms = seat.quote_withdrawable;
        for order in &mine {
            if snapshot
                .book
                .asks
                .iter()
                .any(|ask| ask.index == order.index)
            {
                base_atoms = base_atoms.saturating_add(order.base_atoms);
            } else {
                let locked = (order.price_raw * u128::from(order.base_atoms)).div_ceil(D18);
                quote_atoms = quote_atoms.saturating_add(u64::try_from(locked).unwrap_or(u64::MAX));
            }
        }
        ctx.record_inventory(&prices, base_atoms, quote_atoms);
        if self
            .hedge(ctx, &prices, &seat, base_atoms, quote_atoms)
            .await?
        {
            // The seat changed: re-quote from a fresh read next tick.
            self.last = None;
            return Ok(());
        }
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

        let base = to_ui(&ctx.base, base_atoms);
        let quote = to_ui(&ctx.quote, quote_atoms);
        let value = base * mid + quote;
        let ratio = if value.is_zero() {
            target_base_ratio
        } else {
            base * mid / value
        };
        // Long base → shift quotes down to sell it; short base → shift up.
        let span = target_base_ratio.max(Decimal::ONE - target_base_ratio);
        let lean = ((ratio - target_base_ratio) / span).clamp(-Decimal::ONE, Decimal::ONE);
        let skewed = mid * (Decimal::ONE - lean * Decimal::from(skew_bps) / Decimal::from(10_000));

        let levels = Decimal::from(levels.max(1));
        let ask_size = level_size.min(base / levels);
        let mut orders = Vec::new();
        for level in 0..u32::try_from(levels.mantissa()).unwrap_or(1) {
            let offset = Decimal::from(spread_bps + level * step_bps) / Decimal::from(10_000);
            // Significant figures, not decimal places: 8 places is ~5 bps
            // at BONK's price, enough to collapse levels onto one price.
            let round = |price: Decimal| price.round_sf(8).unwrap_or(price);
            let bid = round(skewed * (Decimal::ONE - offset));
            let ask = round(skewed * (Decimal::ONE + offset));
            let bid_size = level_size.min(quote / levels / bid);
            for (side, price, size) in [(Side::Bid, bid, bid_size), (Side::Ask, ask, ask_size)] {
                let size = size
                    .round_dp_with_strategy(u32::from(ctx.base.decimals), RoundingStrategy::ToZero);
                if size > Decimal::ZERO && price > Decimal::ZERO {
                    orders.push(OrderSpec {
                        side,
                        price,
                        base_amount: size,
                    });
                }
            }
        }
        if orders.is_empty() {
            return Ok(());
        }
        let outcome = self.client.replace_orders(ctx.pool, &orders).await?;
        ctx.action("requote");
        metrics::gauge!("mm_skew_bps", "maker" => ctx.name()).set(
            rust_decimal::prelude::ToPrimitive::to_f64(&(lean * Decimal::from(skew_bps)))
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
