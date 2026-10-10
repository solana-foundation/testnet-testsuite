//! `pool-arb`: keep a pool at the oracle price. When buying or selling at
//! the pool (fees included) beats the oracle by `threshold_bps`, trade the
//! size that moves its marginal price back to the oracle (capped by
//! `max_trade_usd` and the wallet), as one top-level venue swap. The pool mid
//! therefore settles within its fee band around the oracle.

use anyhow::Result;
use mtm_scenario::Strategy;
use mtm_scenario::venue::{execute, marginal, size_to_target, spot_price, to_atoms};
use mtm_venues::PoolSnapshot;
use rust_decimal::Decimal;
use tracing::info;

use crate::maker::{Ctx, deviation_bps};

pub struct Arb;

impl Arb {
    pub async fn tick(&self, ctx: &Ctx) -> Result<()> {
        let Strategy::PoolArb {
            threshold_bps,
            max_trade_usd,
        } = ctx.maker.strategy.clone()
        else {
            unreachable!("arb maker with a non-arb strategy");
        };
        let prices = ctx.prices().await?;
        let oracle = prices.pair(&ctx.base, &ctx.quote)?;
        let program = ctx.scenario.programs.of(ctx.maker.venue);
        let snapshot = PoolSnapshot::load(
            &ctx.chain,
            mtm_scenario::Scenario::venue_kind(ctx.maker.venue),
            program,
            ctx.pool,
        )
        .await?;
        let spot = spot_price(&snapshot, &ctx.base, &ctx.quote);
        ctx.record_prices(oracle, spot);
        let base_atoms = ctx.wallet_balance(&ctx.base).await?;
        let quote_atoms = ctx.wallet_balance(&ctx.quote).await?;
        ctx.record_inventory(&prices, base_atoms, quote_atoms);
        let Some(spot) = spot else {
            return Ok(()); // no liquidity to trade against
        };
        let deviation = deviation_bps(spot, oracle);
        // Trade only when an executable price (fees included) beats the
        // oracle by the threshold; the mid alone would chase the fee band.
        let threshold = Decimal::from(threshold_bps);
        let buy_edge = marginal(&snapshot, &ctx.base, &ctx.quote, true, 0)
            .map(|price| deviation_bps(oracle, price))
            .unwrap_or_default();
        let sell_edge = marginal(&snapshot, &ctx.base, &ctx.quote, false, 0)
            .map(|price| deviation_bps(price, oracle))
            .unwrap_or_default();
        let buy_base = if buy_edge >= threshold {
            true
        } else if sell_edge >= threshold {
            false
        } else {
            return Ok(());
        };
        let (input, output, held) = if buy_base {
            (&ctx.quote, &ctx.base, quote_atoms)
        } else {
            (&ctx.base, &ctx.quote, base_atoms)
        };
        let cap = to_atoms(input, max_trade_usd / prices.usd(input)?).unwrap_or(0);
        let max_in = cap.min(held / 10 * 9);
        let Some(amount) =
            size_to_target(&snapshot, &ctx.base, &ctx.quote, oracle, buy_base, max_in)
        else {
            return Ok(());
        };
        let quote = snapshot.quote(input.mint, output.mint, amount, true, 100)?;
        let outcome = execute(&ctx.chain, ctx.wallet.as_ref(), &snapshot, &quote).await?;
        info!(
            maker = %ctx.maker.name, %deviation, input = %input.name, amount, out = quote.out_amount,
            signature = %outcome.signature, "arbitraged pool toward oracle"
        );
        ctx.action(if buy_base { "arb_buy" } else { "arb_sell" });
        metrics::counter!("mm_arb_volume_usd", "maker" => ctx.name()).increment(
            rust_decimal::prelude::ToPrimitive::to_u64(
                &(mtm_scenario::venue::to_ui(input, amount) * prices.usd(input)?),
            )
            .unwrap_or_default(),
        );
        Ok(())
    }
}
