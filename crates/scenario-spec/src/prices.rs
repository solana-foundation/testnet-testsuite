//! USD prices for rendering, from Binance (`<TOKEN>USDT`), with overrides
//! for hosts where Binance is unreachable or geo-blocked.

use anyhow::{Context, Result};
use rust_decimal::Decimal;

use crate::render::Prices;

/// Prices for `tokens` (USDC is pegged): `overrides` first, Binance for the rest.
pub async fn fetch(
    client: &reqwest::Client,
    tokens: &[&str],
    overrides: &Prices,
) -> Result<Prices> {
    let mut prices = Prices::new();
    for token in tokens.iter().copied().filter(|t| *t != "USDC") {
        if let Some(price) = overrides.get(token) {
            prices.insert(token.to_owned(), *price);
            continue;
        }
        #[derive(serde::Deserialize)]
        struct Ticker {
            price: Decimal,
        }
        let url = format!("https://api.binance.com/api/v3/ticker/price?symbol={token}USDT");
        let ticker: Ticker = client
            .get(&url)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .with_context(|| format!("no price for {token} from Binance; set SCALE_PRICES"))?
            .json()
            .await
            .with_context(|| format!("decoding Binance's {token} price"))?;
        prices.insert(token.to_owned(), ticker.price);
    }
    Ok(prices)
}

/// `SCALE_PRICES`, a JSON object such as `{"JUP": "0.32", "RAY": "1.84"}`.
pub fn overrides_from_env() -> Result<Prices> {
    match std::env::var("SCALE_PRICES") {
        Ok(json) if !json.trim().is_empty() => {
            serde_json::from_str(&json).context("SCALE_PRICES is not a JSON object of prices")
        }
        _ => Ok(Prices::new()),
    }
}
