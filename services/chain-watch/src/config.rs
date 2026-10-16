//! Service config, loaded via mtm-common's layered loader.

use std::net::SocketAddr;
use std::path::PathBuf;

use chain_watch_client::BlockDetails;
pub use mtm_common::config::RpcConfig;
use mtm_scenario::ScenarioConfig;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub rpc: RpcConfig,
    pub chain_watch: ChainWatchConfig,
    /// When present (scenario profiles), participant wallets and the forks'
    /// program ids are resolved so our transactions can be told apart.
    pub scenario: Option<ScenarioConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChainWatchConfig {
    pub listen_addr: SocketAddr,
    pub metrics_addr: Option<SocketAddr>,
    /// Observer-only endpoints; unset fields fall back to `[rpc]`. Set from
    /// Doppler's `CHAIN_WATCH_RPC_URL` / `CHAIN_WATCH_WS_URL` by scenario.sh.
    #[serde(default)]
    pub rpc: RpcOverride,
    #[serde(default)]
    pub block_details: BlockDetails,
    /// A block per slot exhausts a public endpoint's quota within minutes;
    /// chain-watch drops to `slots` there unless this is set.
    #[serde(default)]
    pub allow_public_block_fetch: bool,
    /// `getBlock` calls in flight.
    #[serde(default = "default_block_concurrency")]
    pub block_concurrency: usize,
    /// Further behind the confirmed tip than this, skip ahead (recording a
    /// `gap` event) rather than fall behind for good. 3000 = 10 min at 200 ms.
    #[serde(default = "default_max_lag_slots")]
    pub max_lag_slots: u64,
    /// Attempts for a block that is not available yet.
    #[serde(default = "default_block_attempts")]
    pub block_attempts: u32,
    #[serde(default = "default_run_dir")]
    pub run_dir: PathBuf,
    /// Defaults to `watch-<unix seconds>`; scenario.sh passes the run's id.
    /// Read leniently: the env layer parses `MTM_CHAIN_WATCH__RUN_ID=1` as a
    /// number, which once failed chain-watch at startup.
    #[serde(default, deserialize_with = "string_or_number")]
    pub run_id: Option<String>,
    #[serde(default)]
    pub raw_blocks: RawBlocks,
    #[serde(default = "default_feature_poll_s")]
    pub feature_poll_s: u64,
    #[serde(default = "default_cluster_poll_s")]
    pub cluster_poll_s: u64,
    #[serde(default = "default_commitment_poll_ms")]
    pub commitment_poll_ms: u64,
    /// Request budget across every call. Unset: unlimited on a dedicated
    /// provider, `PUBLIC_MAX_RPS` on a public endpoint (100 requests / 10 s
    /// per IP there, 40 per method), which then also skips the per-gate
    /// block-time lookups (feature times are estimated).
    pub max_rps: Option<f64>,
    /// Block cost limit, for the fullness gauge; unset hides it.
    pub block_cost_limit: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RpcOverride {
    pub http_url: Option<String>,
    pub ws_url: Option<String>,
}

/// Which raw `getBlock` responses to keep (zstd JSON beside run.db).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RawBlocks {
    Off,
    /// Blocks containing one of our failed transactions: the ones worth
    /// re-reading. The default.
    #[default]
    Failed,
    /// Blocks containing one of our transactions: every block at scale
    /// (about 4 GB/h at x1000).
    Ours,
    All,
}

/// A string, or a number taken as its decimal string.
fn string_or_number<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Text(String),
        Unsigned(u64),
        Signed(i64),
    }
    Ok(
        Option::<Raw>::deserialize(deserializer)?.map(|raw| match raw {
            Raw::Text(text) => text,
            Raw::Unsigned(n) => n.to_string(),
            Raw::Signed(n) => n.to_string(),
        }),
    )
}

fn default_block_concurrency() -> usize {
    8
}
fn default_max_lag_slots() -> u64 {
    3_000
}
fn default_block_attempts() -> u32 {
    6
}
fn default_run_dir() -> PathBuf {
    PathBuf::from(".scenario/runs")
}
fn default_feature_poll_s() -> u64 {
    600
}
fn default_cluster_poll_s() -> u64 {
    60
}
fn default_commitment_poll_ms() -> u64 {
    1_000
}

/// Default request budget on a public endpoint.
pub const PUBLIC_MAX_RPS: f64 = 6.0;

/// Public endpoints that can't sustain full blocks at 5 slots/s.
pub fn is_public_endpoint(url: &str) -> bool {
    let host = crate::rpc::host(url);
    [
        "api.testnet.solana.com",
        "api.devnet.solana.com",
        "api.mainnet-beta.solana.com",
        "api.mainnet.solana.com",
    ]
    .contains(&host.as_str())
}

#[cfg(test)]
mod tests {
    use figment::Figment;
    use figment::providers::{Format, Toml};

    use super::*;

    #[derive(Deserialize)]
    struct RunId {
        #[serde(default, deserialize_with = "string_or_number")]
        run_id: Option<String>,
    }

    #[test]
    fn numeric_run_ids_are_strings() {
        let id = |toml: &str| -> Option<String> {
            Figment::new()
                .merge(Toml::string(toml))
                .extract::<RunId>()
                .expect("parses")
                .run_id
        };
        assert_eq!(id("run_id = 1"), Some("1".to_owned()));
        assert_eq!(id(r#"run_id = "tv-x10""#), Some("tv-x10".to_owned()));
        assert_eq!(id(""), None);
    }
}
