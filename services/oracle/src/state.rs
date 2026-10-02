use std::collections::HashMap;
use std::sync::Arc;

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
}

impl AppState {
    pub fn new(instruments: Vec<InstrumentInfo>) -> Arc<Self> {
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
        })
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
