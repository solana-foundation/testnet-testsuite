use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use mtm_common::Symbol;
use oracle_client::{InstrumentInfo, PricePoint};
use tokio::sync::{RwLock, broadcast};

/// Latest price per instrument plus a broadcast channel feeding websocket
/// clients, and the (static) instrument registry for lookups.
pub struct AppState {
    prices: RwLock<HashMap<Symbol, PricePoint>>,
    tx: broadcast::Sender<PricePoint>,
    instruments: Vec<InstrumentInfo>,
    mint_to_symbol: HashMap<String, Symbol>,
    ready: ReadyLimits,
}

/// How recently each instrument must have been published for `/ready`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyLimits {
    pub default: Duration,
    /// Overrides for instruments on a slower cadence than `default` allows.
    pub per_symbol: HashMap<Symbol, Duration>,
}

impl ReadyLimits {
    pub fn uniform(default: Duration) -> Self {
        Self {
            default,
            per_symbol: HashMap::new(),
        }
    }

    fn for_symbol(&self, symbol: &Symbol) -> Duration {
        self.per_symbol.get(symbol).copied().unwrap_or(self.default)
    }
}

/// Why an instrument is not ready.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotReady {
    Missing(Symbol),
    Stale { symbol: Symbol, age_ms: i64 },
}

impl std::fmt::Display for NotReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotReady::Missing(symbol) => write!(f, "{symbol}: no price"),
            NotReady::Stale { symbol, age_ms } => write!(f, "{symbol}: {age_ms} ms old"),
        }
    }
}

impl AppState {
    pub fn new(instruments: Vec<InstrumentInfo>, ready: ReadyLimits) -> Arc<Self> {
        let (tx, _) = broadcast::channel(1024);
        let mint_to_symbol = instruments
            .iter()
            .filter_map(|i| i.mint.clone().map(|m| (m, i.symbol.clone())))
            .collect();
        Arc::new(Self {
            prices: RwLock::new(HashMap::new()),
            tx,
            instruments,
            mint_to_symbol,
            ready,
        })
    }

    /// Registered instruments whose latest point is missing or was published
    /// (`received_at_us`) longer ago than its readiness limit. Empty = ready.
    pub async fn not_ready(&self, now_us: i64) -> Vec<NotReady> {
        let prices = self.prices.read().await;
        self.instruments
            .iter()
            .filter_map(|info| {
                let Some(point) = prices.get(&info.symbol) else {
                    return Some(NotReady::Missing(info.symbol.clone()));
                };
                let age_us = now_us.saturating_sub(point.received_at_us);
                let limit_us = i64::try_from(self.ready.for_symbol(&info.symbol).as_micros())
                    .unwrap_or(i64::MAX);
                (age_us > limit_us).then(|| NotReady::Stale {
                    symbol: info.symbol.clone(),
                    age_ms: age_us / 1000,
                })
            })
            .collect()
    }

    /// Exports what `/ready` would answer as gauges, so a feed that retries
    /// forever (and never exits) shows up in Prometheus: `oracle_ready`
    /// (1 = every registered instrument is fresh) and
    /// `oracle_not_ready_instruments`. Returns the not-ready count.
    pub async fn record_readiness(&self, now_us: i64) -> usize {
        let not_ready = self.not_ready(now_us).await.len();
        metrics::gauge!("oracle_ready").set(if not_ready == 0 { 1.0 } else { 0.0 });
        metrics::gauge!("oracle_not_ready_instruments").set(not_ready as f64);
        not_ready
    }

    pub async fn update(&self, point: PricePoint) {
        metrics::counter!("oracle_price_updates_total", "symbol" => point.symbol.to_string())
            .increment(1);
        self.prices
            .write()
            .await
            .insert(point.symbol.clone(), point.clone());
        // send only fails when there are no ws subscribers — fine
        let _ = self.tx.send(point);
    }

    pub async fn get(&self, symbol: &Symbol) -> Option<PricePoint> {
        self.prices.read().await.get(symbol).cloned()
    }

    pub async fn all(&self) -> Vec<PricePoint> {
        self.prices.read().await.values().cloned().collect()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<PricePoint> {
        self.tx.subscribe()
    }

    pub fn instruments(&self) -> &[InstrumentInfo] {
        &self.instruments
    }

    pub fn symbol_for_mint(&self, mint: &str) -> Option<&Symbol> {
        self.mint_to_symbol.get(mint)
    }
}

#[cfg(test)]
mod tests {
    use metrics_exporter_prometheus::PrometheusBuilder;
    use oracle_client::{Price, PriceData, PriceSource};

    use super::*;

    fn info(symbol: &str) -> InstrumentInfo {
        InstrumentInfo {
            symbol: Symbol::new(symbol),
            mint: None,
            base: "peg".to_string(),
            transforms: Vec::new(),
        }
    }

    fn point(symbol: &str, received_at_us: i64) -> PricePoint {
        PricePoint {
            symbol: Symbol::new(symbol),
            data: PriceData::Aggregate {
                price: Price::new(100_000_000, -8),
                conf: None,
                ema: None,
            },
            publish_time_us: None,
            received_at_us,
            slot: None,
            source: PriceSource::Derived,
        }
    }

    fn gauge(rendered: &str, name: &str) -> String {
        rendered
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name} ")))
            .unwrap_or_else(|| panic!("{name} missing from:\n{rendered}"))
            .to_string()
    }

    // Current-thread runtime: the thread-local recorder sees every gauge.
    #[tokio::test]
    async fn readiness_gauges_follow_the_price_map() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let state = AppState::new(
            vec![info("A/USD"), info("B/USD")],
            ReadyLimits::uniform(Duration::from_secs(15)),
        );
        let now = mtm_common::time::now_us();

        assert_eq!(state.record_readiness(now).await, 2);
        let rendered = handle.render();
        assert_eq!(gauge(&rendered, "oracle_ready"), "0");
        assert_eq!(gauge(&rendered, "oracle_not_ready_instruments"), "2");

        state.update(point("A/USD", now)).await;
        state.update(point("B/USD", now - 60_000_000)).await;
        state.record_readiness(now).await;
        let rendered = handle.render();
        assert_eq!(gauge(&rendered, "oracle_ready"), "0");
        assert_eq!(gauge(&rendered, "oracle_not_ready_instruments"), "1");

        state.update(point("B/USD", now)).await;
        assert_eq!(state.record_readiness(now).await, 0);
        let rendered = handle.render();
        assert_eq!(gauge(&rendered, "oracle_ready"), "1");
        assert_eq!(gauge(&rendered, "oracle_not_ready_instruments"), "0");
    }
}
