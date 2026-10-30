//! Wire types and a small client for the chain-watch service's HTTP API
//! (docs/dashboard-plan.md). Addresses travel as base58 strings and
//! timestamps as unix microseconds, so consumers need no Solana crates.

use serde::{Deserialize, Serialize};

/// `GET /health`: where the observer is and whether it is keeping up.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Health {
    pub run_id: String,
    /// `full`, `accounts` or `slots` (see [`BlockDetails`]).
    pub block_details: BlockDetails,
    /// Host of the RPC endpoint (never the URL: it carries the API key).
    pub rpc_host: String,
    pub processed_slot: u64,
    pub confirmed_slot: u64,
    /// Next slot the block pipeline will record.
    pub cursor_slot: u64,
    pub lag_slots: u64,
    /// Participant wallets recognised as ours (0: scenario not resolved).
    pub our_wallets: usize,
}

/// How much of each block chain-watch fetches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BlockDetails {
    /// Every transaction with its instructions and meta: CU, fees, priority
    /// fees, failing program. Needs a dedicated RPC at 5 blocks/s.
    #[default]
    Full,
    /// Account keys and a partial meta only: counts, fee payers, errors and
    /// fees, no CU.
    Accounts,
    /// No `getBlock`: produced vs skipped and the leader, from one
    /// `getBlocks` per range; TPS from performance samples. The default on
    /// public endpoints, whose quotas can't take a block per slot.
    Slots,
}

/// One slot as observed at `confirmed`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SlotRecord {
    pub slot: u64,
    pub leader: Option<String>,
    /// `clientId` from `getClusterNodes` (Agave, Firedancer, JitoLabs, ...).
    pub leader_client: Option<String>,
    pub leader_version: Option<String>,
    pub status: SlotStatus,
    pub block_time: Option<i64>,
    pub block_height: Option<u64>,
    pub tx_total: u32,
    pub tx_vote: u32,
    pub tx_failed: u32,
    pub tx_ours: u32,
    pub tx_ours_failed: u32,
    /// Sum of `computeUnitsConsumed` (full details only).
    pub cu_consumed: Option<u64>,
    /// Sum of `costUnits`, the block-limit currency (full details only).
    pub cost_units: Option<u64>,
    pub fees_lamports: u64,
    /// Non-vote priority prices, micro-lamports per CU (full details only).
    pub prio_p50: Option<u64>,
    pub prio_p90: Option<u64>,
    pub prio_max: Option<u64>,
    pub observed_at_us: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotStatus {
    Produced,
    Skipped,
    /// The RPC could not serve the block after retries.
    Unavailable,
}

impl SlotStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SlotStatus::Produced => "produced",
            SlotStatus::Skipped => "skipped",
            SlotStatus::Unavailable => "unavailable",
        }
    }
}

/// A feature gate account on the cluster.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Feature {
    pub id: String,
    /// Description from the Agave feature set; `None` for ids it doesn't know.
    pub description: Option<String>,
    /// `None`: pending, activates at the next epoch boundary.
    pub activated_slot: Option<u64>,
    pub activated_time_us: Option<i64>,
    /// The time is extrapolated from the slot (the RPC had no block time).
    pub time_estimated: bool,
    /// First observed pending by this run (so it activated while watched).
    pub first_seen_pending_us: Option<i64>,
}

/// One of our transactions as it landed (`GET /txs?slot=`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OurTx {
    pub signature: String,
    pub slot: u64,
    pub payer: String,
    /// Participant role (`maker/clob-jup-tight`, `taker/3`, `faucet`, ...).
    pub role: String,
    pub ok: bool,
    /// Error kind (`InstructionError:Custom(6001)`).
    pub error: Option<String>,
    pub error_program: Option<String>,
    pub cu_consumed: Option<u64>,
    pub cu_limit: Option<u64>,
    /// micro-lamports per CU
    pub prio_price: Option<u64>,
    pub fee_lamports: u64,
    /// Top-level programs invoked, comma-separated names.
    pub programs: String,
}

/// Something that happened, for the timeline.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub id: i64,
    pub time_us: i64,
    pub slot: Option<u64>,
    pub kind: String,
    pub subject: String,
    pub detail: serde_json::Value,
}

/// Event kinds chain-watch emits.
pub mod kind {
    pub const FEATURE_PENDING: &str = "feature_pending";
    pub const FEATURE_ACTIVATED: &str = "feature_activated";
    pub const EPOCH: &str = "epoch";
    pub const MAJORITY_VERSION: &str = "majority_version";
    pub const GAP: &str = "gap";
    pub const WATCH_STARTED: &str = "watch_started";
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("chain-watch request failed: {0}")]
    Http(#[from] reqwest::Error),
}

/// Client for a running chain-watch.
#[derive(Clone)]
pub struct ChainWatchClient {
    http: reqwest::Client,
    base: String,
}

impl ChainWatchClient {
    pub fn new(base: impl Into<String>) -> Self {
        Self {
            // A stalled chain-watch must not stall its callers' loops.
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            base: base.into().trim_end_matches('/').to_owned(),
        }
    }

    async fn get<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, Error> {
        Ok(self
            .http
            .get(format!("{}{path}", self.base))
            .query(query)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn health(&self) -> Result<Health, Error> {
        self.get("/health", &[]).await
    }

    pub async fn features(&self) -> Result<Vec<Feature>, Error> {
        self.get("/features", &[]).await
    }

    /// Events with `id > after`, oldest first.
    pub async fn events(&self, after: i64) -> Result<Vec<Event>, Error> {
        self.get("/events", &[("after", after.to_string())]).await
    }

    /// Slots in `from..=to` (the server caps the span).
    pub async fn slots(&self, from: u64, to: u64) -> Result<Vec<SlotRecord>, Error> {
        self.get(
            "/slots",
            &[("from", from.to_string()), ("to", to.to_string())],
        )
        .await
    }

    /// Our transactions that landed in `slot`.
    pub async fn txs(&self, slot: u64) -> Result<Vec<OurTx>, Error> {
        self.get("/txs", &[("slot", slot.to_string())]).await
    }
}
