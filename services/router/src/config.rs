//! Service config, loaded via mtm-common's layered loader.

use std::net::SocketAddr;

pub use mtm_common::config::RpcConfig;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub rpc: RpcConfig,
    pub router: RouterConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RouterConfig {
    pub listen_addr: SocketAddr,
    pub metrics_addr: Option<SocketAddr>,
    /// Program IDs of the deployed venue forks. A venue without a program ID
    /// is skipped by discovery; curated pools may still name one explicitly.
    #[serde(default)]
    pub programs: ProgramsConfig,
    /// Curated pools: routed without vetting (we created them).
    #[serde(default)]
    pub pools: Vec<PoolEntry>,
    /// priority fee attached to built transactions, micro-lamports per CU
    #[serde(default = "default_cu_price")]
    pub cu_price_micro_lamports: u64,
    /// CU margin added on top of summed per-leg estimates
    #[serde(default = "default_cu_margin")]
    pub cu_margin: u32,
    #[serde(default)]
    pub discovery: DiscoveryConfig,
    /// intermediate mints multi-hop routes may pass through
    #[serde(default)]
    pub bridge_mints: Vec<String>,
    #[serde(default)]
    pub jupiter: JupiterConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProgramsConfig {
    pub raydium_clmm: Option<String>,
    pub orca_whirlpool: Option<String>,
    pub manifest: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DiscoveryConfig {
    /// Sweep each configured venue program for pools by account discriminator.
    pub enabled: bool,
    pub interval_ms: u64,
    /// Vetting probe: both directions must quote this many input atoms.
    pub probe_amount: u64,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_ms: 60_000,
            probe_amount: 1_000,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct JupiterConfig {
    /// Program ID of the Jupiter aggregator fork. `execution = "jupiter"`
    /// requests fail until this is set.
    pub program_id: Option<String>,
    /// CU charged on top of the legs for the aggregator's own work.
    #[serde(default = "default_jupiter_overhead")]
    pub overhead_cu: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PoolEntry {
    /// "raydium-clmm", "orca-whirlpool", or "manifest"
    pub venue: String,
    pub address: String,
    /// override for this pool; defaults to `[router.programs]`
    #[serde(default)]
    pub program_id: Option<String>,
}

fn default_cu_price() -> u64 {
    1_000
}
fn default_cu_margin() -> u32 {
    50_000
}
fn default_jupiter_overhead() -> u32 {
    60_000
}
