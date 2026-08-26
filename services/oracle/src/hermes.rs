//! Minimal Pyth Hermes REST client (pull-oracle price service).
//! API reference: https://hermes.pyth.network/docs
//!
//! The `binary` blobs returned here are what eventually gets posted on-chain
//! through the Pyth receiver program (see pusher).

use oracle_client::Price;
use serde::Deserialize;

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
    api_key: Option<String>,
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
    #[serde(default)]
    pub metadata: Option<HermesMetadata>,
}

/// Mantissas arrive as decimal strings (Hermes serializes them that way to
/// avoid precision loss); `expo` applies to both price and conf.
#[derive(Debug, Deserialize)]
pub struct HermesPrice {
    pub price: String,
    pub conf: String,
    pub expo: i32,
    pub publish_time: i64,
}

#[derive(Debug, Deserialize)]
pub struct HermesMetadata {
    #[serde(default)]
    pub slot: Option<u64>,
    #[serde(default)]
    pub proof_available_time: Option<i64>,
    #[serde(default)]
    pub prev_publish_time: Option<i64>,
}

impl HermesPrice {
    pub fn price(&self) -> Result<Price, HermesError> {
        let mantissa: i64 = self
            .price
            .parse()
            .map_err(|_| HermesError::BadMantissa(self.price.clone()))?;
        Ok(Price::from_pyth(mantissa, self.expo))
    }

    pub fn conf(&self) -> Result<Price, HermesError> {
        let mantissa: u64 = self
            .conf
            .parse()
            .map_err(|_| HermesError::BadMantissa(self.conf.clone()))?;
        Ok(Price::new(mantissa.into(), self.expo))
    }

    pub fn publish_time_us(&self) -> i64 {
        self.publish_time.saturating_mul(1_000_000)
    }
}

impl HermesClient {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .user_agent(crate::USER_AGENT)
                .build()
                .unwrap_or_default(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            // empty string (e.g. unset env passthrough) means "no key"
            api_key: api_key.filter(|k| !k.is_empty()),
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
        let mut req = self.http.get(url).query(&query);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await?.error_for_status()?;
        Ok(resp.json().await?)
    }
}

/// Normalize a feed id for map lookups: lowercase, no 0x prefix.
pub fn normalize_feed_id(id: &str) -> String {
    id.trim_start_matches("0x").to_lowercase()
}
