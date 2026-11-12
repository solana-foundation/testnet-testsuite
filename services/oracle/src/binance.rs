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
//!
//! bookTicker sends no snapshot on subscribe, so a fresh connection would
//! know nothing about a quiet book until it next changed. Each connection is
//! therefore seeded with one REST `GET /api/v3/ticker/bookTicker` for every
//! symbol, fetched once the stream is live; a symbol the stream already
//! delivered keeps the stream's (newer) book. REST bookTicker carries no
//! update id, so there is no diff to reconcile beyond that.

use std::collections::HashMap;
use std::fmt::Display;
use std::future::Future;
use std::time::{Duration, Instant};

use futures::{Sink, SinkExt, Stream, StreamExt};
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
        top_of_book(&self.bid, &self.bid_qty, &self.ask, &self.ask_qty)
    }
}

/// One entry of the REST `GET /api/v3/ticker/bookTicker?symbols=[...]` reply.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestBookTicker {
    pub symbol: String,
    pub bid_price: String,
    pub bid_qty: String,
    pub ask_price: String,
    pub ask_qty: String,
}

impl RestBookTicker {
    pub fn to_price_data(&self) -> Option<PriceData> {
        top_of_book(
            &self.bid_price,
            &self.bid_qty,
            &self.ask_price,
            &self.ask_qty,
        )
    }
}

fn top_of_book(bid: &str, bid_qty: &str, ask: &str, ask_qty: &str) -> Option<PriceData> {
    Some(PriceData::TopOfBook {
        bid: bid.parse::<Price>().ok()?,
        bid_qty: bid_qty.parse::<Price>().ok()?,
        ask: ask.parse::<Price>().ok()?,
        ask_qty: ask_qty.parse::<Price>().ok()?,
    })
}

/// REST snapshot of every subscribed book, used to seed a new connection.
pub struct SnapshotClient {
    http: reqwest::Client,
    url: String,
    /// The `symbols` query value: a JSON array of Binance symbols.
    symbols: String,
    symbol_by_binance: HashMap<String, Symbol>,
}

impl SnapshotClient {
    pub fn new(cfg: &BinanceConfig, feeds: &[(Symbol, String)]) -> Self {
        let symbol_by_binance: HashMap<String, Symbol> = feeds
            .iter()
            .map(|(symbol, b)| (b.to_uppercase(), symbol.clone()))
            .collect();
        let mut binance: Vec<&String> = symbol_by_binance.keys().collect();
        binance.sort();
        Self {
            http: reqwest::Client::builder()
                .user_agent(crate::USER_AGENT)
                .timeout(crate::HTTP_TIMEOUT)
                .connect_timeout(crate::HTTP_CONNECT_TIMEOUT)
                .build()
                .unwrap_or_default(),
            url: format!(
                "{}/api/v3/ticker/bookTicker",
                cfg.rest_url.trim_end_matches('/')
            ),
            symbols: serde_json::to_string(&binance).unwrap_or_default(),
            symbol_by_binance,
        }
    }

    /// Every subscribed book, keyed by our symbol. Unknown or unparseable
    /// entries are skipped.
    pub async fn fetch(&self) -> anyhow::Result<Vec<(Symbol, PriceData)>> {
        let tickers: Vec<RestBookTicker> = self
            .http
            .get(&self.url)
            .query(&[("symbols", self.symbols.as_str())])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(tickers
            .iter()
            .filter_map(|t| {
                let symbol = self.symbol_by_binance.get(&t.symbol)?;
                let Some(data) = t.to_price_data() else {
                    warn!(%symbol, "unparseable binance snapshot prices");
                    return None;
                };
                Some((symbol.clone(), data))
            })
            .collect())
    }

    /// `fetch`, with a failure logged and counted: the connection then runs
    /// on stream updates alone.
    async fn seed(&self) -> Vec<(Symbol, PriceData)> {
        match self.fetch().await {
            Ok(books) => books,
            Err(e) => {
                metrics::counter!("oracle_binance_snapshot_failures_total").increment(1);
                warn!(error = %e, "binance book snapshot failed; quiet books wait for their next change");
                Vec::new()
            }
        }
    }
}

