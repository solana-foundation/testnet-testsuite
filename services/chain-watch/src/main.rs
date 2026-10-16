//! chain-watch: a read-only observer of the cluster a scenario runs on.
//!
//! Follows every slot (skipped or produced, with its leader's client),
//! summarises each block (transactions, CU, fees, priority prices, and the
//! ones our participants sent), tracks epochs, versions and feature gates,
//! and exports it all as Prometheus metrics plus a per-run SQLite store
//! (`<run_dir>/<run_id>/run.db`) behind a small HTTP API.
//! See docs/dashboard-plan.md.

mod api;
mod block;
mod blocks;
mod cluster;
mod config;
mod features;
mod rpc;
mod slots;
mod state;
mod store;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use chain_watch_client::BlockDetails;
use mtm_chain::ChainClient;
use mtm_scenario::{Scenario, ScenarioConfig};
use tokio::sync::Notify;
use tracing::{info, warn};

use crate::block::{Known, builtin_programs};
use crate::config::{AppConfig, is_public_endpoint};
use crate::state::Shared;
use crate::store::Store;

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    dotenvy::dotenv().ok();
    let cfg: AppConfig = mtm_common::config::load().map_err(|error| anyhow::anyhow!("{error}"))?;
    mtm_telemetry::init("chain-watch", cfg.chain_watch.metrics_addr)?;
    let watch = cfg.chain_watch.clone();

    let http_url = watch
        .rpc
        .http_url
        .clone()
        .unwrap_or_else(|| cfg.rpc.http_url.clone());
    let ws_url = watch
        .rpc
        .ws_url
        .clone()
        .unwrap_or_else(|| cfg.rpc.ws_url.clone());
    let rpc_host = rpc::host(&http_url);
    let public_endpoint = is_public_endpoint(&http_url);
    let max_rps = watch
        .max_rps
        .or(public_endpoint.then_some(config::PUBLIC_MAX_RPS));
    let mut block_details = watch.block_details;
    if block_details != BlockDetails::Slots && public_endpoint && !watch.allow_public_block_fetch {
        warn!(
            rpc_host,
            "public endpoint: recording slots and leaders only (no per-block transactions); \
             set CHAIN_WATCH_RPC_URL or SCENARIO_RPC_URL to a dedicated provider"
        );
        block_details = BlockDetails::Slots;
    }

    let run_id = watch
        .run_id
        .clone()
        .unwrap_or_else(|| format!("watch-{}", mtm_common::time::now_us() / 1_000_000));
    let run_dir = watch.run_dir.join(&run_id);
    let store = Store::open(
        &run_dir,
        &[
            ("run_id", run_id.clone()),
            ("rpc_host", rpc_host.clone()),
            ("block_details", format!("{block_details:?}").to_lowercase()),
            ("profile", std::env::var("MTM_PROFILE").unwrap_or_default()),
            ("started_us", mtm_common::time::now_us().to_string()),
        ],
    )?;
    let raw_dir = run_dir.join("blocks");
    std::fs::create_dir_all(&raw_dir).with_context(|| format!("creating {}", raw_dir.display()))?;
    info!(%run_id, dir = %run_dir.display(), rpc_host, ?block_details, ?max_rps, "run store open");

    let shared = Arc::new(Shared {
        cfg: watch.clone(),
        rpc: rpc::JsonRpc::new(&http_url, max_rps)?,
        store,
        run_id,
        rpc_host,
        block_details,
        public_endpoint,
        our_wallets: AtomicUsize::new(operator_wallets().len()),
        known: RwLock::new(Arc::new(Known {
            wallets: operator_wallets(),
            programs: builtin_programs(),
        })),
        processed: AtomicU64::new(0),
        confirmed: AtomicU64::new(0),
        cursor: AtomicU64::new(0),
        leaders: RwLock::default(),
        nodes: RwLock::default(),
        slot_seconds: RwLock::new(0.4),
        epoch_changed: Notify::new(),
    });

    if let Some(scenario) = cfg.scenario.clone() {
        tokio::spawn(resolve_scenario(
            Arc::clone(&shared),
            scenario,
            http_url.clone(),
            cfg.rpc.commitment.clone(),
        ));
    }
    tokio::spawn(slots::subscribe(Arc::clone(&shared), ws_url));
    tokio::spawn(slots::poll_commitments(Arc::clone(&shared)));
    tokio::spawn(slots::poll_epoch(Arc::clone(&shared)));
    tokio::spawn(slots::poll_performance(Arc::clone(&shared)));
    tokio::spawn(cluster::poll_nodes(Arc::clone(&shared)));
    tokio::spawn(features::poll(Arc::clone(&shared)));
    tokio::spawn(blocks::run(Arc::clone(&shared), raw_dir));

    let listener = tokio::net::TcpListener::bind(watch.listen_addr).await?;
    info!(addr = %watch.listen_addr, "chain-watch API listening");
    axum::serve(listener, api::router(Arc::clone(&shared)))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    shared.store.flush(Duration::from_secs(5));
    Ok(())
}

/// Operator wallets named in the environment (Doppler), when present.
fn operator_wallets() -> HashMap<String, String> {
    [
        ("PUBKEY_FAUCET", "faucet"),
        ("PUBKEY_PROGRAM_AUTHORITY", "authority"),
    ]
    .into_iter()
    .filter_map(|(var, role)| {
        let key = std::env::var(var).ok().filter(|k| !k.is_empty())?;
        Some((key, role.to_owned()))
    })
    .collect()
}

/// Resolve the scenario's participants and program ids, retrying until the
/// mints exist (chain-watch may start before the token bootstrap).
async fn resolve_scenario(
    shared: Arc<Shared>,
    config: ScenarioConfig,
    http_url: String,
    commitment: String,
) {
    let mut logged = false;
    loop {
        let resolved = async {
            let chain = ChainClient::new(&http_url, &commitment)?;
            Scenario::resolve(config.clone(), &chain).await
        }
        .await;
        match resolved {
            Ok(scenario) => {
                let mut wallets = operator_wallets();
                for participant in scenario.participants() {
                    wallets.insert(participant.wallet.to_string(), participant.role);
                }
                let mut programs = builtin_programs();
                let p = &scenario.programs;
                programs.insert(p.raydium_clmm.to_string(), "raydium-clmm");
                programs.insert(p.orca_whirlpool.to_string(), "orca-whirlpool");
                programs.insert(p.manifest.to_string(), "manifest");
                if let Ok(jupiter) = std::env::var("PUBKEY_JUPITER_FORK")
                    && !jupiter.is_empty()
                {
                    programs.insert(jupiter, "jupiter");
                }
                let count = wallets.len();
                if let Ok(mut known) = shared.known.write() {
                    *known = Arc::new(Known { wallets, programs });
                }
                shared.our_wallets.store(count, Ordering::Relaxed);
                info!(
                    wallets = count,
                    "scenario resolved; our transactions are tracked"
                );
                return;
            }
            Err(error) => {
                if !logged {
                    warn!(error = %format!("{error:#}"), "scenario not resolvable yet; retrying every 30 s");
                    logged = true;
                }
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
    info!("chain-watch stopping");
}
