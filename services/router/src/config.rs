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
    /// priority price, micro-lamports per CU; converted to the total-lamport
    /// priority fee of the v1 message config using the transaction's CU limit
    #[serde(default = "default_cu_price")]
    pub cu_price_micro_lamports: u64,
    /// CU margin added on top of summed per-leg estimates
    #[serde(default = "default_cu_margin")]
    pub cu_margin: u32,
    /// Account-data load limit written into each v1 message config (v1
    /// treats an unset limit as zero).
    #[serde(default = "default_loaded_accounts_data_size_limit")]
    pub loaded_accounts_data_size_limit: u32,
    #[serde(default)]
    pub discovery: DiscoveryConfig,
    /// intermediate mints multi-hop routes may pass through
    #[serde(default)]
    pub bridge_mints: Vec<String>,
    /// Reuse a pool snapshot across quotes for this long (default 400 ms,
    /// about one slot; 0 = a fresh load per quote). Concurrent quotes always
    /// share an in-flight load. Without reuse every quote reloads every
    /// candidate pool: ~900 quotes/s averaged 5 s per quote in load tests.
    #[serde(default = "default_snapshot_ttl_ms")]
    pub snapshot_ttl_ms: u64,
    /// A request still running after this long is answered 408, so slow RPC
    /// cannot pile requests up without bound.
    #[serde(default = "default_request_timeout_ms")]
    pub request_timeout_ms: u64,
    /// Requests served at once; further ones wait for a slot.
    #[serde(default = "default_max_concurrent_requests")]
    pub max_concurrent_requests: usize,
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
fn default_request_timeout_ms() -> u64 {
    10_000
}
fn default_max_concurrent_requests() -> usize {
    1_024
}
fn default_snapshot_ttl_ms() -> u64 {
    400
}
fn default_cu_margin() -> u32 {
    50_000
}
fn default_loaded_accounts_data_size_limit() -> u32 {
    crate::assemble::DEFAULT_LOADED_ACCOUNTS_DATA_SIZE_LIMIT
}
fn default_jupiter_overhead() -> u32 {
    60_000
}
