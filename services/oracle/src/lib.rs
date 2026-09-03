//! Oracle service: ingests real market data (Pyth Hermes), runs the pricing
//! engine over the instrument registry, and serves the results over HTTP/WSS.
//! On-chain pushing is parked (docs/testnet-oracle-strategy.md).

pub mod api;
pub mod binance;
pub mod coingecko;
pub mod config;
pub mod feed_task;
pub mod hermes;
pub mod pricing;
pub mod pusher;
pub mod state;

use config::AppConfig;
use oracle_client::InstrumentInfo;
use state::AppState;
use tracing::info;

/// Some upstream CDNs (e.g. CoinGecko) 403 requests without a User-Agent.
pub const USER_AGENT: &str = concat!("mtm-oracle/", env!("CARGO_PKG_VERSION"));

pub async fn run(cfg: AppConfig) -> anyhow::Result<()> {
    let underlying_symbols: Vec<_> = cfg
        .oracle
        .underlyings
        .iter()
        .map(|u| u.symbol.clone())
        .collect();
    let engine = pricing::Engine::from_config(&cfg.oracle.instruments, &underlying_symbols)?;

    let registry: Vec<InstrumentInfo> = cfg
        .oracle
        .instruments
        .iter()
        .filter_map(|i| {
            i.resolved_symbol().map(|symbol| InstrumentInfo {
                symbol,
                mint: i.mint.clone(),
                base: i.base.kind_name().to_string(),
                transforms: i
                    .transforms
                    .iter()
                    .map(|t| t.kind_name().to_string())
                    .collect(),
            })
        })
        .collect();
    let state = AppState::new(registry);

    let feeds = config::partition_underlyings(&cfg.oracle.underlyings)?;
    let (tick_tx, tick_rx) = tokio::sync::mpsc::channel(1024);
    if !feeds.pyth.is_empty() {
        tokio::spawn(feed_task::run(
            tick_tx.clone(),
            cfg.oracle.hermes.clone(),
            feeds.pyth,
        ));
    }
    if !feeds.coingecko.is_empty() {
        tokio::spawn(coingecko::run(
            tick_tx.clone(),
            cfg.oracle.coingecko.clone(),
            feeds.coingecko,
        ));
    }
    if !feeds.binance.is_empty() {
        tokio::spawn(binance::run(
            tick_tx.clone(),
            cfg.oracle.binance.clone(),
            feeds.binance,
        ));
    }
    drop(tick_tx);
    tokio::spawn(pricing::run(engine, tick_rx, state.clone()));

    if cfg.oracle.pusher.enabled {
        tokio::spawn(pusher::run(cfg.oracle.pusher.clone(), cfg.rpc.clone()));
    } else {
        info!("on-chain pusher disabled (parked)");
    }

    let app = api::router(state);
    let listener = tokio::net::TcpListener::bind(cfg.oracle.listen_addr).await?;
    info!(addr = %cfg.oracle.listen_addr, "oracle api listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    info!("shutdown signal received");
}
