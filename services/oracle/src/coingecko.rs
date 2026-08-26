//! CoinGecko `simple/price` polling — a reference-grade aggregator source.
//!
//! Demo tier limits (verified 2026-08-26): 10,000 calls/MONTH cap, 100/min
//! burst; keyless access works at low rates. One call batches every coin id,
//! so the default 5-minute poll stays inside the cap. Values arrive as JSON
//! floats (display-grade per docs/pricing-types.md) — quantized immediately
//! via `Price::from_f64_lossy`; no conf, minutes-scale freshness. Use for
//! cross-source sanity checks, not trading-grade signals.

use std::collections::HashMap;
use std::time::Duration;

use oracle_client::{Price, PriceData, PricePoint, PriceSource, Symbol};
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::config::CoinGeckoConfig;

#[derive(Debug, Clone, Deserialize)]
pub struct CgQuote {
    pub usd: f64,
    #[serde(default)]
    pub last_updated_at: Option<i64>,
}

pub struct CoinGeckoClient {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl CoinGeckoClient {
    pub fn new(cfg: &CoinGeckoConfig) -> Self {
        Self {
            http: reqwest::Client::builder()
                .user_agent(crate::USER_AGENT)
                .build()
                .unwrap_or_default(),
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            api_key: cfg.api_key.clone().filter(|k| !k.is_empty()),
        }
    }

    pub async fn simple_price(
        &self,
        ids: &[String],
    ) -> Result<HashMap<String, CgQuote>, reqwest::Error> {
        let joined = ids.join(",");
        let mut req = self
            .http
            .get(format!("{}/simple/price", self.base_url))
            .query(&[
                ("ids", joined.as_str()),
                ("vs_currencies", "usd"),
                ("precision", "full"),
                ("include_last_updated_at", "true"),
            ]);
        if let Some(key) = &self.api_key {
            req = req.header("x-cg-demo-api-key", key);
        }
        req.send().await?.error_for_status()?.json().await
    }
}

/// Feed task: polls the batch endpoint and forwards underlying ticks.
/// `feeds` maps our symbol → CoinGecko coin id.
pub async fn run(
    ticks: mpsc::Sender<PricePoint>,
    cfg: CoinGeckoConfig,
    feeds: Vec<(Symbol, String)>,
) {
    let client = CoinGeckoClient::new(&cfg);
    let ids: Vec<String> = feeds.iter().map(|(_, id)| id.clone()).collect();

    let mut interval = tokio::time::interval(Duration::from_millis(cfg.poll_interval_ms));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;
        let quotes = match client.simple_price(&ids).await {
            Ok(quotes) => quotes,
            Err(e) => {
                metrics::counter!("oracle_coingecko_poll_errors_total").increment(1);
                warn!(error = %e, "coingecko poll failed");
                continue;
            }
        };
        for (symbol, id) in &feeds {
            let Some(quote) = quotes.get(id) else {
                warn!(%symbol, id, "coingecko response missing coin id");
                continue;
            };
            let price = match Price::from_f64_lossy(quote.usd, -8) {
                Ok(price) => price,
                Err(e) => {
                    warn!(%symbol, value = quote.usd, error = %e, "bad coingecko price");
                    continue;
                }
            };
            debug!(%symbol, %price, "underlying tick");
            let point = PricePoint {
                symbol: symbol.clone(),
                data: PriceData::Aggregate {
                    price,
                    conf: None,
                    ema: None,
                },
                publish_time_us: quote.last_updated_at.map(|s| s.saturating_mul(1_000_000)),
                received_at_us: mtm_common::time::now_us(),
                slot: None,
                source: PriceSource::CoinGecko,
            };
            if ticks.send(point).await.is_err() {
                warn!("pricing engine gone, coingecko task stopping");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_live_response_shape() {
        // exact shape captured live 2026-08-26
        let json = r#"{"solana":{"usd":96.73428175996966,"last_updated_at":1787769580},
                       "bitcoin":{"usd":78384.04894687138,"last_updated_at":1787769580}}"#;
        let quotes: HashMap<String, CgQuote> = serde_json::from_str(json).expect("parses");
        assert_eq!(quotes["solana"].last_updated_at, Some(1_787_769_580));
        let price = Price::from_f64_lossy(quotes["solana"].usd, -8).expect("quantizes");
        assert_eq!(price, Price::new(9_673_428_176, -8));
    }
}
