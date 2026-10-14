//! Oracle prices as `Decimal` USD values and pair prices.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use oracle_client::{OracleClient, PriceData, PricePoint};
use rust_decimal::Decimal;

use crate::Token;

/// One snapshot of every oracle price, by symbol.
#[derive(Debug, Clone, Default)]
pub struct PriceBook {
    prices: HashMap<String, (Decimal, i64)>,
    max_age_us: i64,
}

impl PriceBook {
    pub async fn fetch(oracle: &OracleClient, max_age_ms: u64) -> Result<Self> {
        let points = oracle.prices().await.context("fetching oracle prices")?;
        let prices = points
            .iter()
            .filter_map(|point| {
                // Freshness is the age of the upstream observation: the
                // oracle restamps `received_at_us` whenever it re-serves a
                // price, so a halted feed would look fresh by that clock.
                Some((
                    point.symbol.as_str().to_owned(),
                    (
                        mid(point)?,
                        point.publish_time_us.unwrap_or(point.received_at_us),
                    ),
                ))
            })
            .collect();
        Ok(Self {
            prices,
            max_age_us: i64::try_from(max_age_ms)
                .unwrap_or(i64::MAX)
                .saturating_mul(1_000),
        })
    }

    /// USD price of `token`; errors when missing or stale.
    pub fn usd(&self, token: &Token) -> Result<Decimal> {
        let (price, observed_at_us) = self
            .prices
            .get(&token.oracle_symbol)
            .with_context(|| format!("oracle has no price for {}", token.oracle_symbol))?;
        let age = mtm_common::time::now_us().saturating_sub(*observed_at_us);
        if age > self.max_age_us {
            bail!(
                "oracle price for {} is {} ms old",
                token.oracle_symbol,
                age / 1_000
            );
        }
        if *price <= Decimal::ZERO {
            bail!("oracle price for {} is not positive", token.oracle_symbol);
        }
        Ok(*price)
    }

    /// Price of `base` in units of `quote` (UI units).
    pub fn pair(&self, base: &Token, quote: &Token) -> Result<Decimal> {
        self.usd(base)?
            .checked_div(self.usd(quote)?)
            .context("pair price overflow")
    }
}

/// Mid of any oracle observation.
pub fn mid(point: &PricePoint) -> Option<Decimal> {
    match &point.data {
        PriceData::Aggregate { price, .. } => price.to_decimal(),
        PriceData::TopOfBook { bid, ask, .. } => {
            Some((bid.to_decimal()? + ask.to_decimal()?) / Decimal::TWO)
        }
        PriceData::Trade { price, .. } => price.to_decimal(),
        _ => None,
    }
}
