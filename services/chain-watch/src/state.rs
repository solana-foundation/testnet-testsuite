//! State shared by the watcher tasks and the API.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use chain_watch_client::BlockDetails;
use tokio::sync::Notify;

use crate::block::Known;
use crate::config::ChainWatchConfig;
use crate::rpc::JsonRpc;
use crate::store::Store;

/// A validator as `getClusterNodes` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub client: String,
    pub version: String,
}

pub struct Shared {
    pub cfg: ChainWatchConfig,
    pub rpc: JsonRpc,
    pub store: Store,
    pub run_id: String,
    pub rpc_host: String,
    pub block_details: BlockDetails,
    /// A rate-limited public endpoint: skip optional lookups.
    pub public_endpoint: bool,
    /// Our wallets and program names; replaced once the scenario resolves.
    pub known: RwLock<Arc<Known>>,
    pub our_wallets: AtomicUsize,
    pub processed: AtomicU64,
    pub confirmed: AtomicU64,
    /// Next slot the block pipeline records (0 until it starts).
    pub cursor: AtomicU64,
    /// Slot → leader identity, for a window around the cursor.
    pub leaders: RwLock<BTreeMap<u64, String>>,
    /// Identity → client and version.
    pub nodes: RwLock<HashMap<String, Node>>,
    /// Seconds per slot from the latest performance sample.
    pub slot_seconds: RwLock<f64>,
    /// Woken at each epoch boundary (feature activations happen there).
    pub epoch_changed: Notify,
}

impl Shared {
    pub fn known(&self) -> Arc<Known> {
        self.known
            .read()
            .map(|known| Arc::clone(&known))
            .unwrap_or_default()
    }

    pub fn leader(&self, slot: u64) -> Option<(String, Option<Node>)> {
        let leader = self.leaders.read().ok()?.get(&slot).cloned()?;
        let node = self.nodes.read().ok()?.get(&leader).cloned();
        Some((leader, node))
    }

    pub fn slot_seconds(&self) -> f64 {
        self.slot_seconds.read().map(|s| *s).unwrap_or(0.4)
    }

    pub fn confirmed(&self) -> u64 {
        self.confirmed.load(Ordering::Relaxed)
    }
}
