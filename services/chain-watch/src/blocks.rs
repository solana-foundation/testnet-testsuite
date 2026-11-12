//! The block pipeline: every slot from the cursor up to the confirmed tip,
//! skipped or produced, summarised into metrics and the run store.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use chain_watch_client::{BlockDetails, SlotRecord, SlotStatus, kind};
use futures::{StreamExt, stream};
use metrics::{counter, gauge, histogram};
use mtm_common::time::now_us;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::block::{self, BlockSummary, percentile};
use crate::cluster::ensure_leaders;
use crate::config::RawBlocks;
use crate::rpc::{is_missing, is_not_yet};
use crate::state::Shared;
use crate::store::Write;

/// Slots per `getBlocks` range.
pub const RANGE: u64 = 500;
/// Consecutive `getBlocks` replies that may lack the anchor (the node that
/// answered does not hold that part of the ledger) before the slots below
/// the first block they list are recorded `unavailable`.
const ANCHOR_ATTEMPTS: u32 = 5;
/// Consecutive undecided steps (at [`NOT_YET_WAIT`]) between warnings:
/// about 10 s.
const WAIT_WARN_EVERY: u32 = 25;
/// Widest gap recorded in `slots` mode after falling behind (about 100
/// `getBlocks` calls); a wider one is dropped with only a `gap` event.
const MAX_BACKFILL_SLOTS: u64 = 50_000;
/// Wait before asking again when the RPC has not caught up with the tip.
const NOT_YET_WAIT: Duration = Duration::from_millis(400);

#[derive(Debug, PartialEq)]
enum Fetched {
    Produced(Value),
    /// Produced, block not fetched (`slots` mode, or a backfilled gap).
    Unread,
    Skipped,
    Unavailable,
}

/// What the pipeline carries from one step to the next.
#[derive(Debug, Default)]
struct Progress {
    /// Last slot of a gap being recorded in `slots` mode (0: none).
    backfill_to: u64,
    /// Newest slot below the cursor known to be produced (listed by an
    /// earlier `getBlocks`). Every `getBlocks` starts there: a reply that
    /// lists it comes from a node holding the ledger from there on, so a
    /// slot it leaves out is skipped; a reply without it proves nothing.
    /// `None`: the cursor itself, which starts at the confirmed tip (a
    /// produced slot, since `getSlot` answers with a block).
    anchor: Option<u64>,
    /// Consecutive replies without the anchor.
    anchor_misses: u32,
    /// Consecutive steps that could decide nothing.
    waits: u32,
}

