//! Binance spot top-of-book websocket — the real-time, keyless source.
//!
//! Subscribes `<symbol>@bookTicker` on a combined stream. Prices/quantities
//! arrive as decimal strings (parsed straight into fixed point, never through
//! f64). Spot bookTicker carries NO timestamp — `publish_time_us` is `None`
//! by design; consumers fall back to `received_at_us`. Updates fire per BBO
//! change (can be hundreds/sec), so the task coalesces to at most one tick
//! per symbol per flush interval. Binance drops connections every 24h and
//! pings every few minutes — the task pongs and reconnects with backoff.
//!
//! Liveness is per connection, not per symbol: bookTicker only sends BBO
//! changes, so a thin book can be quiet for a minute while perfectly
//! current. The task pings every `ping_interval_ms`; any frame (book, ping,
//! pong) is a sign of life, and a connection silent for `idle_timeout_ms`
//! is reconnected (a stalled socket can stay open for minutes before TCP
//! gives up; the check runs every second, not on the ping tick). While the
//! connection showed life within `confirm_max_silence_ms` (about two ping
//! round trips), each symbol's last book is re-published every
//! `confirm_interval_ms` with a fresh `received_at_us`, so consumers'
//! staleness checks measure the feed, not the market's quietness. A stall
//! therefore stops being vouched for after `confirm_max_silence_ms`, well
//! before the reconnect. Books are forgotten on reconnect: nothing is
//! confirmed that this connection did not deliver.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use oracle_client::{Price, PriceData, PricePoint, PriceSource, Symbol};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::config::BinanceConfig;

/// How often the idle timeout is checked, independent of the ping cadence.
const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(1);

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
        // every book this connection delivered, for re-confirmation
        let mut books: HashMap<Symbol, PriceData> = HashMap::new();
        let mut flush = tokio::time::interval(Duration::from_millis(cfg.flush_interval_ms));
        flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let idle_timeout = Duration::from_millis(cfg.idle_timeout_ms);
        let confirm_max_silence = Duration::from_millis(cfg.confirm_max_silence_ms);
        let mut last_alive = Instant::now();
        let mut idle_check = tokio::time::interval(IDLE_CHECK_INTERVAL);
        idle_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut ping = tokio::time::interval(Duration::from_millis(cfg.ping_interval_ms.max(100)));
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut confirm =
            tokio::time::interval(Duration::from_millis(cfg.confirm_interval_ms.max(100)));
        confirm.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                msg = stream.next() => match msg {
                    Some(Ok(Message::Text(text))) => {
                        last_alive = Instant::now();
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
                        books.insert(symbol.clone(), data.clone());
                        latest.insert(symbol.clone(), point(symbol, data));
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        last_alive = Instant::now();
                        if sink.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Pong(_))) => last_alive = Instant::now(),
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
                _ = idle_check.tick() => {
                    let silent = last_alive.elapsed();
                    if silent >= idle_timeout {
                        metrics::counter!("oracle_binance_idle_reconnects_total").increment(1);
                        warn!(silent_s = silent.as_secs(), "binance connection silent, reconnecting");
                        break;
                    }
                }
                _ = ping.tick() => {
                    if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                        break;
                    }
                }
                _ = confirm.tick() => {
                    let silent = last_alive.elapsed();
                    for point in confirmations(&books, &latest, silent, confirm_max_silence) {
                        metrics::counter!("oracle_binance_confirmations_total").increment(1);
                        latest.insert(point.symbol.clone(), point);
                    }
                }
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

fn point(symbol: &Symbol, data: PriceData) -> PricePoint {
    PricePoint {
        symbol: symbol.clone(),
        data,
        publish_time_us: None, // spot bookTicker is untimestamped
        received_at_us: mtm_common::time::now_us(),
        slot: None,
        source: PriceSource::Binance,
    }
}

/// Re-confirmations of unchanged books: one per known symbol without a
/// pending update, and none unless the connection showed life within
/// `max_silence` (a silent connection proves nothing, even before the idle
/// timeout gives up on it).
fn confirmations(
    books: &HashMap<Symbol, PriceData>,
    pending: &HashMap<Symbol, PricePoint>,
    silent: Duration,
    max_silence: Duration,
) -> Vec<PricePoint> {
    if silent >= max_silence {
        return Vec::new();
    }
    books
        .iter()
        .filter(|(symbol, _)| !pending.contains_key(*symbol))
        .map(|(symbol, data)| point(symbol, data.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(bid: &str, ask: &str) -> PriceData {
        PriceData::TopOfBook {
            bid: bid.parse().expect("price"),
            bid_qty: "1".parse().expect("qty"),
            ask: ask.parse().expect("price"),
            ask_qty: "1".parse().expect("qty"),
        }
    }

    #[test]
    fn quiet_books_are_reconfirmed_only_on_a_live_connection() {
        let jup = Symbol::new("JUP/USD");
        let ray = Symbol::new("RAY/USD");
        let books = HashMap::from([
            (jup.clone(), book("0.5", "0.51")),
            (ray.clone(), book("2.0", "2.01")),
        ]);
        // RAY has a fresh update pending: only JUP is re-confirmed.
        let pending = HashMap::from([(ray.clone(), point(&ray, book("2.1", "2.11")))]);
        let defaults = BinanceConfig::default();
        let max_silence = Duration::from_millis(defaults.confirm_max_silence_ms);
        let idle = Duration::from_millis(defaults.idle_timeout_ms);
        let confirmed = confirmations(&books, &pending, Duration::from_secs(2), max_silence);
        assert_eq!(confirmed.len(), 1);
        assert_eq!(confirmed[0].symbol, jup);
        // PriceData has no PartialEq; its Debug form pins the values.
        assert_eq!(
            format!("{:?}", confirmed[0].data),
            format!("{:?}", book("0.5", "0.51"))
        );
        // A quiet book on a connection that just answered a ping (one ping
        // interval of silence) is still confirmed.
        let one_ping = Duration::from_millis(defaults.ping_interval_ms);
        assert_eq!(
            confirmations(&books, &HashMap::new(), one_ping, max_silence).len(),
            2
        );
        // Silent for 11 s: still short of the 15 s reconnect, but past two
        // ping round trips, so nothing is vouched for any more.
        let silent = Duration::from_secs(11);
        assert!(silent < idle);
        assert!(confirmations(&books, &HashMap::new(), silent, max_silence).is_empty());
        // A connection silent past the timeout confirms nothing.
        assert!(confirmations(&books, &HashMap::new(), idle, max_silence).is_empty());
    }

    #[test]
    fn parses_book_ticker_envelope() {
        // shape per Binance spot docs
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
