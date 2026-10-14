//! Router service (docs/router-architecture.md). Routes across the suite's
//! Raydium CLMM, Orca Whirlpool, and Manifest forks: ExactIn with same-pair
//! splits and 2-hop bridge routes, single-hop ExactOut, executed either as
//! top-level venue instructions or through a Jupiter aggregator fork.
//! Unsigned v1 transactions, with routes fitted to the v1 limits.

pub mod api;
pub mod assemble;
pub mod config;
pub mod engine;
pub mod registry;

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use config::AppConfig;
use engine::{Book, EngineError, Request};
use futures::future::join_all;
use mtm_chain::ChainClient;
use mtm_venues::PoolSnapshot;
use registry::{Programs, Registry};
use solana_pubkey::Pubkey;
use tracing::{debug, info, warn};

/// Pool snapshots are shared between the cache, the book, and planned legs.
pub type Snapshot = Arc<PoolSnapshot>;

pub struct AppState {
    pub registry: Arc<Registry>,
    pub snapshots: SnapshotCache,
    pub chain: ChainClient,
    pub cfg: config::RouterConfig,
    pub bridge_mints: Vec<Pubkey>,
    pub jupiter_program: Option<Pubkey>,
    pub jupiter_overhead_cu: u32,
}

pub async fn run(cfg: AppConfig) -> anyhow::Result<()> {
    let chain = ChainClient::new(&cfg.rpc.http_url, &cfg.rpc.commitment)?;
    let programs = Programs::from_config(&cfg.router)?;
    let registry = Registry::new();
    registry
        .add_curated(&chain, &programs, &cfg.router.pools)
        .await?;
    info!(pools = cfg.router.pools.len(), "curated pools loaded");

    let discovery = &cfg.router.discovery;
    if discovery.enabled {
        registry
            .sweep(&chain, &programs, discovery.probe_amount)
            .await;
        registry::spawn_discovery(
            Arc::clone(&registry),
            chain.clone(),
            programs.clone(),
            Duration::from_millis(discovery.interval_ms),
            discovery.probe_amount,
        );
        info!("pool discovery enabled");
    }

    let bridge_mints = cfg
        .router
        .bridge_mints
        .iter()
        .map(|mint| Pubkey::from_str(mint).context("bridge mint"))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let jupiter_program = cfg
        .router
        .jupiter
        .program_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .map(|id| Pubkey::from_str(id).context("jupiter program id"))
        .transpose()?;
    let state = Arc::new(AppState {
        registry,
        snapshots: SnapshotCache::new(Duration::from_millis(cfg.router.snapshot_ttl_ms)),
        chain,
        jupiter_overhead_cu: cfg.router.jupiter.overhead_cu,
        cfg: cfg.router.clone(),
        bridge_mints,
        jupiter_program,
    });
    let app = api::router(state);
    let listener = tokio::net::TcpListener::bind(cfg.router.listen_addr).await?;
    info!(addr = %cfg.router.listen_addr, "router api listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Snapshot every routable pool on the pairs the request may touch, and the
/// slot the snapshots were taken at. Pools that fail to load are skipped.
pub async fn load_book(
    state: &AppState,
    request: &Request,
) -> Result<(Book<Snapshot>, u64), EngineError> {
    let state_slot = state
        .chain
        .current_slot()
        .await
        .map_err(|error| EngineError::Rpc(error.to_string()))?;
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    for (a, b) in request.pairs(&state.bridge_mints) {
        for entry in state.registry.routable(&a, &b).await {
            if seen.insert(entry.pool) {
                entries.push(entry);
            }
        }
    }
    let loads = entries
        .iter()
        .map(|entry| state.snapshots.load(&state.chain, entry));
    let mut sources = Vec::new();
    let mut failures = Vec::new();
    for (entry, loaded) in entries.iter().zip(join_all(loads).await) {
        match loaded {
            Ok(snapshot) => sources.push((snapshot.mints(), snapshot)),
            Err(error) => {
                debug!(pool = %entry.pool, error = %error, "skipping pool without a snapshot");
                failures.push(format!("{}: {error}", entry.pool));
            }
        }
    }
    // Every candidate failing is an upstream problem (502), not "no route".
    if sources.is_empty() && !failures.is_empty() {
        return Err(EngineError::Rpc(format!(
            "every candidate pool failed to load: {}",
            failures.join("; ")
        )));
    }
    Ok((Book::new(sources), state_slot))
}

type SharedSnapshot = Arc<tokio::sync::OnceCell<Result<Snapshot, String>>>;

/// Pool snapshots shared by concurrent quotes. A pool is loaded once per
/// `ttl` window (single flight), and an in-flight load is always shared, even
/// with `ttl` 0; hits hand out the shared snapshot, never a copy.
pub struct SnapshotCache {
    ttl: Duration,
    // Never held across an await: only to find or insert a pool's cell.
    entries: std::sync::Mutex<HashMap<Pubkey, (Instant, SharedSnapshot)>>,
}

impl SnapshotCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: std::sync::Mutex::new(HashMap::new()),
        }
    }

    async fn load(&self, chain: &ChainClient, entry: &registry::Entry) -> Result<Snapshot, String> {
        // Logged here, once per load attempt, not once per quote that reuses
        // a cached failure (that was a warning per pool per quote at load).
        let fetch = || async {
            PoolSnapshot::load(chain, entry.kind, entry.program_id, entry.pool)
                .await
                .map(Arc::new)
                .map_err(|error| {
                    warn!(pool = %entry.pool, error = %error, "pool snapshot failed");
                    metrics::counter!("router_snapshot_load_failures_total").increment(1);
                    error.to_string()
                })
        };
        let cell = {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let now = Instant::now();
            match entries.get(&entry.pool) {
                Some((loaded_at, cell))
                    if !cell.initialized() || now.duration_since(*loaded_at) < self.ttl =>
                {
                    metrics::counter!("router_snapshot_cache_total", "result" => "hit")
                        .increment(1);
                    Arc::clone(cell)
                }
                _ => {
                    metrics::counter!("router_snapshot_cache_total", "result" => "miss")
                        .increment(1);
                    let cell = SharedSnapshot::default();
                    entries.insert(entry.pool, (now, Arc::clone(&cell)));
                    cell
                }
            }
        };
        cell.get_or_init(fetch).await.clone()
    }
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    info!("shutdown signal received");
}
