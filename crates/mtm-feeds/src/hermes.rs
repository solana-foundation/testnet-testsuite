//! Minimal Pyth Hermes REST client (pull-oracle price service).
//! API reference: https://hermes.pyth.network/docs
//!
//! The `binary` blobs returned here are what eventually gets posted on-chain
//! through the Pyth receiver program (see services/oracle pusher).

use mtm_math::Price;
use serde::Deserialize;

pub const DEFAULT_BASE_URL: &str = "https://hermes.pyth.network";
pub const SOURCE_NAME: &str = "pyth-hermes";

#[derive(Debug, thiserror::Error)]
pub enum HermesError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("bad price mantissa: {0}")]
    BadMantissa(String),
}

#[derive(Debug, Clone)]
pub struct HermesClient {
    http: reqwest::Client,
    base_url: String,
}

#[derive(Debug, Deserialize)]
pub struct LatestPriceUpdates {
    pub binary: BinaryData,
    #[serde(default)]
    pub parsed: Vec<ParsedPriceUpdate>,
}

#[derive(Debug, Deserialize)]
pub struct BinaryData {
    pub encoding: String,
    pub data: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct ParsedPriceUpdate {
    pub id: String,
    pub price: HermesPrice,
    pub ema_price: HermesPrice,
}

#[derive(Debug, Deserialize)]
pub struct HermesPrice {
    pub price: String,
    pub conf: String,
    pub expo: i32,
    pub publish_time: i64,
}

impl ParsedPriceUpdate {
    pub fn price(&self) -> Result<Price, HermesError> {
        let mantissa = self
            .price
            .price
            .parse()
            .map_err(|_| HermesError::BadMantissa(self.price.price.clone()))?;
        Ok(Price::new(mantissa, self.price.expo))
    }
}

impl HermesClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }

    /// Fetch the latest update for each feed id (lowercase hex, no 0x prefix).
    pub async fn latest_price_updates(
        &self,
        feed_ids: &[String],
    ) -> Result<LatestPriceUpdates, HermesError> {
        let url = format!("{}/v2/updates/price/latest", self.base_url);
        let query: Vec<(&str, &str)> = feed_ids
            .iter()
            .map(|id| ("ids[]", id.as_str()))
            .chain([("encoding", "base64"), ("parsed", "true")])
            .collect();
        let resp = self
            .http
            .get(url)
            .query(&query)
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.json().await?)
    }
}

/// Normalize a feed id for map lookups: lowercase, no 0x prefix.
pub fn normalize_feed_id(id: &str) -> String {
    id.trim_start_matches("0x").to_lowercase()
}
