//! Service config, loaded via mtm-common's layered loader.

use std::net::SocketAddr;
use std::time::Duration;

pub use mtm_common::config::RpcConfig;
use serde::Deserialize;

use crate::snapshots::CacheTimings;

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
    /// A pool snapshot is fresh for this long after its load completed
    /// (default 400 ms, about one slot). Past it, the next quote starts a
    /// background refresh and quotes keep using the previous snapshot until
    /// `snapshot_max_age_ms`. Concurrent quotes always share one load.
    /// Without reuse every quote reloads every candidate pool: ~900 quotes/s
    /// averaged 5 s per quote in load tests. A failed load is reused for one
    /// TTL, at least 200 ms. `0` means a refresh starts with every quote
    /// while none is in flight (still one load at a time per pool, and
    /// quotes still use the previous snapshot meanwhile); a failed pool is
    /// then retried every 200 ms.
    #[serde(default = "default_snapshot_ttl_ms")]
    pub snapshot_ttl_ms: u64,
    /// Hard bound on snapshot age when a quote is planned, counted from the
    /// start of the snapshot's load (its data is that old): an older pool is
    /// left out of that quote. `router_snapshot_age_seconds` uses the same
    /// age. Must exceed `snapshot_ttl_ms` + the failure backoff (the TTL, at
    /// least 200 ms) + `snapshot_load_timeout_ms`, so one timed-out refresh
    /// leaves its retry time to land before the pool is dropped; checked at
    /// startup.
    #[serde(default = "default_snapshot_max_age_ms")]
    pub snapshot_max_age_ms: u64,
    /// Each snapshot load is abandoned after this long (and the failure
    /// reused for one TTL); also the per-request HTTP timeout of the RPC
    /// client used on the quote path. Must be above 0.
    #[serde(default = "default_snapshot_load_timeout_ms")]
    pub snapshot_load_timeout_ms: u64,
    /// Longest a quote waits for a pool with no usable snapshot (its first
    /// load, or a refresh after an idle gap); the pool is left out of that
    /// quote after it (its load keeps running). A pool whose last load
    /// failed is left out at once, without waiting, until a load succeeds.
    /// Must be below `request_timeout_ms`.
    #[serde(default = "default_snapshot_wait_ms")]
    pub snapshot_wait_ms: u64,
    /// How often the background task refreshes the slot and blockhash that
    /// quotes report and transactions are built on.
    #[serde(default = "default_chain_tip_refresh_ms")]
    pub chain_tip_refresh_ms: u64,
    /// Each tip refresh is abandoned after this long (also the HTTP timeout
    /// of the tip's own RPC client) and retried on the next tick. Must be
    /// above 0, and `chain_tip_max_age_ms` above `chain_tip_refresh_ms` +
    /// this, so one timed-out refresh does not expire the tip.
    #[serde(default = "default_chain_tip_timeout_ms")]
    pub chain_tip_timeout_ms: u64,
    /// Quotes needing the slot or blockhash answer 502 once the cached
    /// value is older than this (the refresher keeps failing).
    #[serde(default = "default_chain_tip_max_age_ms")]
    pub chain_tip_max_age_ms: u64,
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

impl RouterConfig {
    /// Reject timings that cannot work together (checked at startup).
    pub fn validate(&self) -> anyhow::Result<()> {
        let timings = CacheTimings::from_config(self);
        let ms = |duration: Duration| duration.as_millis();
        anyhow::ensure!(
            self.snapshot_load_timeout_ms > 0,
            "router.snapshot_load_timeout_ms must be above 0 (every load would fail)"
        );
        let needed = timings.ttl + timings.failure_backoff() + timings.load_timeout;
        anyhow::ensure!(
            timings.max_age > needed,
            "router.snapshot_max_age_ms ({}) must exceed snapshot_ttl_ms + failure backoff + \
             snapshot_load_timeout_ms ({} ms), or one timed-out refresh drops the pool",
            self.snapshot_max_age_ms,
            ms(needed)
        );
        anyhow::ensure!(
            self.snapshot_wait_ms < self.request_timeout_ms,
            "router.snapshot_wait_ms ({}) must be below request_timeout_ms ({})",
            self.snapshot_wait_ms,
            self.request_timeout_ms
        );
        anyhow::ensure!(
            self.chain_tip_timeout_ms > 0,
            "router.chain_tip_timeout_ms must be above 0"
        );
        let tip_needed = self.chain_tip_refresh_ms + self.chain_tip_timeout_ms;
        anyhow::ensure!(
            self.chain_tip_max_age_ms > tip_needed,
            "router.chain_tip_max_age_ms ({}) must exceed chain_tip_refresh_ms + \
             chain_tip_timeout_ms ({tip_needed} ms), or one timed-out refresh expires the tip",
            self.chain_tip_max_age_ms
        );
        Ok(())
    }
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
fn default_snapshot_max_age_ms() -> u64 {
    3_000
}
fn default_snapshot_load_timeout_ms() -> u64 {
    1_000
}
fn default_snapshot_wait_ms() -> u64 {
    1_000
}
fn default_chain_tip_refresh_ms() -> u64 {
    400
}
fn default_chain_tip_timeout_ms() -> u64 {
    1_000
}
fn default_chain_tip_max_age_ms() -> u64 {
    5_000
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config(overrides: serde_json::Value) -> RouterConfig {
        let mut value = serde_json::json!({ "listen_addr": "127.0.0.1:0" });
        value
            .as_object_mut()
            .expect("object")
            .extend(overrides.as_object().expect("object").clone());
        serde_json::from_value(value).expect("router config")
    }

    #[test]
    fn defaults_are_valid() {
        config(serde_json::json!({})).validate().expect("defaults");
    }

    #[test]
    fn timings_that_cannot_work_are_rejected() {
        for (overrides, why) in [
            (
                serde_json::json!({ "snapshot_load_timeout_ms": 0 }),
                "load_timeout",
            ),
            // 400 + 400 + 2000 >= 2800: one timed-out refresh drops the pool.
            (
                serde_json::json!({ "snapshot_load_timeout_ms": 2_000, "snapshot_max_age_ms": 2_800 }),
                "snapshot_max_age_ms",
            ),
            // TTL 0 still backs off 200 ms.
            (
                serde_json::json!({ "snapshot_ttl_ms": 0, "snapshot_max_age_ms": 1_200 }),
                "snapshot_max_age_ms",
            ),
            (
                serde_json::json!({ "snapshot_wait_ms": 10_000 }),
                "snapshot_wait_ms",
            ),
            (
                serde_json::json!({ "chain_tip_timeout_ms": 0 }),
                "chain_tip_timeout_ms",
            ),
            (
                serde_json::json!({ "chain_tip_max_age_ms": 1_400 }),
                "chain_tip_max_age_ms",
            ),
        ] {
            let error = config(overrides.clone())
                .validate()
                .expect_err(&format!("{overrides} should be rejected"));
            assert!(error.to_string().contains(why), "{overrides}: {error}");
        }
    }
}
