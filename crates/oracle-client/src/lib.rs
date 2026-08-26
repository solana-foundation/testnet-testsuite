//! Typed client for the mtm oracle service (HTTP + WSS).
//!
//! This crate also owns the oracle API *wire types* — the oracle service
//! depends on it for them, so client and server can never drift apart.

pub mod types;

use futures::StreamExt;
use futures::stream::BoxStream;
pub use mtm_common::Symbol;
pub use mtm_math::Price;
use tokio_tungstenite::tungstenite::Message;
pub use types::{InstrumentInfo, PriceData, PricePoint, PriceSource};

#[derive(Debug, thiserror::Error)]
pub enum OracleClientError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("websocket error: {0}")]
    Ws(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("decode error: {0}")]
    Decode(#[from] serde_json::Error),
}

#[derive(Debug, Clone)]
pub struct OracleClient {
    http: reqwest::Client,
    base_url: String,
}

impl OracleClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }

    pub async fn health(&self) -> Result<(), OracleClientError> {
        self.http
            .get(format!("{}/health", self.base_url))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn prices(&self) -> Result<Vec<PricePoint>, OracleClientError> {
        let resp = self
            .http
            .get(format!("{}/v1/prices", self.base_url))
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.json().await?)
    }

    pub async fn price(&self, symbol: &Symbol) -> Result<PricePoint, OracleClientError> {
        let resp = self
            .http
            .get(format!("{}/v1/price", self.base_url))
            .query(&[("symbol", symbol.as_str())])
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.json().await?)
    }

    /// Lookup by testnet mint address (base58).
    pub async fn price_by_mint(&self, mint: &str) -> Result<PricePoint, OracleClientError> {
        let resp = self
            .http
            .get(format!("{}/v1/price", self.base_url))
            .query(&[("mint", mint)])
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.json().await?)
    }

    /// The instrument registry.
    pub async fn instruments(&self) -> Result<Vec<InstrumentInfo>, OracleClientError> {
        let resp = self
            .http
            .get(format!("{}/v1/instruments", self.base_url))
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.json().await?)
    }

    /// Subscribe to the live price stream over websocket.
    pub async fn subscribe(
        &self,
    ) -> Result<BoxStream<'static, Result<PricePoint, OracleClientError>>, OracleClientError> {
        let (socket, _) = tokio_tungstenite::connect_async(self.ws_url()).await?;
        let stream = socket.filter_map(|msg| async {
            match msg {
                Ok(Message::Text(text)) => {
                    Some(serde_json::from_str::<PricePoint>(text.as_str()).map_err(Into::into))
                }
                Ok(_) => None,
                Err(e) => Some(Err(e.into())),
            }
        });
        Ok(stream.boxed())
    }

    fn ws_url(&self) -> String {
        // http -> ws, https -> wss
        let base = self.base_url.replacen("http", "ws", 1);
        format!("{base}/v1/ws")
    }
}
