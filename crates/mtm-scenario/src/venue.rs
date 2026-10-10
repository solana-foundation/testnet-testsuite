//! Venue price math on swap snapshots: spot prices, marginal prices, the
//! trade size that moves a pool to a target price, and swap execution.

use anyhow::{Context, Result};
use mtm_chain::{ChainClient, SubmissionConfig, SubmissionOutcome};
use mtm_venues::{LegQuote, PoolSnapshot};
use rust_decimal::Decimal;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use solana_signer::Signer;

use crate::Token;

/// Exact-in output of `amount` input atoms, or `None` if the venue cannot fill.
fn out(snapshot: &PoolSnapshot, input: &Token, output: &Token, amount: u64) -> Option<u64> {
    if amount == 0 {
        return Some(0);
    }
    snapshot
        .quote(input.mint, output.mint, amount, true, 0)
        .ok()
        .map(|quote| quote.out_amount)
}

/// UI price (quote per base) of a `quote_atoms`/`base_atoms` exchange.
pub fn ui_price(base: &Token, quote: &Token, base_atoms: u64, quote_atoms: u64) -> Option<Decimal> {
    if base_atoms == 0 {
        return None;
    }
    let ratio = Decimal::from(quote_atoms).checked_div(Decimal::from(base_atoms))?;
    let shift = i32::from(base.decimals) - i32::from(quote.decimals);
    let scale = Decimal::from_i128_with_scale(10_i128.pow(shift.unsigned_abs()), 0);
    if shift >= 0 {
        ratio.checked_mul(scale)
    } else {
        ratio.checked_div(scale)
    }
}

/// Probe size: 1/1000 of one UI unit, at least one atom.
fn probe(token: &Token) -> u64 {
    (10_u64.pow(u32::from(token.decimals)) / 1_000).max(1)
}

/// Mid of the best sell and buy prices for a tiny size.
pub fn spot_price(snapshot: &PoolSnapshot, base: &Token, quote: &Token) -> Option<Decimal> {
    let base_probe = probe(base);
    let sell = out(snapshot, base, quote, base_probe)
        .and_then(|quote_out| ui_price(base, quote, base_probe, quote_out));
    let quote_probe = probe(quote);
    let buy = out(snapshot, quote, base, quote_probe)
        .and_then(|base_out| ui_price(base, quote, base_out, quote_probe));
    match (sell, buy) {
        (Some(sell), Some(buy)) => Some((sell + buy) / Decimal::TWO),
        (one, other) => one.or(other),
    }
}

/// Marginal price (quote per base) after trading `amount` input atoms:
/// the executable price of the next unit, fees included.
pub fn marginal(
    snapshot: &PoolSnapshot,
    base: &Token,
    quote: &Token,
    buy_base: bool,
    amount: u64,
) -> Option<Decimal> {
    let (input, output) = if buy_base {
        (quote, base)
    } else {
        (base, quote)
    };
    let delta = (amount / 1_000).max(probe(input));
    let before = out(snapshot, input, output, amount)?;
    let after = out(snapshot, input, output, amount.checked_add(delta)?)?;
    let gained = after.checked_sub(before)?;
    if buy_base {
        ui_price(base, quote, gained, delta)
    } else {
        ui_price(base, quote, delta, gained)
    }
}

/// Input atoms that move the venue's marginal price to `target` (quote per
/// base), capped at `max_in`. `None` when even a probe trade is not
/// profitable against `target`.
pub fn size_to_target(
    snapshot: &PoolSnapshot,
    base: &Token,
    quote: &Token,
    target: Decimal,
    buy_base: bool,
    max_in: u64,
) -> Option<u64> {
    // Buying base is worth it while the marginal price is below target;
    // selling base while it is above.
    let worth = |amount: u64| {
        marginal(snapshot, base, quote, buy_base, amount).is_some_and(|price| {
            if buy_base {
                price < target
            } else {
                price > target
            }
        })
    };
    let input = if buy_base { quote } else { base };
    if max_in < probe(input) || !worth(0) {
        return None;
    }
    if worth(max_in) {
        return Some(max_in);
    }
    let (mut low, mut high) = (0_u64, max_in);
    for _ in 0..40 {
        if high - low <= probe(input) {
            break;
        }
        let middle = low + (high - low) / 2;
        if worth(middle) {
            low = middle;
        } else {
            high = middle;
        }
    }
    (low > 0).then_some(low)
}

/// UI amount → atoms (floor).
pub fn to_atoms(token: &Token, amount: Decimal) -> Option<u64> {
    let scale = Decimal::from_u64(10_u64.pow(u32::from(token.decimals)))?;
    amount.checked_mul(scale)?.floor().to_u64()
}

/// Atoms → UI amount.
pub fn to_ui(token: &Token, atoms: u64) -> Decimal {
    Decimal::from(atoms) / Decimal::from(10_u64.pow(u32::from(token.decimals)))
}

/// Execute a quoted leg as one top-level venue swap signed by `owner`.
pub async fn execute(
    chain: &ChainClient,
    owner: &(dyn Signer + Send + Sync),
    snapshot: &PoolSnapshot,
    quote: &LegQuote,
) -> Result<SubmissionOutcome> {
    let instructions = snapshot
        .swap_instructions(owner.pubkey(), quote)
        .context("building swap")?;
    Ok(chain
        .submit(
            instructions,
            owner,
            &[],
            SubmissionConfig {
                compute_unit_limit: quote.cu_estimate.saturating_add(50_000),
                ..SubmissionConfig::default()
            },
        )
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_pubkey::Pubkey;

    fn token(decimals: u8) -> Token {
        Token {
            name: "T".into(),
            mint: Pubkey::new_unique(),
            decimals,
            token_program: Pubkey::default(),
            oracle_symbol: "T/USD".into(),
        }
    }

    #[test]
    fn ui_price_shifts_by_decimals() {
        // 1 base (9 dp) for 2 quote (6 dp)
        assert_eq!(
            ui_price(&token(9), &token(6), 1_000_000_000, 2_000_000),
            Some(Decimal::TWO)
        );
        assert_eq!(
            ui_price(&token(6), &token(6), 4, 1),
            Some(Decimal::new(25, 2))
        );
        assert_eq!(ui_price(&token(6), &token(6), 0, 1), None);
    }

    #[test]
    fn atoms_round_trip() {
        let t = token(6);
        assert_eq!(to_atoms(&t, Decimal::new(15, 1)), Some(1_500_000));
        assert_eq!(to_ui(&t, 1_500_000), Decimal::new(15, 1));
    }
}
