//! Oracle service: ingests real market data (Pyth Hermes, Binance,
//! CoinGecko), runs the pricing engine over the instrument registry, and
//! serves the results over HTTP/WSS. On-chain pushing is parked until a
//! testnet oracle program exists. `/health` is liveness; `/ready` reports
//! whether every registered instrument has been published recently, and the
//! same answer is exported as the `oracle_ready` gauge.

pub mod api;
pub mod binance;
pub mod coingecko;
pub mod config;
pub mod feed_task;
pub mod hermes;
pub mod pricing;
pub mod pusher;
pub mod state;

use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use config::{AppConfig, OracleConfig};
use oracle_client::{InstrumentInfo, Symbol};
use pricing::spec::{BaseSourceConfig, InstrumentConfig};
use state::{AppState, ReadyLimits};
use tokio::task::JoinSet;
use tracing::info;

/// Some upstream CDNs (e.g. CoinGecko) 403 requests without a User-Agent.
pub const USER_AGENT: &str = concat!("mtm-oracle/", env!("CARGO_PKG_VERSION"));

/// Upstream price APIs: a stuck request fails and the next poll retries.
pub const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
pub const HTTP_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub async fn run(cfg: AppConfig) -> anyhow::Result<()> {
    cfg.oracle.validate()?;
    let underlying_symbols: Vec<_> = cfg
        .oracle
        .underlyings
        .iter()
        .map(|u| u.symbol.clone())
        .collect();
    let mut engine = pricing::Engine::from_config(&cfg.oracle.instruments, &underlying_symbols)?;
    if let Some(path) = &cfg.oracle.state_path
        && let Some(saved) = pricing::load_state(path)
    {
        let restored = engine.restore(&saved);
        info!(path = %path.display(), restored, "pricing state restored");
    }

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
    let state = AppState::new(registry, ready_limits(&cfg.oracle));

    // Every task below runs for the life of the process: one that returns or
    // panics leaves the API serving a frozen map, so it fails the process
    // (non-zero exit) and the supervisor restarts it.
    let mut tasks = JoinSet::new();
    let feeds = config::partition_underlyings(&cfg.oracle.underlyings)?;
    let (tick_tx, tick_rx) = tokio::sync::mpsc::channel(1024);
    if !feeds.pyth.is_empty() {
        let feed = feed_task::run(tick_tx.clone(), cfg.oracle.hermes.clone(), feeds.pyth);
        tasks.spawn(named("hermes feed", feed));
    }
    if !feeds.coingecko.is_empty() {
        let feed = coingecko::run(
            tick_tx.clone(),
            cfg.oracle.coingecko.clone(),
            feeds.coingecko,
        );
        tasks.spawn(named("coingecko feed", feed));
    }
    if !feeds.binance.is_empty() {
        let feed = binance::run(tick_tx.clone(), cfg.oracle.binance.clone(), feeds.binance);
        tasks.spawn(named("binance feed", feed));
    }
    drop(tick_tx);
    tasks.spawn(named(
        "pricing engine",
        pricing::run(
            engine,
            tick_rx,
            state.clone(),
            cfg.oracle.state_path.clone(),
        ),
    ));
    tasks.spawn(named("readiness gauges", record_readiness(state.clone())));

    if cfg.oracle.pusher.enabled {
        // Not supervised: the parked stub returns straight away.
        tokio::spawn(pusher::run(cfg.oracle.pusher.clone(), cfg.rpc.clone()));
    } else {
        info!("on-chain pusher disabled (parked)");
    }

    let app = api::router(state);
    let listener = tokio::net::TcpListener::bind(cfg.oracle.listen_addr).await?;
    info!(addr = %cfg.oracle.listen_addr, "oracle api listening");
    let server = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal());
    supervise(tasks, async move { server.await }).await
}

/// How often the `/ready` gauges are refreshed.
const READINESS_INTERVAL: Duration = Duration::from_secs(1);

async fn record_readiness(state: std::sync::Arc<AppState>) {
    let mut tick = tokio::time::interval(READINESS_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        state.record_readiness(mtm_common::time::now_us()).await;
    }
}

