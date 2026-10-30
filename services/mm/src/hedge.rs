//! Off-chain hedging at the oracle price.
//!
//! Real market makers lay off their on-chain fills on a CEX, which keeps
//! their inventory near a target mix; the scenario's uninformed flow
//! otherwise leaves every maker one-sided within an hour or two (all base,
//! no quote), and the venues lose depth. A maker whose base share of
//! inventory value strays past `threshold_bps` from its target sells the
//! excess "on the exchange": the tokens are test tokens, so the sold side
//! is burned from the maker's wallet and the bought side minted into it,
//! at the oracle price less `cost_bps`. At the oracle price a hedge only
//! costs its fee, so it does not distort PnL (unlike taker refills).

use anyhow::{Context, Result};
use metrics::counter;
use mtm_chain::SubmissionConfig;
use mtm_dex_harness::spl;
use mtm_scenario::HedgeConfig;
use mtm_scenario::pricing::PriceBook;
use mtm_scenario::venue::{to_atoms, to_ui};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use solana_signer::Signer;
use tracing::info;

use crate::maker::Ctx;

/// A planned hedge, in UI units.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// Sell base for quote (else quote for base).
    pub sell_base: bool,
    pub sell: Decimal,
    pub buy: Decimal,
    pub notional_usd: Decimal,
    pub cost_usd: Decimal,
}

/// What the maker holds, in UI units: `total` sizes the decision, `free`
/// (wallet, or seat withdrawable) caps what can be sold now.
#[derive(Debug, Clone, Copy)]
pub struct Inventory {
    pub base_total: Decimal,
    pub quote_total: Decimal,
    pub base_free: Decimal,
    pub quote_free: Decimal,
}

/// The hedge that brings the base value share back to `target`, or `None`
/// inside the threshold or below the minimum size.
pub fn plan(
    held: Inventory,
    base_usd: Decimal,
    quote_usd: Decimal,
    target: Decimal,
    config: &HedgeConfig,
) -> Option<Plan> {
    let base_value = held.base_total * base_usd;
    let total = base_value + held.quote_total * quote_usd;
    if total <= Decimal::ZERO || base_usd <= Decimal::ZERO || quote_usd <= Decimal::ZERO {
        return None;
    }
    let share = base_value / total;
    if (share - target).abs() * Decimal::from(10_000) < Decimal::from(config.threshold_bps) {
        return None;
    }
    let excess_usd = (base_value - target * total).abs();
    let sell_base = share > target;
    let (price_sell, price_buy, free) = if sell_base {
        (base_usd, quote_usd, held.base_free)
    } else {
        (quote_usd, base_usd, held.quote_free)
    };
    let sell = (excess_usd / price_sell).min(free);
    let notional_usd = sell * price_sell;
    if notional_usd < config.min_usd || notional_usd <= Decimal::ZERO {
        return None;
    }
    let cost_usd = notional_usd * Decimal::from(config.cost_bps) / Decimal::from(10_000);
    Some(Plan {
        sell_base,
        sell,
        buy: (notional_usd - cost_usd) / price_buy,
        notional_usd,
        cost_usd,
    })
}

/// The maker's target base share: the CLOB ladder's own target, else half.
pub fn target(ctx: &Ctx) -> Decimal {
    match &ctx.maker.strategy {
        mtm_scenario::Strategy::ClobLadder {
            target_base_ratio, ..
        } => *target_base_ratio,
        _ => Decimal::new(5, 1),
    }
}

/// Plan for `held`, if hedging is on for this maker.
pub fn check(ctx: &Ctx, prices: &PriceBook, held: Inventory) -> Result<Option<Plan>> {
    let Some((config, _)) = &ctx.hedger else {
        return Ok(None);
    };
    Ok(plan(
        held,
        prices.usd(&ctx.base)?,
        prices.usd(&ctx.quote)?,
        target(ctx),
        config,
    ))
}