pub async fn run(shared: Arc<Shared>, raw_dir: PathBuf) {
    let mut progress = Progress::default();
    loop {
        if let Err(error) = step(&shared, &raw_dir, &mut progress).await {
            warn!(error = %format!("{error:#}"), "block pipeline step failed");
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

async fn step(shared: &Shared, raw_dir: &Path, progress: &mut Progress) -> Result<()> {
    let tip = shared.confirmed();
    let mut cursor = shared.cursor.load(Ordering::Relaxed);
    if tip == 0 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        return Ok(());
    }
    if cursor == 0 {
        // Start at the tip: the run store records what happens from now on.
        cursor = tip;
        shared.cursor.store(cursor, Ordering::Relaxed);
        info!(slot = cursor, "block pipeline starting");
        shared
            .store
            .event(
                now_us(),
                Some(cursor),
                kind::WATCH_STARTED,
                &shared.run_id,
                json!({
                    "rpc_host": shared.rpc_host,
                    "block_details": shared.block_details,
                }),
            )
            .await?;
    }
    if tip < cursor {
        tokio::time::sleep(Duration::from_millis(100)).await;
        return Ok(());
    }
    let lag = tip - cursor;
    gauge!("chain_block_lag_slots").set(lag as f64);
    if lag > shared.cfg.max_lag_slots && progress.backfill_to < cursor {
        // Fell behind: record the gap in `slots` mode (produced or skipped
        // and the leader, one `getBlocks` per range) and resume reading
        // blocks at the tip. Only a gap too wide for that is dropped.
        let backfilled = lag <= MAX_BACKFILL_SLOTS;
        warn!(
            from = cursor,
            to = tip,
            backfilled,
            "block pipeline fell behind; skipping ahead"
        );
        // `cursor..=tip`, both ends included.
        let slots = lag + 1;
        shared
            .store
            .event(
                now_us(),
                Some(cursor),
                kind::GAP,
                &format!("{cursor}-{tip}"),
                json!({
                    "from": cursor, "to": tip, "slots": slots, "backfilled": backfilled,
                }),
            )
            .await?;
        // Counted once the event is stored: a failed insert retries the step.
        if backfilled {
            counter!("chain_block_backfill_slots_total").increment(slots);
        } else {
            counter!("chain_block_gap_slots_total").increment(slots);
        }
        if !backfilled {
            shared.cursor.store(tip, Ordering::Relaxed);
            progress.anchor = None;
            return Ok(());
        }
        progress.backfill_to = tip;
    }
    let backfilling = cursor <= progress.backfill_to;
    // A slot known to be produced at or past every range asked for: the
    // tip (`getSlot` answers with a block), or the tip a backfill started at.
    let upper = if backfilling {
        progress.backfill_to
    } else {
        tip
    };
    let anchor = progress.anchor.unwrap_or(cursor).min(cursor);
    let narrow_end = upper.min(cursor + RANGE - 1);
    let mut produced = list_blocks(shared, anchor, narrow_end).await?;
    let widened = produced.range(cursor..).next().is_none() && narrow_end < upper;
    if widened {
        // Nothing listed in the range: ask once more up to `upper`. One
        // reply listing the anchor and a block past the range shows that
        // the node answering holds all of it, so the slots between were
        // skipped, however long the outage. (Two separate calls would
        // prove nothing: another node may answer each.)
        counter!("chain_block_widened_ranges_total").increment(1);
        produced = list_blocks(shared, anchor, upper).await?;
    }
    let end = if widened { upper } else { narrow_end };
    let exhausted = progress.anchor_misses + 1 >= ANCHOR_ATTEMPTS;
    let Some(decision) = decide(cursor, end, anchor, widened, exhausted, &produced) else {
        progress.waits += 1;
        if !produced.contains(&anchor) {
            progress.anchor_misses += 1;
            counter!("chain_block_anchor_missing_total").increment(1);
        }
        counter!("chain_block_undecided_waits_total").increment(1);
        if progress.waits.is_multiple_of(WAIT_WARN_EVERY) {
            warn!(
                cursor,
                tip,
                anchor,
                anchor_listed = produced.contains(&anchor),
                waits = progress.waits,
                "getBlocks has not listed the slots after the cursor yet; waiting"
            );
        }
        // The node that answered is behind the tip, or does not hold this
        // part of the ledger: ask again shortly.
        tokio::time::sleep(NOT_YET_WAIT).await;
        return Ok(());
    };
    if decision.unavailable_below > cursor {
        warn!(
            from = cursor,
            to = decision.unavailable_below - 1,
            anchor,
            "getBlocks never listed the anchor; recording the slots before \
             its first block as unavailable"
        );
    }
    let until = decision.until;
    if let Err(error) = ensure_leaders(shared, cursor, until).await {
        warn!(error = %format!("{error:#}"), "leader schedule unavailable");
    }
    let slots_only = backfilling || shared.block_details == BlockDetails::Slots;
    let mut blocks = stream::iter(cursor..=until)
        .map(|slot| {
            let produced = produced.contains(&slot);
            let known = slot >= decision.unavailable_below;
            async move {
                let fetched = match (produced, slots_only) {
                    (false, _) if known => Fetched::Skipped,
                    (false, _) => Fetched::Unavailable,
                    (true, true) => Fetched::Unread,
                    (true, false) => fetch(shared, slot).await,
                };
                (slot, fetched)
            }
        })
        .buffered(shared.cfg.block_concurrency.max(1));
    while let Some((slot, fetched)) = blocks.next().await {
        record(shared, raw_dir, slot, fetched, tip);
        shared.cursor.store(slot + 1, Ordering::Relaxed);
        gauge!("chain_cursor_slot").set(slot as f64);
    }
    // `until` is a listed slot: the next anchor.
    progress.anchor = Some(until);
    progress.anchor_misses = 0;
    progress.waits = 0;
    Ok(())
}

async fn list_blocks(shared: &Shared, from: u64, to: u64) -> Result<BTreeSet<u64>> {
    let listed = shared
        .rpc
        .call(
            "getBlocks",
            json!([from, to, { "commitment": "confirmed" }]),
        )
        .await?;
    Ok(produced_set(&listed))
}

/// Slots a `getBlocks` reply lists as produced.
fn produced_set(listed: &Value) -> BTreeSet<u64> {
    listed
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_u64)
        .collect()
}

/// What one `getBlocks(anchor, end)` reply lets the pipeline record.
#[derive(Debug, PartialEq)]
struct Decision {
    /// Record `cursor..=until`; always a listed slot.
    until: u64,
    /// Unlisted slots below this are `unavailable`, not skipped (the node
    /// never showed it holds them); `cursor` when the anchor was listed.
    unavailable_below: u64,
}

/// `getSlot` and `getBlocks` may be answered by different nodes behind a
/// load balancer, and a node lists nothing below its first available slot
/// or past its own tip, so an unlisted slot is skipped only between two
/// listed ones: below, the anchor (a slot known produced) must be in the
/// reply; above, a listed block. The tail after the newest listed block
/// waits for the next step. `None`: nothing can be decided yet. After
/// [`ANCHOR_ATTEMPTS`] replies without the anchor (`exhausted`), the slots
/// below the first listed block are given up as unavailable. A `widened`
/// reply is decided only up to its first block past the cursor, to keep
/// one step's work to about a range.
fn decide(
    cursor: u64,
    end: u64,
    anchor: u64,
    widened: bool,
    exhausted: bool,
    produced: &BTreeSet<u64>,
) -> Option<Decision> {
    let mut listed = produced.range(cursor..=end);
    let first = *listed.next()?;
    let last = listed.next_back().copied().unwrap_or(first);
    let unavailable_below = if produced.contains(&anchor) {
        cursor
    } else if exhausted {
        first
    } else {
        return None;
    };
    Some(Decision {
        until: if widened { first } else { last },
        unavailable_below,
    })
}

async fn fetch(shared: &Shared, slot: u64) -> Fetched {
    let details = match shared.block_details {
        BlockDetails::Full => "full",
        BlockDetails::Accounts => "accounts",
        BlockDetails::Slots => return Fetched::Unread,
    };
    let params = json!([slot, {
        "encoding": "json", "transactionDetails": details, "rewards": false,
        "maxSupportedTransactionVersion": 1, "commitment": "confirmed",
    }]);
    let attempts = shared.cfg.block_attempts.max(1);
    for attempt in 1..=attempts {
        let result = shared.rpc.call("getBlock", params.clone()).await;
        let error = match &result {
            Ok(block) if block.is_null() => "not confirmed on this node yet".to_owned(),
            Ok(_) => String::new(),
            Err(error) => format!("{error:#}"),
        };
        if let Some(fetched) = outcome(result, attempt, attempts) {
            if fetched == Fetched::Unavailable {
                warn!(slot, attempt, error, "block unavailable");
            }
            return fetched;
        }
        tracing::debug!(slot, attempt, error, "block not ready");
        tokio::time::sleep(Duration::from_millis(400 * u64::from(attempt))).await;
    }
    Fetched::Unavailable
}

/// One `getBlock` attempt for a slot `getBlocks` listed as produced (`None`:
/// try again). Such a slot is never recorded as skipped: `null` means the
/// answering node has not confirmed it yet (retried like "not available
/// yet"), and -32007/-32009 that it is missing from that node's ledger
/// (retried like any other error, since another node may serve it); both
/// end up `Unavailable`.
fn outcome(result: Result<Value>, attempt: u32, attempts: u32) -> Option<Fetched> {
    let retry = match result {
        Ok(block) if !block.is_null() => return Some(Fetched::Produced(block)),
        Ok(_) => true,
        Err(error) if is_not_yet(&error) => true,
        // Never `Skipped`: only `getBlocks` decides that. Three attempts,
        // like any other error (behind a load balancer another node may
        // have it).
        Err(error) if is_missing(&error) => attempt < 3,
        Err(_) => attempt < 3,
    };
    (!retry || attempt >= attempts).then_some(Fetched::Unavailable)
}

fn record(shared: &Shared, raw_dir: &Path, slot: u64, fetched: Fetched, tip: u64) {
    let leader = shared.leader(slot);
    let leader_client = leader
        .as_ref()
        .and_then(|(_, node)| node.as_ref())
        .map_or_else(|| "unknown".to_owned(), |node| node.client.clone());
    let (status, summary, raw) = match fetched {
        Fetched::Produced(block) => {
            let summary = block::summarize(&block, &shared.known());
            (SlotStatus::Produced, summary, Some(block))
        }
        Fetched::Unread => (SlotStatus::Produced, BlockSummary::default(), None),
        Fetched::Skipped => (SlotStatus::Skipped, BlockSummary::default(), None),
        Fetched::Unavailable => (SlotStatus::Unavailable, BlockSummary::default(), None),
    };
    counter!("chain_slots_total", "status" => status.as_str(), "leader_client" => leader_client.clone())
        .increment(1);
    gauge!("chain_block_lag_slots").set(tip.saturating_sub(slot) as f64);
    let mut prices = summary.prio_prices.clone();
    // Only for blocks actually read (not `slots` mode).
    if raw.is_some() {
        observe(shared, &summary, &leader_client);
    }
    let ours_failed = summary.ours.iter().filter(|t| !t.ok).count() as u32;
    let record = SlotRecord {
        slot,
        leader: leader.as_ref().map(|(identity, _)| identity.clone()),
        leader_client: leader.as_ref().and(Some(leader_client)),
        leader_version: leader
            .as_ref()
            .and_then(|(_, node)| node.as_ref())
            .map(|node| node.version.clone()),
        status,
        block_time: summary.block_time,
        block_height: summary.block_height,
        tx_total: summary.tx_total,
        tx_vote: summary.tx_vote,
        tx_failed: summary.tx_failed,
        tx_ours: summary.ours.len() as u32,
        tx_ours_failed: ours_failed,
        cu_consumed: summary.cu_consumed,
        cost_units: summary.cost_units,
        fees_lamports: summary.fees_lamports,
        prio_p50: percentile(&mut prices, 50),
        prio_p90: percentile(&mut prices, 90),
        prio_max: prices.last().copied(),
        observed_at_us: now_us(),
    };
    shared.store.write(Write::Slot(record));
    let keep_raw = match shared.cfg.raw_blocks {
        RawBlocks::Off => false,
        RawBlocks::Failed => summary.ours.iter().any(|tx| !tx.ok),
        RawBlocks::Ours => !summary.ours.is_empty(),
        RawBlocks::All => true,
    };
    if !summary.ours.is_empty() {
        shared.store.write(Write::Txs(slot, summary.ours));
    }
    if keep_raw && let Some(raw) = raw {
        let path = raw_dir.join(format!("{slot}.json.zst"));
        tokio::task::spawn_blocking(move || {
            let written = serde_json::to_vec(&raw)
                .map_err(anyhow::Error::from)
                .and_then(|bytes| Ok(zstd::encode_all(bytes.as_slice(), 3)?))
                .and_then(|compressed| Ok(std::fs::write(&path, compressed)?));
            if let Err(error) = written {
                warn!(slot, error = %format!("{error:#}"), "writing raw block failed");
            }
        });
    }
}

/// Metrics for one produced block.
fn observe(shared: &Shared, summary: &BlockSummary, leader_client: &str) {
    let non_vote = summary.tx_total - summary.tx_vote;
    counter!("chain_block_transactions_total", "kind" => "vote")
        .increment(u64::from(summary.tx_vote));
    counter!("chain_block_transactions_total", "kind" => "non_vote").increment(u64::from(non_vote));
    counter!("chain_block_failed_transactions_total").increment(u64::from(summary.tx_failed));
    counter!("chain_block_fees_lamports_total").increment(summary.fees_lamports);
    histogram!("chain_block_non_vote_transactions").record(f64::from(non_vote));
    if let Some(cu) = summary.cu_consumed {
        counter!("chain_block_cu_total").increment(cu);
        histogram!("chain_block_cu").record(cu as f64);
    }
    if let Some(cost) = summary.cost_units {
        histogram!("chain_block_cost_units").record(cost as f64);
        if let Some(limit) = shared.cfg.block_cost_limit.filter(|l| *l > 0) {
            histogram!("chain_block_fill_ratio").record(cost as f64 / limit as f64);
        }
    }
    for price in &summary.prio_prices {
        histogram!("chain_prio_price_micro_lamports", "scope" => "all").record(*price as f64);
    }
    for tx in &summary.ours {
        let actor_kind = tx.role.split('/').next().unwrap_or("unknown").to_owned();
        let program = tx
            .programs
            .iter()
            .find(|p| !matches!(**p, "system" | "ata"))
            .copied()
            .or_else(|| tx.programs.first().copied())
            .unwrap_or("none");
        counter!("chain_ours_transactions_total",
            "actor_kind" => actor_kind.clone(),
            "program" => program,
            "status" => if tx.ok { "ok" } else { "failed" },
            "error" => tx.error.clone().unwrap_or_default(),
            "error_program" => tx.error_program.unwrap_or(""))
        .increment(1);
        counter!("chain_ours_by_leader_client_total",
            "leader_client" => leader_client.to_owned(),
            "status" => if tx.ok { "ok" } else { "failed" })
        .increment(1);
        counter!("chain_ours_fees_lamports_total", "actor_kind" => actor_kind)
            .increment(tx.fee_lamports);
        if let Some(cu) = tx.cu_consumed {
            histogram!("chain_ours_cu", "program" => program).record(cu as f64);
        }
        if let (Some(cu), Some(limit)) = (tx.cu_consumed, tx.cu_limit.filter(|l| *l > 0)) {
            histogram!("chain_ours_cu_limit_used_ratio", "program" => program)
                .record(cu as f64 / limit as f64);
        }
        if let Some(price) = tx.prio_price {
            histogram!("chain_prio_price_micro_lamports", "scope" => "ours").record(price as f64);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::RwLock;
    use std::sync::atomic::{AtomicU64, AtomicUsize};

    use axum::extract::State;
    use axum::{Json, Router};
    use tokio::sync::Notify;

    use super::*;
    use crate::block::Known;
    use crate::rpc::{JsonRpc, RpcError};
    use crate::store::{self, Store};

    /// A JSON-RPC node whose `getBlocks` knows the blocks in `blocks` from
    /// `first_available` (its ledger starts there) up to `node_tip`, as a
    /// node behind the cluster tip would.
    struct FakeNode {
        blocks: BTreeSet<u64>,
        first_available: AtomicU64,
        node_tip: AtomicU64,
        get_block_calls: AtomicU64,
    }

    impl FakeNode {
        fn new(blocks: impl IntoIterator<Item = u64>, node_tip: u64) -> Arc<Self> {
            Arc::new(Self {
                blocks: blocks.into_iter().collect(),
                first_available: AtomicU64::new(0),
                node_tip: AtomicU64::new(node_tip),
                get_block_calls: AtomicU64::new(0),
            })
        }
    }

    async fn answer(State(node): State<Arc<FakeNode>>, Json(request): Json<Value>) -> Json<Value> {
        let params = &request["params"];
        let result = match request["method"].as_str() {
            Some("getBlocks") => {
                let (from, to) = (
                    params[0].as_u64().expect("test setup"),
                    params[1].as_u64().expect("test setup"),
                );
                let from = from.max(node.first_available.load(Ordering::Relaxed));
                let to = to.min(node.node_tip.load(Ordering::Relaxed));
                json!(
                    node.blocks
                        .range(from..=to.max(from))
                        .filter(|s| **s <= to)
                        .collect::<Vec<_>>()
                )
            }
            Some("getBlock") => {
                node.get_block_calls.fetch_add(1, Ordering::Relaxed);
                json!({ "blockHeight": params[0] })
            }
            _ => json!([]),
        };
        Json(json!({ "jsonrpc": "2.0", "id": 1, "result": result }))
    }

    /// Shared state against a fake node, with the pipeline at `cursor`
    /// and the confirmed tip at `tip`.
    async fn harness(name: &str, node: &Arc<FakeNode>, cursor: u64, tip: u64) -> (Shared, PathBuf) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test setup");
        let url = format!("http://{}", listener.local_addr().expect("test setup"));
        let app = Router::new()
            .route("/", axum::routing::post(answer))
            .with_state(Arc::clone(node));
        tokio::spawn(async move { axum::serve(listener, app).await });
        let dir = std::env::temp_dir().join(format!("chain-watch-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let shared = Shared {
            cfg: serde_json::from_value(json!({ "listen_addr": "127.0.0.1:0" }))
                .expect("test setup"),
            rpc: JsonRpc::new(url, None).expect("test setup"),
            store: Store::open(&dir, &[]).expect("test setup"),
            run_id: name.to_owned(),
            rpc_host: "fake".to_owned(),
            block_details: BlockDetails::Full,
            public_endpoint: false,
            known: RwLock::new(Arc::new(Known::default())),
            our_wallets: AtomicUsize::new(0),
            processed: AtomicU64::new(tip),
            confirmed: AtomicU64::new(tip),
            cursor: AtomicU64::new(cursor),
            leaders: RwLock::default(),
            nodes: RwLock::new(HashMap::new()),
            slot_seconds: RwLock::new(0.4),
            epoch_changed: Notify::new(),
        };
        (shared, dir)
    }

    async fn records(shared: &Shared, from: u64, to: u64) -> Vec<SlotRecord> {
        shared.store.flush(Duration::from_secs(5));
        shared
            .store
            .read(move |conn| store::slots(conn, from, to, u32::MAX))
            .await
            .expect("test setup")
    }

    async fn rows(shared: &Shared, from: u64, to: u64) -> Vec<(u64, SlotStatus)> {
        records(shared, from, to)
            .await
            .into_iter()
            .map(|r| (r.slot, r.status))
            .collect()
    }

    async fn steps(shared: &Shared, dir: &Path, progress: &mut Progress, n: usize) {
        for _ in 0..n {
            step(shared, dir, progress).await.expect("test setup");
        }
    }

    #[tokio::test]
    async fn slots_a_lagging_node_has_not_listed_are_not_recorded_skipped() {
        let node = FakeNode::new([1_000, 1_003], 1_001);
        let (shared, dir) = harness("lagging-node", &node, 1_000, 1_003).await;
        let mut progress = Progress::default();
        steps(&shared, &dir, &mut progress, 2).await;
        // 1_001..=1_003 are past what the node lists: undecided.
        assert_eq!(shared.cursor.load(Ordering::Relaxed), 1_001);
        assert_eq!(
            rows(&shared, 1_000, 1_003).await,
            vec![(1_000, SlotStatus::Produced)]
        );
        node.node_tip.store(1_003, Ordering::Relaxed);
        steps(&shared, &dir, &mut progress, 1).await;
        assert_eq!(
            rows(&shared, 1_000, 1_003).await,
            vec![
                (1_000, SlotStatus::Produced),
                (1_001, SlotStatus::Skipped),
                (1_002, SlotStatus::Skipped),
                (1_003, SlotStatus::Produced),
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_gap_is_backfilled_in_slots_mode_then_blocks_are_read_again() {
        let (cursor, tip) = (10_000, 13_500);
        let node = FakeNode::new((cursor..=tip + 10).filter(|s| s % 2 == 0), tip);
        let (shared, dir) = harness("backfill", &node, cursor, tip).await;
        let mut progress = Progress::default();
        steps(&shared, &dir, &mut progress, 10).await;
        assert_eq!(shared.cursor.load(Ordering::Relaxed), tip + 1);
        // Every slot of the gap has a row, without one getBlock.
        let recorded = records(&shared, cursor, tip).await;
        assert_eq!(recorded.len() as u64, tip - cursor + 1);
        assert!(recorded.iter().all(|r| {
            let produced = r.slot % 2 == 0;
            r.status
                == if produced {
                    SlotStatus::Produced
                } else {
                    SlotStatus::Skipped
                }
        }));
        assert_eq!(node.get_block_calls.load(Ordering::Relaxed), 0);
        let events = shared
            .store
            .read(|conn| store::events(conn, 0, 10))
            .await
            .expect("test setup");
        let gap = events
            .iter()
            .find(|e| e.kind == kind::GAP)
            .expect("gap event");
        assert_eq!(gap.detail["backfilled"], json!(true));
        // `cursor..=tip` inclusive.
        assert_eq!(gap.detail["slots"], json!(tip - cursor + 1));
        // Past the gap, blocks are read again.
        node.node_tip.store(tip + 2, Ordering::Relaxed);
        shared.confirmed.store(tip + 2, Ordering::Relaxed);
        steps(&shared, &dir, &mut progress, 1).await;
        assert_eq!(node.get_block_calls.load(Ordering::Relaxed), 1);
        assert_eq!(shared.cursor.load(Ordering::Relaxed), tip + 3);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_gap_whose_event_fails_is_counted_once() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _local = metrics::set_default_local_recorder(&recorder);
        let (cursor, tip) = (10_000, 13_500);
        let node = FakeNode::new([cursor, tip], tip);
        let (shared, dir) = harness("gap-event-fails", &node, cursor, tip).await;
        let db = rusqlite::Connection::open(dir.join("run.db")).expect("test setup");
        db.execute_batch("ALTER TABLE events RENAME TO events_away")
            .expect("test setup");
        let mut progress = Progress::default();
        for _ in 0..3 {
            assert!(step(&shared, &dir, &mut progress).await.is_err());
        }
        db.execute_batch("ALTER TABLE events_away RENAME TO events")
            .expect("test setup");
        steps(&shared, &dir, &mut progress, 1).await;
        let counted = handle
            .render()
            .lines()
            .find_map(|line| line.strip_prefix("chain_block_backfill_slots_total "))
            .map(str::to_owned);
        assert_eq!(counted, Some((tip - cursor + 1).to_string()));
        std::fs::remove_dir_all(&dir).ok();
    }

    fn rpc_error(code: i64) -> Result<Value> {
        Err(RpcError {
            code,
            message: format!("error {code}"),
        }
        .into())
    }

    #[tokio::test]
    async fn a_range_below_the_nodes_ledger_is_not_recorded_skipped() {
        // Backfilling 10_000..=13_500 against a node restarted from a
        // snapshot at 12_000: it lists nothing below that.
        let (cursor, tip) = (10_000, 13_500);
        let node = FakeNode::new((cursor..=tip).filter(|s| s % 2 == 0), tip);
        node.first_available.store(12_000, Ordering::Relaxed);
        let (shared, dir) = harness("first-available", &node, cursor, tip).await;
        let mut progress = Progress::default();
        steps(&shared, &dir, &mut progress, ANCHOR_ATTEMPTS as usize - 1).await;
        // The anchor (10_000) is never listed: nothing decided yet.
        assert_eq!(shared.cursor.load(Ordering::Relaxed), cursor);
        assert_eq!(rows(&shared, cursor, tip).await, vec![]);
        // Given up: unavailable below the node's first block, not skipped.
        steps(&shared, &dir, &mut progress, 1).await;
        let recorded = rows(&shared, cursor, tip).await;
        assert_eq!(recorded.len(), 2_001);
        assert!(
            recorded[..2_000]
                .iter()
                .all(|(_, status)| *status == SlotStatus::Unavailable)
        );
        assert_eq!(recorded[2_000], (12_000, SlotStatus::Produced));
        // From there the node holds the ledger: skips are real again.
        steps(&shared, &dir, &mut progress, 1).await;
        assert_eq!(
            rows(&shared, 12_001, 12_002).await,
            vec![
                (12_001, SlotStatus::Skipped),
                (12_002, SlotStatus::Produced)
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn an_empty_range_from_a_node_far_behind_is_not_recorded_skipped() {
        // The tip is 2_000 slots past the range, but the node answering
        // getBlocks is at 1_200 and has not seen the block at 1_300.
        let node = FakeNode::new([1_000, 1_300, 3_000], 1_200);
        let (shared, dir) = harness("far-behind", &node, 1_000, 3_000).await;
        let mut progress = Progress::default();
        steps(&shared, &dir, &mut progress, 3).await;
        assert_eq!(shared.cursor.load(Ordering::Relaxed), 1_001);
        assert_eq!(
            rows(&shared, 1_000, 3_000).await,
            vec![(1_000, SlotStatus::Produced)]
        );
        node.node_tip.store(3_000, Ordering::Relaxed);
        steps(&shared, &dir, &mut progress, 1).await;
        let recorded = rows(&shared, 1_000, 1_300).await;
        assert_eq!(recorded.len(), 301);
        assert_eq!(recorded[300], (1_300, SlotStatus::Produced));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn an_outage_longer_than_a_range_is_decided_as_soon_as_blocks_resume() {
        // 699 skipped slots, then blocks again; the tip only 5 past them.
        let node = FakeNode::new([1_000, 1_700, 1_705], 1_705);
        let (shared, dir) = harness("outage", &node, 1_000, 1_705).await;
        let mut progress = Progress::default();
        steps(&shared, &dir, &mut progress, 3).await;
        assert_eq!(progress.waits, 0);
        assert_eq!(shared.cursor.load(Ordering::Relaxed), 1_706);
        let recorded = rows(&shared, 1_000, 1_705).await;
        assert_eq!(recorded.len(), 706);
        assert!(recorded.iter().all(|(slot, status)| {
            *status
                == if [1_000, 1_700, 1_705].contains(slot) {
                    SlotStatus::Produced
                } else {
                    SlotStatus::Skipped
                }
        }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_reply_is_decided_only_between_the_anchor_and_a_listed_block() {
        let set = |slots: &[u64]| produced_set(&json!(slots));
        // Anchor listed: up to the newest listed block; the tail waits.
        assert_eq!(
            decide(1_001, 1_003, 1_000, false, false, &set(&[1_000, 1_002])),
            Some(Decision {
                until: 1_002,
                unavailable_below: 1_001
            })
        );
        // Nothing listed after the cursor: undecided, however far the tip.
        assert_eq!(
            decide(1_001, 1_500, 1_000, false, false, &set(&[1_000])),
            None
        );
        // Anchor missing: undecided until the attempts run out.
        let no_anchor = set(&[1_200, 1_300]);
        assert_eq!(decide(1_001, 1_500, 1_000, false, false, &no_anchor), None);
        assert_eq!(
            decide(1_001, 1_500, 1_000, false, true, &no_anchor),
            Some(Decision {
                until: 1_300,
                unavailable_below: 1_200
            })
        );
        // A widened reply: up to its first block only.
        assert_eq!(
            decide(
                1_001,
                5_000,
                1_000,
                true,
                false,
                &set(&[1_000, 1_700, 1_705])
            ),
            Some(Decision {
                until: 1_700,
                unavailable_below: 1_001
            })
        );
        assert_eq!(produced_set(&Value::Null), BTreeSet::new());
    }

    #[test]
    fn a_listed_slot_is_never_recorded_as_skipped() {
        // `null`: not confirmed on the node that answered; retried, then unavailable.
        assert_eq!(outcome(Ok(Value::Null), 1, 6), None);
        assert_eq!(outcome(Ok(Value::Null), 6, 6), Some(Fetched::Unavailable));
        // Missing from that node's ledger / long-term storage.
        for code in [-32007, -32009] {
            assert_eq!(outcome(rpc_error(code), 2, 6), None);
            assert_eq!(outcome(rpc_error(code), 3, 6), Some(Fetched::Unavailable));
        }
        // Not available yet: retried up to the last attempt.
        assert_eq!(outcome(rpc_error(-32004), 5, 6), None);
        assert_eq!(outcome(rpc_error(-32004), 6, 6), Some(Fetched::Unavailable));
        // Other errors: three attempts.
        assert_eq!(outcome(rpc_error(-32600), 2, 6), None);
        assert_eq!(outcome(rpc_error(-32600), 3, 6), Some(Fetched::Unavailable));
        let block = json!({ "blockHeight": 5 });
        assert_eq!(
            outcome(Ok(block.clone()), 1, 6),
            Some(Fetched::Produced(block))
        );
    }
}
