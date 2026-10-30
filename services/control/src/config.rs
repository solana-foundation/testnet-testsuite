//! Service config, loaded via mtm-common's layered loader.

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub control: ControlConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ControlConfig {
    /// Loopback only until the API has auth (docs/dashboard-plan.md).
    pub listen_addr: SocketAddr,
    /// The target every job runs against. It must match the Doppler config
    /// control runs under: the `just control-*` recipes pair them.
    pub target: String,
    pub chain_watch_url: String,
    pub prometheus_url: String,
    /// The built dashboard (`just dashboard-build`).
    pub ui_dir: PathBuf,
    /// `.scenario`: pid files, service logs, snapshots, control.db.
    pub state_dir: PathBuf,
    pub script: PathBuf,
    /// Where `scale-*.toml` overlays live.
    pub config_dir: PathBuf,
}
