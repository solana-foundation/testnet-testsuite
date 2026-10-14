//! Pool registry (docs/router-architecture.md §2): curated pools from config
//! plus discovered pools that passed vetting.
//!
//! Discovery sweeps each venue program with `getProgramAccounts` filtered by
//! account discriminator. A discovered pool is loaded through its venue
//! client (owner, layout, PDA, vault, and mint checks) and then probed: both
//! directions must quote `probe_amount` input atoms (escalated ×1000 up to
//! three times for tokens where that rounds to nothing). Pools that fail stay
//! `Quarantined` (listed, never routed) and are re-vetted every sweep, so
//! transient failures such as an empty book heal on their own.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use mtm_chain::ChainClient;
use mtm_venues::{PoolSnapshot, VenueKind};
use serde::Serialize;
use solana_pubkey::Pubkey;
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::config::{PoolEntry, RouterConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tier {
    /// From config: created by our own bootstrap, routed without vetting.
    Curated,
    /// Discovered and passed vetting.
    Verified,
    /// Discovered but failed vetting: visible, not routed.
    Quarantined,
}

impl Tier {
    pub fn routable(self) -> bool {
        matches!(self, Tier::Curated | Tier::Verified)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Entry {
    pub kind: VenueKind,
    pub program_id: Pubkey,
    pub pool: Pubkey,
    pub mints: (Pubkey, Pubkey),
    pub tier: Tier,
}

impl Entry {
    pub fn trades(&self, a: &Pubkey, b: &Pubkey) -> bool {
        (self.mints.0 == *a && self.mints.1 == *b) || (self.mints.0 == *b && self.mints.1 == *a)
    }
}

/// Venue program IDs resolved from `[router.programs]`.
#[derive(Debug, Clone, Default)]
pub struct Programs(HashMap<VenueKind, Pubkey>);

impl Programs {
    pub fn from_config(cfg: &RouterConfig) -> anyhow::Result<Self> {
        let mut programs = HashMap::new();
        for (kind, value) in [
            (VenueKind::RaydiumClmm, &cfg.programs.raydium_clmm),
            (VenueKind::OrcaWhirlpool, &cfg.programs.orca_whirlpool),
            (VenueKind::Manifest, &cfg.programs.manifest),
        ] {
            if let Some(value) = value.as_deref().filter(|value| !value.is_empty()) {
                programs.insert(
                    kind,
                    Pubkey::from_str(value)
                        .with_context(|| format!("{} program id", kind.as_str()))?,
                );
            }
        }
        Ok(Self(programs))
    }

    pub fn get(&self, kind: VenueKind) -> Option<Pubkey> {
        self.0.get(&kind).copied()
    }

    pub fn iter(&self) -> impl Iterator<Item = (VenueKind, Pubkey)> + '_ {
        self.0.iter().map(|(kind, program)| (*kind, *program))
    }
}

#[derive(Default)]
pub struct Registry {
    entries: RwLock<HashMap<Pubkey, Entry>>,
}

impl Registry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Load every curated pool; a pool that fails to load aborts startup.
    pub async fn add_curated(
        &self,
        chain: &ChainClient,
        programs: &Programs,
        pools: &[PoolEntry],
    ) -> anyhow::Result<()> {
        for entry in pools {
            let kind = VenueKind::from_str(&entry.venue)?;
            let program_id = match &entry.program_id {
                Some(id) => Pubkey::from_str(id).context("pool program_id")?,
                None => programs.get(kind).with_context(|| {
                    format!(
                        "pool {} needs a {} program id",
                        entry.address,
                        kind.as_str()
                    )
                })?,
            };
            let pool = Pubkey::from_str(&entry.address).context("pool address")?;
            let snapshot = PoolSnapshot::load(chain, kind, program_id, pool)
                .await
                .with_context(|| format!("loading curated pool {pool}"))?;
            info!(%pool, venue = kind.as_str(), "curated pool loaded");
            self.insert(Entry {
                kind,
                program_id,
                pool,
                mints: snapshot.mints(),
                tier: Tier::Curated,
            })
            .await;
        }
        Ok(())
    }

    pub async fn insert(&self, entry: Entry) {
        self.entries.write().await.insert(entry.pool, entry);
    }

    pub async fn all(&self) -> Vec<Entry> {
        let mut entries: Vec<Entry> = self.entries.read().await.values().copied().collect();
        entries.sort_by_key(|entry| entry.pool);
        entries
    }

    /// Routable pools trading the unordered pair.
    pub async fn routable(&self, a: &Pubkey, b: &Pubkey) -> Vec<Entry> {
        let mut entries: Vec<Entry> = self
            .entries
            .read()
            .await
            .values()
            .filter(|entry| entry.tier.routable() && entry.trades(a, b))
            .copied()
            .collect();
        entries.sort_by_key(|entry| entry.pool);
        entries
    }

    /// One discovery sweep across every configured venue program.
    pub async fn sweep(&self, chain: &ChainClient, programs: &Programs, probe_amount: u64) {
        for (kind, program_id) in programs.iter() {
            let pools = match mtm_venues::discover(chain, kind, program_id).await {
                Ok(pools) => pools,
                Err(error) => {
                    warn!(venue = kind.as_str(), error = %error, "discovery sweep failed");
                    continue;
                }
            };
            for pool in pools {
                let known = self.entries.read().await.get(&pool).map(|entry| entry.tier);
                if matches!(known, Some(Tier::Curated | Tier::Verified)) {
                    continue;
                }
                let (tier, mints) = vet(chain, kind, program_id, pool, probe_amount).await;
                let Some(mints) = mints else {
                    continue;
                };
                if known != Some(tier) {
                    info!(%pool, venue = kind.as_str(), ?tier, "discovered pool vetted");
                }
                metrics::counter!("router_vetting_total", "tier" => format!("{tier:?}"))
                    .increment(1);
                self.insert(Entry {
                    kind,
                    program_id,
                    pool,
                    mints,
                    tier,
                })
                .await;
            }
        }
    }
}

/// Load and probe one discovered pool. Unloadable accounts are ignored
/// (not a pool of this venue); loadable pools that cannot quote both
/// directions are quarantined.
async fn vet(
    chain: &ChainClient,
    kind: VenueKind,
    program_id: Pubkey,
    pool: Pubkey,
    probe_amount: u64,
) -> (Tier, Option<(Pubkey, Pubkey)>) {
    let snapshot = match PoolSnapshot::load(chain, kind, program_id, pool).await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            warn!(%pool, venue = kind.as_str(), error = %error, "discovered account failed to load");
            return (Tier::Quarantined, None);
        }
    };
    let (a, b) = snapshot.mints();
    // `probe_amount` atoms can be worth nothing for high-decimal or very
    // cheap tokens (the output rounds to zero), so each direction may
    // escalate by ×1000 up to 10^9 × the base probe.
    let quotes = |input, output| {
        (0..4).any(|step| {
            probe_amount
                .checked_mul(1_000_u64.pow(step))
                .is_some_and(|amount| snapshot.quote(input, output, amount, true, 0).is_ok())
        })
    };
    let functional = quotes(a, b) && quotes(b, a);
    let tier = if functional {
        Tier::Verified
    } else {
        Tier::Quarantined
    };
    (tier, Some((a, b)))
}

pub fn spawn_discovery(
    registry: Arc<Registry>,
    chain: ChainClient,
    programs: Programs,
    interval: Duration,
    probe_amount: u64,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            registry.sweep(&chain, &programs, probe_amount).await;
        }
    });
}
