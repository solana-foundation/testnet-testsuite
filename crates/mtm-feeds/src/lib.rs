//! Price feed sources: Pyth Hermes (pull oracle) and, later, exchange
//! websocket connectors (Binance, Coinbase, ...) via tokio-tungstenite.

pub mod hermes;

use futures::stream::BoxStream;
use mtm_common::Symbol;
use mtm_math::Price;

/// A single price observation from some source.
#[derive(Debug, Clone)]
pub struct PriceUpdate {
    pub symbol: Symbol,
    pub price: Price,
    /// Source-reported publish time, unix seconds.
    pub publish_time: i64,
    pub source: &'static str,
}

/// A source of streaming price updates (Hermes SSE, exchange ws, ...).
#[async_trait::async_trait]
pub trait PriceFeed: Send + Sync {
    fn name(&self) -> &'static str;
    async fn subscribe(
        &self,
        symbols: &[Symbol],
    ) -> anyhow::Result<BoxStream<'static, PriceUpdate>>;
}
