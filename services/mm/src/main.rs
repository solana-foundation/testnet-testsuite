//! Market-maker service: runs every `[[scenario.makers]]` entry (or the
//! subset named by `--maker`) as an independent task with its own derived
//! wallet, venue, strategy, and inventory. Prices come from the oracle
//! service, so the scenario's tokens track external markets:
//!
//! - `clob-ladder` (Manifest): resting bid/ask ladders around the oracle mid,
//!   re-quoted atomically when the mid moves, skewed against inventory.
//! - `clmm-range` (Raydium CLMM / Orca): one concentrated position centred on
//!   the pool price, re-centred when the price leaves the inner band.
//! - `pool-arb` (any venue): trades the pool back to the oracle price when
//!   flow pushes it beyond a threshold.

mod arb;
mod clmm;
mod clob;
mod maker;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use mtm_chain::ChainClient;
use mtm_common::config::RpcConfig;
use mtm_scenario::{Scenario, ScenarioConfig};
use serde::Deserialize;
use tracing::info;

#[derive(Debug, Parser)]
#[command(name = "mm", about = "Run the scenario's market makers")]
struct Cli {
    /// Only run these makers (repeatable); default: all.
    #[arg(long = "maker")]
    makers: Vec<String>,
    /// Where makers persist their open positions across restarts.
    #[arg(long, default_value = ".scenario/state")]
    state_dir: PathBuf,
}

#[derive(Debug, Deserialize)]
struct AppConfig {
    rpc: RpcConfig,
    mm: MmConfig,
    scenario: ScenarioConfig,
}

#[derive(Debug, Deserialize)]
struct MmConfig {
    metrics_addr: Option<SocketAddr>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let _ = dotenvy::dotenv();
    let cli = Cli::parse();
    let cfg: AppConfig = mtm_common::config::load().map_err(|error| anyhow::anyhow!("{error}"))?;
    mtm_telemetry::init("mm", cfg.mm.metrics_addr)?;
    let chain = ChainClient::new(&cfg.rpc.http_url, &cfg.rpc.commitment)?;
    let scenario = Arc::new(Scenario::resolve(cfg.scenario, &chain).await?);
    std::fs::create_dir_all(&cli.state_dir)?;

    // Venue futures hold `&dyn Signer` across awaits (not `Send`), so the
    // makers share one thread; they are I/O-bound.
    let local = tokio::task::LocalSet::new();
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let mut tasks = Vec::new();
    for maker in &scenario.config.makers {
        if !cli.makers.is_empty() && !cli.makers.contains(&maker.name) {
            continue;
        }
        let ctx = maker::Ctx::new(
            chain.clone(),
            Arc::clone(&scenario),
            maker.clone(),
            cli.state_dir.clone(),
        )
        .await?;
        info!(maker = %maker.name, wallet = %ctx.wallet_pubkey(), venue = maker.venue.as_str(), "maker starting");
        tasks.push(local.spawn_local(maker::run(ctx, stopped.clone())));
    }
    anyhow::ensure!(!tasks.is_empty(), "no makers selected");
    local
        .run_until(async {
            shutdown_signal().await;
            info!("shutdown signal received; letting makers finish their ticks");
            let _ = stop.send(true);
            let drained = futures::future::join_all(tasks);
            if tokio::time::timeout(std::time::Duration::from_secs(60), drained)
                .await
                .is_err()
            {
                tracing::warn!("makers still busy after 60 s; exiting");
            }
        })
        .await;
    Ok(())
}

/// Ctrl-C, or SIGTERM (`docker stop`, `scenario.sh down`).
async fn shutdown_signal() {
    let mut terminate =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = async {
            match terminate.as_mut() {
                Some(signal) => { signal.recv().await; }
                None => std::future::pending::<()>().await,
            }
        } => {}
    }
}
