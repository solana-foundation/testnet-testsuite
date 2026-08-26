//! Binance spot top-of-book websocket — the real-time, keyless source.
//!
//! Subscribes `<symbol>@bookTicker` on a combined stream. Prices/quantities
//! arrive as decimal strings (parsed straight into fixed point, never through
//! f64). Spot bookTicker carries NO timestamp — `publish_time_us` is `None`
//! by design; consumers fall back to `received_at_us`. Updates fire per BBO
//! change (can be hundreds/sec), so the task coalesces to at most one tick
//! per symbol per flush interval. Binance drops connections every 24h and
//! pings every ~3min — the task pongs and reconnects with backoff.

use std::collections::HashMap;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use oracle_client::{Price, PriceData, PricePoint, PriceSource, Symbol};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::config::BinanceConfig;

#[derive(Debug, Deserialize)]
struct StreamMessage {
    stream: String,
    data: BookTicker,
}

#[derive(Debug, Deserialize)]
pub struct BookTicker {
    #[serde(rename = "u")]
    pub update_id: u64,
    #[serde(rename = "b")]
    pub bid: String,
    #[serde(rename = "B")]
    pub bid_qty: String,
    #[serde(rename = "a")]
    pub ask: String,
    #[serde(rename = "A")]
    pub ask_qty: String,
}

impl BookTicker {
    pub fn to_price_data(&self) -> Option<PriceData> {
        Some(PriceData::TopOfBook {
            bid: self.bid.parse::<Price>().ok()?,
            bid_qty: self.bid_qty.parse::<Price>().ok()?,
            ask: self.ask.parse::<Price>().ok()?,
            ask_qty: self.ask_qty.parse::<Price>().ok()?,
        })
    }
}

/// Feed task. `feeds` maps our symbol → Binance spot symbol (e.g. "SOLUSDT").
pub async fn run(
    ticks: mpsc::Sender<PricePoint>,
    cfg: BinanceConfig,
    feeds: Vec<(Symbol, String)>,
) {
    let symbol_by_stream: HashMap<String, Symbol> = feeds
        .iter()
        .map(|(symbol, b)| (format!("{}@bookTicker", b.to_lowercase()), symbol.clone()))
        .collect();
    let streams: Vec<&str> = symbol_by_stream.keys().map(String::as_str).collect();
    let url = format!(
        "{}/stream?streams={}",
        cfg.ws_url.trim_end_matches('/'),
        streams.join("/")
    );

    loop {
        let ws = match tokio_tungstenite::connect_async(&url).await {
            Ok((ws, _)) => ws,
            Err(e) => {
                metrics::counter!("oracle_binance_reconnects_total").increment(1);
                warn!(error = %e, "binance ws connect failed, retrying in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        info!(streams = streams.len(), "binance ws connected");
        let (mut sink, mut stream) = ws.split();

        // latest book per symbol, flushed on the coalescing interval
        let mut latest: HashMap<Symbol, PricePoint> = HashMap::new();
        let mut flush = tokio::time::interval(Duration::from_millis(cfg.flush_interval_ms));
        flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                msg = stream.next() => match msg {
                    Some(Ok(Message::Text(text))) => {
                        let Ok(envelope) = serde_json::from_str::<StreamMessage>(text.as_str()) else {
                            continue; // subscription acks etc.
                        };
                        let Some(symbol) = symbol_by_stream.get(&envelope.stream) else {
                            continue;
                        };
                        let Some(data) = envelope.data.to_price_data() else {
                            warn!(%symbol, "unparseable binance book prices");
                            continue;
                        };
                        metrics::counter!("oracle_binance_updates_total").increment(1);
                        latest.insert(symbol.clone(), PricePoint {
                            symbol: symbol.clone(),
                            data,
                            publish_time_us: None, // spot bookTicker is untimestamped
                            received_at_us: mtm_common::time::now_us(),
                            slot: None,
                            source: PriceSource::Binance,
                        });
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if sink.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        warn!(error = %e, "binance ws error, reconnecting");
                        break;
                    }
                    None => {
                        warn!("binance ws closed, reconnecting");
                        break;
                    }
                },
                _ = flush.tick() => {
                    for (_, point) in latest.drain() {
                        debug!(symbol = %point.symbol, "underlying tick");
                        if ticks.send(point).await.is_err() {
                            warn!("pricing engine gone, binance task stopping");
                            return;
                        }
                    }
                }
            }
        }
        metrics::counter!("oracle_binance_reconnects_total").increment(1);
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_book_ticker_envelope() {
        // shape per Binance spot docs (verified in docs/pricing-types.md research)
        let json = r#"{"stream":"solusdt@bookTicker","data":
            {"u":400900217,"s":"SOLUSDT","b":"96.75000000","B":"31.21000000","a":"96.76000000","A":"40.66000000"}}"#;
        let envelope: StreamMessage = serde_json::from_str(json).expect("parses");
        assert_eq!(envelope.stream, "solusdt@bookTicker");
        let Some(PriceData::TopOfBook { bid, ask, .. }) = envelope.data.to_price_data() else {
            panic!("top of book");
        };
        assert_eq!(bid, Price::new(9_675_000_000, -8));
        assert_eq!(ask, Price::new(9_676_000_000, -8));
    }
}