async fn named<F: Future<Output = ()>>(name: &'static str, task: F) -> &'static str {
    task.await;
    name
}

/// Runs `server` until it returns; any task in `tasks` ending first (return
/// or panic) is an error.
async fn supervise(
    mut tasks: JoinSet<&'static str>,
    server: impl Future<Output = std::io::Result<()>>,
) -> anyhow::Result<()> {
    tokio::select! {
        served = server => Ok(served?),
        Some(exited) = tasks.join_next() => match exited {
            Ok(name) => anyhow::bail!("{name} task exited"),
            Err(e) if e.is_panic() => anyhow::bail!("oracle task panicked: {e}"),
            Err(e) => anyhow::bail!("oracle task stopped: {e}"),
        },
    }
}

/// `/ready` limits: `ready_max_age_ms`, raised to twice the cadence for
/// instruments that are legitimately published less often (a CoinGecko
/// poll, a slow GBM clock), so a healthy slow source never reads as down.
fn ready_limits(cfg: &OracleConfig) -> ReadyLimits {
    let default = Duration::from_millis(cfg.ready_max_age_ms);
    let cadence_by_feed: HashMap<&Symbol, u64> = cfg
        .underlyings
        .iter()
        .map(|u| {
            let ms = if u.pyth_feed_id.is_some() {
                cfg.hermes.poll_interval_ms
            } else if u.coingecko_id.is_some() {
                cfg.coingecko.poll_interval_ms
            } else {
                cfg.binance.confirm_interval_ms
            };
            (&u.symbol, ms)
        })
        .collect();
    let mut limits = ReadyLimits::uniform(default);
    for inst in &cfg.instruments {
        let Some(symbol) = inst.resolved_symbol() else {
            continue;
        };
        let limit = default.max(Duration::from_millis(
            instrument_cadence_ms(inst, &cadence_by_feed).saturating_mul(2),
        ));
        if limit > default {
            limits.per_symbol.insert(symbol, limit);
        }
    }
    limits
}

fn instrument_cadence_ms(inst: &InstrumentConfig, cadence_by_feed: &HashMap<&Symbol, u64>) -> u64 {
    let own = match &inst.base {
        BaseSourceConfig::Gbm { tick_ms, .. } => *tick_ms,
        _ => 0,
    };
    inst.base
        .feeds()
        .into_iter()
        .filter_map(|feed| cadence_by_feed.get(feed).copied())
        .fold(own, u64::max)
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    info!("shutdown signal received");
}

#[cfg(test)]
mod tests {
    use figment::Figment;
    use figment::providers::{Format, Toml};

    use super::*;

    fn pending_server() -> impl Future<Output = std::io::Result<()>> {
        std::future::pending()
    }

    #[tokio::test]
    async fn a_task_that_returns_fails_the_process() {
        let mut tasks = JoinSet::new();
        tasks.spawn(named("forever", std::future::pending()));
        tasks.spawn(named("binance feed", async {}));
        let err = supervise(tasks, pending_server())
            .await
            .expect_err("task exit is an error");
        assert_eq!(err.to_string(), "binance feed task exited");
    }

    #[tokio::test]
    async fn a_task_that_panics_fails_the_process() {
        let mut tasks = JoinSet::new();
        // tokio::time::interval(0) panics like this inside a feed task.
        tasks.spawn(named("hermes feed", async {
            let _ = tokio::time::interval(Duration::ZERO);
        }));
        let err = supervise(tasks, pending_server())
            .await
            .expect_err("panic is an error");
        assert!(err.to_string().contains("panicked"), "{err}");
    }

    #[tokio::test]
    async fn a_clean_server_shutdown_is_ok() {
        let mut tasks = JoinSet::new();
        tasks.spawn(named("forever", std::future::pending()));
        supervise(tasks, async { Ok(()) })
            .await
            .expect("shutdown is clean");
    }

    #[test]
    fn slow_sources_get_a_proportionally_longer_ready_limit() {
        let cfg: AppConfig = Figment::new()
            .merge(Toml::string(
                r#"
                [rpc]
                http_url = "http://localhost:8899"
                ws_url = "ws://localhost:8900"

                [oracle]
                listen_addr = "127.0.0.1:8080"

                [oracle.hermes]
                base_url = "https://hermes.pyth.network"
                poll_interval_ms = 1000

                [oracle.coingecko]
                poll_interval_ms = 300000

                [[oracle.underlyings]]
                symbol = "SOL/USD"
                pyth_feed_id = "ef0d"

                [[oracle.underlyings]]
                symbol = "SOLCG/USD"
                coingecko_id = "solana"

                [[oracle.instruments]]
                symbol = "tSOL/USD"
                base = { kind = "underlying", feed = "SOL/USD" }

                [[oracle.instruments]]
                symbol = "tSOLCG/USD"
                base = { kind = "cross", base = "SOLCG/USD", quote = "SOL/USD" }

                [[oracle.instruments]]
                symbol = "tSLOW/USD"
                base = { kind = "gbm", initial = "1", daily_vol_bps = 100, seed = 1, tick_ms = 60000 }
                "#,
            ))
            .extract()
            .expect("config parses");
        let limits = ready_limits(&cfg.oracle);
        assert_eq!(limits.default, Duration::from_secs(15));
        assert_eq!(
            limits.per_symbol,
            HashMap::from([
                (Symbol::new("tSOLCG/USD"), Duration::from_secs(600)),
                (Symbol::new("tSLOW/USD"), Duration::from_secs(120)),
            ])
        );
    }
}
