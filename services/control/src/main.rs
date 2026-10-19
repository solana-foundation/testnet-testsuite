//! control: the dashboard's backend (docs/dashboard-plan.md, P1).
//!
//! Runs `scenario.sh` jobs against one target, reports the processes it
//! started and their logs, keeps a timeline of what it did (control.db),
//! proxies chain-watch and Prometheus so the UI has one origin, pushes a
//! live feed over `/api/live`, and serves the built dashboard. Loopback
//! only: there is no auth yet.

mod api;
mod compose;
mod config;
mod jobs;
mod live;
mod processes;
mod store;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use chain_watch_client::{ChainWatchClient, Health};
use control_client::{Job, Status};
use tracing::info;

use crate::config::{AppConfig, ControlConfig};
use crate::live::Hub;
use crate::store::Store;

pub struct RunningJob {
    pub job: Job,
    pub pid: Option<u32>,
    pub cancelled: bool,
}

pub struct App {
    pub cfg: ControlConfig,
    pub store: Store,
    pub hub: Hub,
    pub chain: ChainWatchClient,
    pub http: reqwest::Client,
    pub job: tokio::sync::Mutex<Option<RunningJob>>,
    chain_health: Mutex<Option<Health>>,
}

impl App {
    pub fn set_chain_health(&self, health: Option<Health>) {
        if let Ok(mut current) = self.chain_health.lock() {
            *current = health;
        }
    }

    pub async fn status(&self) -> Status {
        Status {
            target: self.cfg.target.clone(),
            processes: processes::processes(&self.cfg.state_dir),
            chain: self.chain_health.lock().ok().and_then(|h| h.clone()),
            job: self.job.lock().await.as_ref().map(|r| r.job.clone()),
            scales: processes::scales(&self.cfg.config_dir),
            active_scale: std::fs::read_to_string(self.cfg.state_dir.join("scale"))
                .ok()
                .map(|s| s.trim().to_owned()),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    dotenvy::dotenv().ok();
    let cfg: AppConfig = mtm_common::config::load().map_err(|error| anyhow::anyhow!("{error}"))?;
    mtm_telemetry::init("control", None)?;
    let cfg = cfg.control;
    anyhow::ensure!(
        cfg.listen_addr.ip().is_loopback(),
        "control has no auth yet: listen_addr must be a loopback address, got {}",
        cfg.listen_addr
    );
    anyhow::ensure!(
        ["local", "devnet", "testnet"].contains(&cfg.target.as_str()),
        "control.target must be local, devnet or testnet"
    );

    let app = Arc::new(App {
        store: Store::open(&cfg.state_dir.join("control").join("control.db"))?,
        hub: Hub::new(),
        chain: ChainWatchClient::new(&cfg.chain_watch_url),
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?,
        job: tokio::sync::Mutex::new(None),
        chain_health: Mutex::new(None),
        cfg,
    });
    // Control's own events from the last day seed the timeline.
    let since = mtm_common::time::now_us() - 24 * 3_600 * 1_000_000;
    for event in app.store.events(since)? {
        app.hub.push_event(control_client::Source::Control, event);
    }

    tokio::spawn(live::pump_chain(Arc::clone(&app)));
    tokio::spawn(live::pump_status(Arc::clone(&app)));

    let listener = tokio::net::TcpListener::bind(app.cfg.listen_addr).await?;
    info!(addr = %app.cfg.listen_addr, target = %app.cfg.target, "control listening");
    axum::serve(listener, api::router(app))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
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
    info!("control stopping (running jobs continue in their own process group)");
}
