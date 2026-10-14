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
//!
//! Scans never wait on confirmations: each planned cycle runs as its own
//! future next to the scan loop, and a bot with a cycle in flight sits out
//! later scans until it resolves. Accounting values every token the cycle
//! touched, from a read pinned to its slot, with gains, losses and fees on
//! separate counters.

mod search;

use std::collections::HashSet;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use metrics::{counter, gauge, histogram};
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
    drop(tx);
    let searcher = search::Searcher::new(chain, &scenario, pools, &arb)?;
    info!(
        bots = searcher.bot_count(),
        pools = searcher.pool_count(),
        "arb searcher started"
    );

    // A full reload on a fixed cadence, whether or not updates arrive (a
    // steady update stream would otherwise postpone it forever).
    let mut poll = tokio::time::interval(Duration::from_millis(arb.poll_ms));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let debounce = Duration::from_millis(arb.debounce_ms);
    let mut in_flight = FuturesUnordered::new();
    let mut busy: HashSet<usize> = HashSet::new();
    loop {
        let (trigger, changed) = tokio::select! {
            update = rx.recv() => {
                let pool = update.context("every pool subscription task ended")?;
                // Coalesce the burst a single transaction produces.
                tokio::time::sleep(debounce).await;
                let mut changed = HashSet::from([pool]);
                while let Ok(pool) = rx.try_recv() {
                    changed.insert(pool);
                }
                tracing::debug!(pools = changed.len(), "pool update");
                ("pool-update", Some(changed))
            }
            _ = poll.tick() => ("poll", None),
            Some(bot) = in_flight.next(), if !in_flight.is_empty() => {
                busy.remove(&bot);
                gauge!("arb_in_flight").set(busy.len() as f64);
                continue;
            }
        };
        let started = Instant::now();
        counter!("arb_scans_total", "trigger" => trigger).increment(1);
        let idle = busy.clone();
        let scan = searcher.scan(trigger, changed.as_ref(), &idle);
        tokio::pin!(scan);
        // Keep driving in-flight cycles while the scan runs.
        let plans = loop {
            tokio::select! {
                plans = &mut scan => break plans,
                Some(bot) = in_flight.next(), if !in_flight.is_empty() => {
                    busy.remove(&bot);
                }
            }
        };
        histogram!("arb_scan_seconds").record(started.elapsed().as_secs_f64());
        match plans {
            Ok(plans) => {
                for plan in plans {
                    busy.insert(plan.bot);
                    in_flight.push(searcher.execute(plan));
                }
            }
            Err(error) => {
                warn!(error = %format!("{error:#}"), "arb scan failed");
                counter!("arb_errors_total").increment(1);
            }
        }
        gauge!("arb_in_flight").set(busy.len() as f64);
    }
}

fn spawn_subscription(ws_url: String, address: Pubkey, tx: mpsc::UnboundedSender<Pubkey>) {
    tokio::spawn(async move {
        let mut failures: u32 = 0;
        loop {
            match subscribe(&ws_url, &address, &tx).await {
                Ok(()) if tx.is_closed() => return,
                Ok(()) => {
                    failures = 0;
                    warn!(%address, "pool subscription ended; reconnecting");
                }
                Err(error) => {
                    failures = failures.saturating_add(1);
                    warn!(%address, error = %error, "pool subscription failed");
                }
            }
            counter!("arb_ws_reconnects_total").increment(1);
            tokio::time::sleep(backoff(failures)).await;
        }
    });
}

/// 1-2 s after a clean end, doubling per consecutive failure up to 30 s,
/// with jitter so every pool's subscription does not reconnect at once.
fn backoff(failures: u32) -> Duration {
    let base_ms = 1_000_u64.saturating_mul(1 << failures.min(5)).min(30_000);
    Duration::from_millis(base_ms + rand::random_range(0..base_ms.max(1)))
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
    // Updates during the gap before this subscription are lost: treat the
    // pool as changed so the next scan reloads it.
    if tx.send(*address).is_err() {
        return Ok(());
    }
    while stream.next().await.is_some() {
        counter!("arb_pool_updates_total").increment(1);
        if tx.send(*address).is_err() {
            break;
        }
    }
    Ok(())
}