/// Burn the sold side from the maker's wallet and mint the bought side into
/// it, in one transaction (the maker pays, the mint authority co-signs).
pub async fn execute(ctx: &Ctx, plan: &Plan) -> Result<()> {
    let (_, authority) = ctx.hedger.as_ref().context("hedging is off")?;
    let (sold, bought) = if plan.sell_base {
        (&ctx.base, &ctx.quote)
    } else {
        (&ctx.quote, &ctx.base)
    };
    let owner = ctx.wallet_pubkey();
    let sell_atoms = to_atoms(sold, plan.sell).context("hedge size overflow")?;
    let buy_atoms = to_atoms(bought, plan.buy).context("hedge size overflow")?;
    if sell_atoms == 0 || buy_atoms == 0 {
        return Ok(());
    }
    let instructions = vec![
        spl::create_associated_token_account_idempotent(
            &owner,
            &owner,
            &bought.mint,
            &bought.token_program,
        ),
        spl::burn_checked(
            &sold.token_program,
            &spl::associated_token_address(&owner, &sold.mint, &sold.token_program),
            &sold.mint,
            &owner,
            sell_atoms,
            sold.decimals,
        ),
        spl::mint_to_checked(
            &bought.token_program,
            &bought.mint,
            &spl::associated_token_address(&owner, &bought.mint, &bought.token_program),
            &authority.pubkey(),
            buy_atoms,
            bought.decimals,
        ),
    ];
    let config = SubmissionConfig {
        compute_unit_limit: 60_000,
        ..SubmissionConfig::default()
    };
    ctx.chain
        .submit(
            instructions,
            ctx.wallet.as_ref(),
            &[authority.as_ref()],
            config,
        )
        .await
        .context("hedging")?;
    let micro = |usd: Decimal| {
        (usd * Decimal::from(1_000_000))
            .round()
            .to_u64()
            .unwrap_or_default()
    };
    let side = if plan.sell_base {
        "sell_base"
    } else {
        "sell_quote"
    };
    counter!("mm_hedge_micro_usd_total", "maker" => ctx.name(), "side" => side)
        .increment(micro(plan.notional_usd));
    counter!("mm_hedge_cost_micro_usd_total", "maker" => ctx.name())
        .increment(micro(plan.cost_usd));
    ctx.action("hedge");
    info!(
        maker = %ctx.maker.name, sold = %sold.name, amount = %plan.sell, bought = %bought.name,
        received = %plan.buy, notional_usd = %plan.notional_usd.round_dp(2), "hedged inventory at the oracle"
    );
    Ok(())
}

/// Hedge out of the wallet (oracle arbs, and LPs between positions):
/// re-reads the balances, so callers see the result.
pub async fn wallet(ctx: &Ctx, prices: &PriceBook) -> Result<bool> {
    let base = to_ui(&ctx.base, ctx.wallet_balance(&ctx.base).await?);
    let quote = to_ui(&ctx.quote, ctx.wallet_balance(&ctx.quote).await?);
    let held = Inventory {
        base_total: base,
        quote_total: quote,
        base_free: base,
        quote_free: quote,
    };
    match check(ctx, prices, held)? {
        Some(plan) => {
            execute(ctx, &plan).await?;
            Ok(true)
        }
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).expect("decimal")
    }

    fn config() -> HedgeConfig {
        HedgeConfig {
            threshold_bps: 2_000,
            cost_bps: 10,
            min_usd: d("50"),
        }
    }

    fn held(base: &str, quote: &str) -> Inventory {
        Inventory {
            base_total: d(base),
            quote_total: d(quote),
            base_free: d(base),
            quote_free: d(quote),
        }
    }

    #[test]
    fn inside_the_threshold_nothing_happens() {
        // 60% base against a 50% target: 10 points < 20
        assert_eq!(
            plan(held("600", "400"), d("1"), d("1"), d("0.5"), &config()),
            None
        );
    }

    #[test]
    fn a_one_sided_maker_sells_back_to_target() {
        // all base: $1000 at $2, target half → sell $500 of base
        let p = plan(held("500", "0"), d("2"), d("1"), d("0.5"), &config()).expect("hedge");
        assert!(p.sell_base);
        assert_eq!(p.sell, d("250"));
        assert_eq!(p.notional_usd, d("500"));
        assert_eq!(p.cost_usd, d("0.5"));
        assert_eq!(p.buy, d("499.5"));
        // and the other way round
        let p = plan(held("0", "1000"), d("2"), d("1"), d("0.5"), &config()).expect("hedge");
        assert!(!p.sell_base);
        assert_eq!(p.sell, d("500"));
        assert_eq!(p.buy, d("249.75"));
    }

    #[test]
    fn only_free_inventory_is_sold_and_dust_is_skipped() {
        let mut h = held("500", "0");
        h.base_free = d("100"); // the rest locked in orders
        let p = plan(h, d("2"), d("1"), d("0.5"), &config()).expect("hedge");
        assert_eq!(p.sell, d("100"));
        assert_eq!(p.notional_usd, d("200"));
        h.base_free = d("10"); // $20 < $50 minimum
        assert_eq!(plan(h, d("2"), d("1"), d("0.5"), &config()), None);
        assert_eq!(
            plan(held("0", "0"), d("2"), d("1"), d("0.5"), &config()),
            None
        );
    }

    #[test]
    fn a_skewed_target_is_respected() {
        // target 30% base; holding 50% → 20 points off → sell base
        let p = plan(held("500", "500"), d("1"), d("1"), d("0.3"), &config()).expect("hedge");
        assert!(p.sell_base);
        assert_eq!(p.notional_usd, d("200"));
    }
}
