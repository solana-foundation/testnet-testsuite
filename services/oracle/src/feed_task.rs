//! Polls Pyth Hermes and forwards underlying ticks to the pricing engine.
//! TODO: switch to Hermes SSE streaming; exchange feeds come later.

use std::collections::HashMap;
use std::time::Duration;

use oracle_client::{PriceData, PricePoint, PriceSource, Symbol};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::config::HermesConfig;
use crate::hermes::{HermesClient, normalize_feed_id};

/// Feed task: polls Hermes and forwards underlying ticks.
/// `feeds` maps our symbol → Pyth feed id.
pub async fn run(ticks: mpsc::Sender<PricePoint>, cfg: HermesConfig, feeds: Vec<(Symbol, String)>) {
    let client = HermesClient::new(&cfg.base_url, cfg.api_key.clone());
    let ids: Vec<String> = feeds.iter().map(|(_, id)| normalize_feed_id(id)).collect();
    let symbol_by_id: HashMap<String, Symbol> = feeds
        .iter()
        .map(|(symbol, id)| (normalize_feed_id(id), symbol.clone()))
        .collect();

    let mut interval = tokio::time::interval(Duration::from_millis(cfg.poll_interval_ms));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;
        let updates = match client.latest_price_updates(&ids).await {
            Ok(updates) => updates,
            Err(e) => {
                metrics::counter!("oracle_hermes_poll_errors_total").increment(1);
                warn!(error = %e, "hermes poll failed");
                continue;
            }
        };
        for update in updates.parsed {
            let Some(symbol) = symbol_by_id.get(&normalize_feed_id(&update.id)) else {
                continue;
            };
            let price = match update.price.price() {
                Ok(price) => price,
                Err(e) => {
                    warn!(%symbol, error = %e, "bad hermes price");
                    continue;
                }
            };
            debug!(%symbol, %price, "underlying tick");
            let point = PricePoint {
                symbol: symbol.clone(),
                data: PriceData::Aggregate {
                    price,
                    conf: update.price.conf().ok(),
                    ema: update.ema_price.price().ok(),
                },
                publish_time_us: Some(update.price.publish_time_us()),
                received_at_us: mtm_common::time::now_us(),
                slot: update.metadata.as_ref().and_then(|m| m.slot),
                source: PriceSource::PythHermes,
            };
            if ticks.send(point).await.is_err() {
                warn!("pricing engine gone, feed task stopping");
                return;
            }
        }
    }
}
