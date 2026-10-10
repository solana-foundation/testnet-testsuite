//! Cross-venue arbitrage searcher.
//!
//! Subscribes to every scenario pool and market account over websocket.
//! When uninformed flow (or anyone) moves a pool, the update wakes the
//! searcher, which snapshots all pools and searches cycles that start and
//! end in a token the bot holds:
//!
//! - 2 legs: the same pair on two venues (buy where cheap, sell where rich);
//! - 3 legs: triangular cycles across pairs and venues (e.g. USDC→JUP on
//!   Manifest, JUP→RAY on Orca, RAY→USDC on Raydium).
//!
//! Each cycle's output is concave in its input, so the profit-maximizing size
//! comes from a ternary search over pure snapshot quotes. The winning cycle
//! executes as one v1 transaction of top-level venue swaps. Hops are chained
//! conservatively (each leg spends the previous leg's minimum) and the final
//! leg's minimum enforces `input + min_profit`, so a stale cycle reverts
//! instead of losing money. The search is oracle-free; the oracle only values
//! profit in USD for the threshold and the metrics.

mod search;

use std::collections::HashSet;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::StreamExt;
use metrics::{counter, histogram};
use mtm_chain::ChainClient;
use mtm_common::config::RpcConfig;
use mtm_scenario::{Scenario, ScenarioConfig};
use serde::Deserialize;
use solana_account_decoder_client_types::UiAccountEncoding;
use solana_pubkey::Pubkey;
use solana_pubsub_client::nonblocking::pubsub_client::PubsubClient;
use solana_rpc_client_api::config::RpcAccountInfoConfig;
use tokio::sync::mpsc;
use tracing::{info, warn};

#[derive(Debug, Deserialize)]
struct AppConfig {
    rpc: RpcConfig,
    arb: ArbServiceConfig,
    scenario: ScenarioConfig,
}

#[derive(Debug, Deserialize)]
struct ArbServiceConfig {
    metrics_addr: Option<SocketAddr>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let _ = dotenvy::dotenv();
    let cfg: AppConfig = mtm_common::config::load().map_err(|error| anyhow::anyhow!("{error}"))?;
    mtm_telemetry::init("arb", cfg.arb.metrics_addr)?;
    let chain = ChainClient::new(&cfg.rpc.http_url, &cfg.rpc.commitment)?;
    let scenario = Scenario::resolve(cfg.scenario, &chain).await?;
    let arb = scenario
        .config
        .arb
        .clone()
        .context("the profile has no [scenario.arb] section")?;

    let mut pools = Vec::new();
    for pool in &scenario.config.pools {
        if let Some(address) = scenario
            .pool_address(&chain, pool.venue, &pool.base, &pool.quote)
            .await?
        {
            pools.push((pool.venue, address));
        }
    }
    anyhow::ensure!(
        !pools.is_empty(),
        "no scenario pools exist; run scenario bootstrap"
    );

    // One websocket subscription per pool; every update is a scan trigger.
    let (tx, mut rx) = mpsc::unbounded_channel::<Pubkey>();
    for (_, address) in &pools {
        spawn_subscription(cfg.rpc.ws_url.clone(), *address, tx.clone());
    }
    let searcher = search::Searcher::new(chain, &scenario, pools, &arb)?;
    info!(
        bots = arb.bots.len(),
        pools = searcher.pool_count(),
        "arb searcher started"
    );

    let poll = Duration::from_millis(arb.poll_ms);
    let debounce = Duration::from_millis(arb.debounce_ms);
    loop {
        let (trigger, changed) = tokio::select! {
            update = rx.recv() => match update {
                Some(pool) => {
                    // Coalesce the burst a single transaction produces.
                    tokio::time::sleep(debounce).await;
                    let mut changed = HashSet::from([pool]);
                    while let Ok(pool) = rx.try_recv() {
                        changed.insert(pool);
                    }
                    tracing::debug!(pools = changed.len(), "pool update");
                    ("pool-update", Some(changed))
                }
                None => ("poll", None),
            },
            () = tokio::time::sleep(poll) => ("poll", None),
        };
        let started = Instant::now();
        counter!("arb_scans_total", "trigger" => trigger).increment(1);
        if let Err(error) = searcher.scan_and_execute(trigger, changed.as_ref()).await {
            warn!(error = %format!("{error:#}"), "arb scan failed");
            counter!("arb_errors_total").increment(1);
        }
        histogram!("arb_scan_seconds").record(started.elapsed().as_secs_f64());
    }
}

fn spawn_subscription(ws_url: String, address: Pubkey, tx: mpsc::UnboundedSender<Pubkey>) {
    tokio::spawn(async move {
        loop {
            match subscribe(&ws_url, &address, &tx).await {
                Ok(()) => warn!(%address, "pool subscription ended; reconnecting"),
                Err(error) => warn!(%address, error = %error, "pool subscription failed"),
            }
            counter!("arb_ws_reconnects_total").increment(1);
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

async fn subscribe(
    ws_url: &str,
    address: &Pubkey,
    tx: &mpsc::UnboundedSender<Pubkey>,
) -> Result<()> {
    let client = PubsubClient::new(ws_url).await?;
    let (mut stream, _unsubscribe) = client
        .account_subscribe(
            address,
            Some(RpcAccountInfoConfig {
                encoding: Some(UiAccountEncoding::Base64),
                data_slice: Some(solana_account_decoder_client_types::UiDataSliceConfig {
                    offset: 0,
                    length: 0,
                }),
                ..RpcAccountInfoConfig::default()
            }),
        )
        .await?;
    while stream.next().await.is_some() {
        counter!("arb_pool_updates_total").increment(1);
        if tx.send(*address).is_err() {
            break;
        }
    }
    Ok(())
}