/// Why a connection loop ended.
#[derive(Debug, PartialEq, Eq)]
enum Ended {
    /// The connection failed or went silent: reconnect.
    Reconnect,
    /// The pricing engine dropped its receiver: stop the task.
    EngineGone,
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
    let snapshots = SnapshotClient::new(&cfg, &feeds);

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
        let (sink, stream) = ws.split();
        // Subscribed before the snapshot is taken, so no change between the
        // two is lost.
        let ended = run_connection(
            stream,
            sink,
            snapshots.seed(),
            &symbol_by_stream,
            &cfg,
            &ticks,
        )
        .await;
        if ended == Ended::EngineGone {
            warn!("pricing engine gone, binance task stopping");
            return;
        }
        metrics::counter!("oracle_binance_reconnects_total").increment(1);
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// One connection's life: books from `stream` (and the `seed` snapshot, for
/// symbols the stream has not delivered yet) are coalesced into `ticks`,
/// pings go out on `sink`, quiet books are re-confirmed while the connection
/// is alive, and a silent connection ends the loop.
async fn run_connection<St, E, Si, F>(
    mut stream: St,
    mut sink: Si,
    seed: F,
    symbol_by_stream: &HashMap<String, Symbol>,
    cfg: &BinanceConfig,
    ticks: &mpsc::Sender<PricePoint>,
) -> Ended
where
    St: Stream<Item = Result<Message, E>> + Unpin,
    E: Display,
    Si: Sink<Message> + Unpin,
    F: Future<Output = Vec<(Symbol, PriceData)>>,
{
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
    let mut ping = tokio::time::interval(Duration::from_millis(cfg.ping_interval_ms));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut confirm = tokio::time::interval(Duration::from_millis(cfg.confirm_interval_ms));
    confirm.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tokio::pin!(seed);
    let mut seeded = false;

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
                        return Ended::Reconnect;
                    }
                }
                Some(Ok(Message::Pong(_))) => last_alive = Instant::now(),
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    warn!(error = %e, "binance ws error, reconnecting");
                    return Ended::Reconnect;
                }
                None => {
                    warn!("binance ws closed, reconnecting");
                    return Ended::Reconnect;
                }
            },
            snapshot = &mut seed, if !seeded => {
                seeded = true;
                let mut seeded_symbols = 0;
                for (symbol, data) in snapshot {
                    // The stream's book for a symbol is at least as new.
                    if books.contains_key(&symbol) {
                        continue;
                    }
                    seeded_symbols += 1;
                    books.insert(symbol.clone(), data.clone());
                    latest.insert(symbol.clone(), point(&symbol, data));
                }
                debug!(seeded_symbols, "binance books seeded from snapshot");
            }
            _ = idle_check.tick() => {
                let silent = last_alive.elapsed();
                if silent >= idle_timeout {
                    metrics::counter!("oracle_binance_idle_reconnects_total").increment(1);
                    warn!(silent_s = silent.as_secs(), "binance connection silent, reconnecting");
                    return Ended::Reconnect;
                }
            }
            _ = ping.tick() => {
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                    return Ended::Reconnect;
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
                        return Ended::EngineGone;
                    }
                }
            }
        }
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

    /// Fast intervals so a connection's first flush and confirm land within
    /// milliseconds of real time.
    fn fast_cfg() -> BinanceConfig {
        BinanceConfig {
            flush_interval_ms: 20,
            confirm_interval_ms: 50,
            ..BinanceConfig::default()
        }
    }

    fn streams(symbols: &[(&Symbol, &str)]) -> HashMap<String, Symbol> {
        symbols
            .iter()
            .map(|(symbol, b)| (format!("{b}@bookTicker"), (*symbol).clone()))
            .collect()
    }

    fn bid(point: &PricePoint) -> Price {
        let PriceData::TopOfBook { bid, .. } = point.data else {
            panic!("top of book");
        };
        bid
    }

    #[tokio::test]
    async fn a_new_connection_publishes_every_book_from_the_snapshot() {
        let jup = Symbol::new("JUP/USD");
        let ray = Symbol::new("RAY/USD");
        let by_stream = streams(&[(&jup, "jupusdt"), (&ray, "rayusdt")]);
        let (tx, mut rx) = mpsc::channel(16);
        // A quiet market: the stream delivers no book frames at all.
        let seed = {
            let (jup, ray) = (jup.clone(), ray.clone());
            async move { vec![(jup, book("0.5", "0.51")), (ray, book("2.0", "2.01"))] }
        };
        let cfg = fast_cfg();
        let conn = tokio::spawn(async move {
            run_connection(
                futures::stream::pending::<Result<Message, String>>(),
                futures::sink::drain(),
                seed,
                &by_stream,
                &cfg,
                &tx,
            )
            .await
        });
        let mut seen = HashMap::new();
        while seen.len() < 2 {
            let point = tokio::time::timeout(Duration::from_millis(500), rx.recv())
                .await
                .expect("every snapshot book is published within one flush")
                .expect("connection alive");
            assert_eq!(point.source, PriceSource::Binance);
            seen.insert(point.symbol.clone(), bid(&point));
        }
        assert_eq!(seen[&jup], "0.5".parse::<Price>().expect("price"));
        assert_eq!(seen[&ray], "2.0".parse::<Price>().expect("price"));
        // And the seeded books keep being re-confirmed while the connection
        // is alive.
        let again = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("seeded books are re-confirmed")
            .expect("connection alive");
        assert!(again.symbol == jup || again.symbol == ray);
        conn.abort();
    }

    #[tokio::test]
    async fn a_book_the_stream_delivered_first_beats_the_snapshot() {
        let jup = Symbol::new("JUP/USD");
        let ray = Symbol::new("RAY/USD");
        let by_stream = streams(&[(&jup, "jupusdt"), (&ray, "rayusdt")]);
        let frame = r#"{"stream":"jupusdt@bookTicker","data":
            {"u":1,"s":"JUPUSDT","b":"0.60","B":"1","a":"0.61","A":"1"}}"#;
        let stream = futures::stream::iter([Ok::<_, String>(Message::Text(frame.into()))])
            .chain(futures::stream::pending());
        // The snapshot lands after the stream's JUP frame, with an older JUP.
        let seed = {
            let (jup, ray) = (jup.clone(), ray.clone());
            async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                vec![(jup, book("0.5", "0.51")), (ray, book("2.0", "2.01"))]
            }
        };
        let (tx, mut rx) = mpsc::channel(64);
        let cfg = fast_cfg();
        let conn = tokio::spawn(async move {
            run_connection(stream, futures::sink::drain(), seed, &by_stream, &cfg, &tx).await
        });
        // Collect everything published over a few flushes and confirms.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
        let mut points = Vec::new();
        while let Ok(Some(point)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            points.push(point);
        }
        conn.abort();
        let jup_bids: Vec<Price> = points.iter().filter(|p| p.symbol == jup).map(bid).collect();
        assert!(!jup_bids.is_empty());
        let stream_bid: Price = "0.60".parse().expect("price");
        assert!(jup_bids.iter().all(|b| *b == stream_bid), "{jup_bids:?}");
        assert!(points.iter().any(|p| p.symbol == ray), "RAY seeded");
    }

    #[tokio::test]
    async fn a_failed_snapshot_leaves_the_stream_running() {
        let jup = Symbol::new("JUP/USD");
        let by_stream = streams(&[(&jup, "jupusdt")]);
        let frame = r#"{"stream":"jupusdt@bookTicker","data":
            {"u":1,"s":"JUPUSDT","b":"0.60","B":"1","a":"0.61","A":"1"}}"#;
        let stream = futures::stream::iter([Ok::<_, String>(Message::Text(frame.into()))])
            .chain(futures::stream::pending());
        let (tx, mut rx) = mpsc::channel(16);
        let cfg = fast_cfg();
        let conn = tokio::spawn(async move {
            run_connection(
                stream,
                futures::sink::drain(),
                async { Vec::new() },
                &by_stream,
                &cfg,
                &tx,
            )
            .await
        });
        let point = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("stream book published")
            .expect("connection alive");
        assert_eq!(point.symbol, jup);
        conn.abort();
    }

    #[tokio::test]
    async fn a_closed_stream_ends_the_connection_for_a_reconnect() {
        let (tx, _rx) = mpsc::channel(16);
        let ended = run_connection(
            futures::stream::empty::<Result<Message, String>>(),
            futures::sink::drain(),
            std::future::pending(),
            &HashMap::new(),
            &fast_cfg(),
            &tx,
        )
        .await;
        assert_eq!(ended, Ended::Reconnect);
    }

    #[tokio::test]
    async fn snapshot_client_requests_every_symbol_and_maps_them_back() {
        use axum::extract::Query;
        use axum::routing::get;

        async fn book_ticker(Query(q): Query<HashMap<String, String>>) -> String {
            // Only answer the exact documented request shape.
            assert_eq!(
                q.get("symbols").map(String::as_str),
                Some(r#"["JUPUSDT","RAYUSDT"]"#)
            );
            r#"[{"symbol":"JUPUSDT","bidPrice":"0.50000000","bidQty":"10.0","askPrice":"0.51000000","askQty":"12.0"},
                {"symbol":"RAYUSDT","bidPrice":"2.00000000","bidQty":"3.0","askPrice":"2.01000000","askQty":"4.0"},
                {"symbol":"OTHERUSDT","bidPrice":"1","bidQty":"1","askPrice":"1","askQty":"1"}]"#
                .to_string()
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let app = axum::Router::new().route("/api/v3/ticker/bookTicker", get(book_ticker));
        tokio::spawn(async move { axum::serve(listener, app).await });

        let cfg = BinanceConfig {
            rest_url: format!("http://{addr}/"),
            ..BinanceConfig::default()
        };
        let jup = Symbol::new("JUP/USD");
        let ray = Symbol::new("RAY/USD");
        // Config casing does not matter: REST symbols are upper case.
        let client = SnapshotClient::new(
            &cfg,
            &[
                (jup.clone(), "jupusdt".to_string()),
                (ray.clone(), "RAYUSDT".to_string()),
            ],
        );
        let books: HashMap<Symbol, PriceData> = client
            .fetch()
            .await
            .expect("snapshot")
            .into_iter()
            .collect();
        assert_eq!(books.len(), 2);
        assert_eq!(
            format!("{:?}", books[&jup]),
            format!(
                "{:?}",
                PriceData::TopOfBook {
                    bid: "0.50000000".parse().expect("price"),
                    bid_qty: "10.0".parse().expect("qty"),
                    ask: "0.51000000".parse().expect("price"),
                    ask_qty: "12.0".parse().expect("qty"),
                }
            )
        );
        assert!(books.contains_key(&ray));

        // A failing endpoint is an empty seed, not a dead connection.
        let missing = SnapshotClient::new(
            &BinanceConfig {
                rest_url: format!("http://{addr}/nope"),
                ..BinanceConfig::default()
            },
            &[(jup, "JUPUSDT".to_string())],
        );
        assert!(missing.fetch().await.is_err());
        assert!(missing.seed().await.is_empty());
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
