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
pub mod snapshots;
pub mod tip;

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use config::AppConfig;
use engine::{Book, EngineError, Request};
use futures::future::join_all;
use mtm_chain::ChainClient;
use mtm_venues::PoolSnapshot;
use registry::{Programs, Registry};
use snapshots::{CacheTimings, SnapshotCache};
use solana_pubkey::Pubkey;
use tip::TipCell;
use tracing::{debug, info};

/// Pool snapshots are shared between the cache, the book, and planned legs.
pub type Snapshot = Arc<PoolSnapshot>;

pub struct AppState {
    pub registry: Arc<Registry>,
    pub snapshots: SnapshotCache,
    /// Slot and blockhash, refreshed in the background (never fetched per quote).
    pub tip: TipCell,
    /// Quote-path client (short request timeout): snapshot loads.
    pub chain: ChainClient,
    pub cfg: config::RouterConfig,
    pub bridge_mints: Vec<Pubkey>,
    pub jupiter_program: Option<Pubkey>,
    pub jupiter_overhead_cu: u32,
}

pub async fn run(cfg: AppConfig) -> anyhow::Result<()> {
    cfg.router.validate()?;
    // Startup and discovery (`getProgramAccounts`) keep the default HTTP
    // timeout; the quote path gets one bounded by the snapshot load timeout.
    let chain = ChainClient::new(&cfg.rpc.http_url, &cfg.rpc.commitment)?;
    let load_timeout = Duration::from_millis(cfg.router.snapshot_load_timeout_ms);
    let quote_chain =
        ChainClient::with_timeout(&cfg.rpc.http_url, &cfg.rpc.commitment, load_timeout)?;
    // The tip gets a client of its own, so snapshot loads cannot queue
    // ahead of it.
    let tip_timeout = Duration::from_millis(cfg.router.chain_tip_timeout_ms);
    let tip_chain = ChainClient::with_timeout(&cfg.rpc.http_url, &cfg.rpc.commitment, tip_timeout)?;
    let tip = tip::spawn_refresher(
        tip::chain_fetch(tip_chain),
        Duration::from_millis(cfg.router.chain_tip_refresh_ms.max(1)),
        tip_timeout,
        Duration::from_millis(cfg.router.chain_tip_max_age_ms),
    );
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
        snapshots: SnapshotCache::new(
            CacheTimings::from_config(&cfg.router),
            snapshots::chain_loader(quote_chain.clone()),
        ),
        tip,
        chain: quote_chain,
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
/// slot of the cached chain tip. No RPC: snapshots come from the cache
/// (waiting at most `snapshot_wait_ms` for a pool without a usable one) and
/// the slot from the tip refresher. Pools that fail to load, or whose
/// snapshot is older than `snapshot_max_age_ms` by the time every pool is
/// in, are left out.
pub async fn load_book(
    state: &AppState,
    request: &Request,
) -> Result<(Book<Snapshot>, u64), EngineError> {
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    for (a, b) in request.pairs(&state.bridge_mints) {
        for entry in state.registry.routable(&a, &b).await {
            if seen.insert(entry.pool) {
                entries.push(entry);
            }
        }
    }
    if entries.is_empty() {
        return Err(EngineError::NoRoute);
    }
    let state_slot = state.tip.current().map_err(EngineError::Rpc)?.slot;
    let wait = Duration::from_millis(state.cfg.snapshot_wait_ms);
    let loads = entries.iter().map(|entry| state.snapshots.get(entry, wait));
    let loaded = join_all(loads).await;
    let max_age = state.snapshots.timings().max_age;
    let now = tokio::time::Instant::now();
    let mut sources = Vec::new();
    let mut failures = Vec::new();
    for (entry, loaded) in entries.iter().zip(loaded) {
        match loaded {
            Ok(loaded) if loaded.age(now) > max_age => {
                metrics::counter!("router_snapshot_skipped_total", "reason" => "too_old")
                    .increment(1);
                failures.push(format!(
                    "{}: snapshot {} ms old",
                    entry.pool,
                    loaded.age(now).as_millis()
                ));
            }
            Ok(loaded) => {
                metrics::histogram!("router_snapshot_age_seconds")
                    .record(loaded.age(now).as_secs_f64());
                sources.push((loaded.value.mints(), loaded.value));
            }
            Err(error) => {
                metrics::counter!("router_snapshot_skipped_total", "reason" => "unavailable")
                    .increment(1);
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

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    info!("shutdown signal received");
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures::FutureExt;
    use mtm_venues::VenueKind;
    use router_client::QuoteRequest;
    use solana_hash::Hash;
    use solana_instruction::{AccountMeta, Instruction};

    use super::*;
    use crate::registry::{Entry, Tier};
    use crate::tip::ChainTip;

    /// An "RPC node" that counts connections and never answers.
    async fn counting_rpc() -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let connections = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&connections);
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                held.push(socket);
            }
        });
        (url, connections)
    }

    fn mint(byte: u8) -> Pubkey {
        Pubkey::new_from_array([byte; 32])
    }

    /// State over `rpc_url` with one curated pool (mints 3/4) whose loader
    /// always fails, counting loads.
    async fn state(
        rpc_url: &str,
    ) -> (
        AppState,
        tokio::sync::watch::Sender<Option<ChainTip>>,
        Arc<AtomicUsize>,
    ) {
        let cfg: config::RouterConfig =
            serde_json::from_value(serde_json::json!({ "listen_addr": "127.0.0.1:0" }))
                .expect("router config");
        let chain = ChainClient::with_timeout(rpc_url, "confirmed", Duration::from_secs(1))
            .expect("client");
        let registry = Registry::new();
        registry
            .insert(Entry {
                kind: VenueKind::OrcaWhirlpool,
                program_id: mint(1),
                pool: mint(2),
                mints: (mint(3), mint(4)),
                tier: Tier::Curated,
            })
            .await;
        let loads = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&loads);
        let loader: snapshots::Loader<Snapshot> = Arc::new(move |_entry| {
            counter.fetch_add(1, Ordering::SeqCst);
            async { Err("fake load failure".to_owned()) }.boxed()
        });
        let (tip_tx, tip) = TipCell::new(Duration::from_millis(cfg.chain_tip_max_age_ms));
        let state = AppState {
            registry,
            snapshots: SnapshotCache::new(CacheTimings::from_config(&cfg), loader),
            tip,
            chain,
            cfg,
            bridge_mints: Vec::new(),
            jupiter_program: None,
            jupiter_overhead_cu: 0,
        };
        (state, tip_tx, loads)
    }

    fn request(input: u8, output: u8) -> Request {
        Request::parse(&QuoteRequest {
            input_mint: mint(input).to_string(),
            output_mint: mint(output).to_string(),
            amount: 1_000,
            mode: Default::default(),
            slippage_bps: 50,
            execution: Default::default(),
            user: None,
        })
        .expect("request")
    }

    fn fresh_tip() -> Option<ChainTip> {
        Some(ChainTip {
            slot: 42,
            blockhash: Hash::new_from_array([7; 32]),
            fetched_at: tokio::time::Instant::now(),
        })
    }

    /// ROU-1: unknown mints answer "no route" from the registry alone, before
    /// the tip (empty here, which would be a 502) or any load is consulted.
    #[tokio::test]
    async fn registry_lookup_comes_before_any_io() {
        let (url, connections) = counting_rpc().await;
        let (state, _tip, loads) = state(&url).await;
        let result = load_book(&state, &request(5, 6)).await;
        assert!(matches!(result, Err(EngineError::NoRoute)));
        assert_eq!(loads.load(Ordering::SeqCst), 0);
        assert_eq!(connections.load(Ordering::SeqCst), 0);
    }

    /// ROU-1: concurrent quotes and transaction builds make no RPC calls:
    /// the slot and blockhash come from the cached tip, snapshot loads from
    /// the cache (one load shared by all, failure reused).
    #[tokio::test]
    async fn quote_path_makes_no_rpc_calls() {
        let (url, connections) = counting_rpc().await;
        let (state, tip, loads) = state(&url).await;
        tip.send_replace(fresh_tip());
        let state = Arc::new(state);
        let quotes = (0..32).map(|_| {
            let state = Arc::clone(&state);
            tokio::spawn(async move { load_book(&state, &request(3, 4)).await.map(|_| ()) })
        });
        for quote in join_all(quotes).await {
            // Every candidate failed (fake loader): 502 from the loader, not
            // from an RPC call.
            let error = quote.expect("task").expect_err("fake loader fails");
            assert!(error.to_string().contains("fake load failure"), "{error}");
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1, "one shared load");

        let assembled = assemble::Assembled {
            instructions: vec![Instruction {
                program_id: mint(9),
                accounts: vec![AccountMeta::new(mint(10), false)],
                data: vec![1, 2, 3],
            }],
            cu_estimate: 10_000,
        };
        for _ in 0..32 {
            assemble::route_tx(&state, &mint(11), &assembled).expect("built on the cached hash");
        }
        assert_eq!(connections.load(Ordering::SeqCst), 0);
    }

    /// ROU-1: a tip older than `chain_tip_max_age_ms` is a 502 (RPC fault),
    /// not a transaction on an expiring blockhash.
    #[tokio::test(start_paused = true)]
    async fn stale_tip_is_an_upstream_error() {
        let (url, _connections) = counting_rpc().await;
        let (state, tip, _loads) = state(&url).await;
        tip.send_replace(fresh_tip());
        tokio::time::sleep(Duration::from_millis(state.cfg.chain_tip_max_age_ms + 1)).await;
        let assembled = assemble::Assembled {
            instructions: vec![],
            cu_estimate: 0,
        };
        let error = assemble::route_tx(&state, &mint(11), &assembled).expect_err("stale tip");
        assert!(matches!(
            error.downcast_ref::<assemble::AssemblyFault>(),
            Some(assemble::AssemblyFault::Rpc(_))
        ));
        assert!(matches!(
            load_book(&state, &request(3, 4)).await,
            Err(EngineError::Rpc(_))
        ));
    }
}
