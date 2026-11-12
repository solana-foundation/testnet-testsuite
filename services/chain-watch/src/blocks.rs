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
const RANGE: u64 = 500;
/// A `getBlocks` range that lists no block at all is taken as skipped only
/// once the confirmed tip is this far past its end; before that the node
/// answering is more likely behind than the cluster silent.
const EMPTY_RANGE_MARGIN: u64 = RANGE;
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

pub async fn run(shared: Arc<Shared>, raw_dir: PathBuf) {
    // Last slot of a gap being recorded in `slots` mode (0: none).
    let mut backfill_to = 0;
    loop {
        if let Err(error) = step(&shared, &raw_dir, &mut backfill_to).await {
            warn!(error = %format!("{error:#}"), "block pipeline step failed");
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

async fn step(shared: &Shared, raw_dir: &Path, backfill_to: &mut u64) -> Result<()> {
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
    if lag > shared.cfg.max_lag_slots && *backfill_to < cursor {
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
        if backfilled {
            counter!("chain_block_backfill_slots_total").increment(lag);
        } else {
            counter!("chain_block_gap_slots_total").increment(lag);
        }
        shared
            .store
            .event(
                now_us(),
                Some(cursor),
                kind::GAP,
                &format!("{cursor}-{tip}"),
                json!({
                    "from": cursor, "to": tip, "slots": lag, "backfilled": backfilled,
                }),
            )
            .await?;
        if !backfilled {
            shared.cursor.store(tip, Ordering::Relaxed);
            return Ok(());
        }
        *backfill_to = tip;
    }
    let backfilling = cursor <= *backfill_to;
    let mut end = tip.min(cursor + RANGE - 1);
    if backfilling {
        end = end.min(*backfill_to);
    }
    let listed = shared
        .rpc
        .call(
            "getBlocks",
            json!([cursor, end, { "commitment": "confirmed" }]),
        )
        .await?;
    let produced = produced_set(&listed);
    let Some(until) = decided_until(end, tip, &produced) else {
        // The node that answered is behind the tip: ask again shortly.
        tokio::time::sleep(NOT_YET_WAIT).await;
        return Ok(());
    };
    if let Err(error) = ensure_leaders(shared, cursor, until).await {
        warn!(error = %format!("{error:#}"), "leader schedule unavailable");
    }
    let slots_only = backfilling || shared.block_details == BlockDetails::Slots;
    let mut blocks = stream::iter(cursor..=until)
        .map(|slot| {
            let produced = produced.contains(&slot);
            async move {
                let fetched = match (produced, slots_only) {
                    (false, _) => Fetched::Skipped,
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
    Ok(())
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

/// The last slot of `cursor..=end` that a `getBlocks` reply lets us decide
/// (`None`: none yet). `getSlot` and `getBlocks` may be answered by different
/// nodes behind a load balancer, so a slot missing from the list is skipped
/// only below the newest block the list holds: the tail after it may just be
/// missing from a node that is behind, and is left for the next step. An
/// empty list is accepted only once the tip is well past the range.
fn decided_until(end: u64, tip: u64, produced: &BTreeSet<u64>) -> Option<u64> {
    match produced.last() {
        Some(&last) => Some(last.min(end)),
        None if tip >= end.saturating_add(EMPTY_RANGE_MARGIN) => Some(end),
        None => None,
    }
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

    /// A JSON-RPC node whose `getBlocks` knows the blocks in `blocks` up to
    /// `node_tip`, as a node behind the cluster tip would.
    struct FakeNode {
        blocks: BTreeSet<u64>,
        node_tip: AtomicU64,
        get_block_calls: AtomicU64,
    }

    async fn answer(State(node): State<Arc<FakeNode>>, Json(request): Json<Value>) -> Json<Value> {
        let params = &request["params"];
        let result = match request["method"].as_str() {
            Some("getBlocks") => {
                let (from, to) = (
                    params[0].as_u64().expect("test setup"),
                    params[1].as_u64().expect("test setup"),
                );
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

    async fn rows(shared: &Shared, from: u64, to: u64) -> Vec<(u64, SlotStatus)> {
        shared.store.flush(Duration::from_secs(5));
        let rows = shared
            .store
            .read(move |conn| store::slots(conn, from, to, u32::MAX))
            .await
            .expect("test setup");
        rows.into_iter().map(|r| (r.slot, r.status)).collect()
    }

    #[tokio::test]
    async fn slots_a_lagging_node_has_not_listed_are_not_recorded_skipped() {
        let node = Arc::new(FakeNode {
            blocks: BTreeSet::from([1_000, 1_003]),
            node_tip: AtomicU64::new(1_001),
            get_block_calls: AtomicU64::new(0),
        });
        let (shared, dir) = harness("lagging-node", &node, 1_000, 1_003).await;
        let mut backfill_to = 0;
        step(&shared, &dir, &mut backfill_to)
            .await
            .expect("test setup");
        step(&shared, &dir, &mut backfill_to)
            .await
            .expect("test setup");
        // 1_001..=1_003 are past what the node lists: undecided.
        assert_eq!(shared.cursor.load(Ordering::Relaxed), 1_001);
        assert_eq!(
            rows(&shared, 1_000, 1_003).await,
            vec![(1_000, SlotStatus::Produced)]
        );
        node.node_tip.store(1_003, Ordering::Relaxed);
        step(&shared, &dir, &mut backfill_to)
            .await
            .expect("test setup");
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
        let node = Arc::new(FakeNode {
            blocks: (cursor..=tip + 10).filter(|s| s % 2 == 0).collect(),
            node_tip: AtomicU64::new(tip),
            get_block_calls: AtomicU64::new(0),
        });
        let (shared, dir) = harness("backfill", &node, cursor, tip).await;
        let mut backfill_to = 0;
        for _ in 0..10 {
            step(&shared, &dir, &mut backfill_to)
                .await
                .expect("test setup");
        }
        assert_eq!(shared.cursor.load(Ordering::Relaxed), tip + 1);
        // Every slot of the gap has a row, without one getBlock.
        let recorded = rows(&shared, cursor, tip).await;
        assert_eq!(recorded.len() as u64, tip - cursor + 1);
        assert!(recorded.iter().all(|(slot, status)| {
            *status
                == if slot % 2 == 0 {
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
        // Past the gap, blocks are read again.
        node.node_tip.store(tip + 2, Ordering::Relaxed);
        shared.confirmed.store(tip + 2, Ordering::Relaxed);
        step(&shared, &dir, &mut backfill_to)
            .await
            .expect("test setup");
        assert_eq!(node.get_block_calls.load(Ordering::Relaxed), 1);
        assert_eq!(shared.cursor.load(Ordering::Relaxed), tip + 3);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn rpc_error(code: i64) -> Result<Value> {
        Err(RpcError {
            code,
            message: format!("error {code}"),
        }
        .into())
    }

    #[test]
    fn a_tail_missing_from_get_blocks_waits_for_the_next_step() {
        // getSlot says 1_003, the getBlocks node only has up to 1_000.
        let produced = produced_set(&json!([998, 1_000]));
        assert_eq!(decided_until(1_003, 1_003, &produced), Some(1_000));
        // Next step (cursor 1_001): that node is still behind, nothing listed.
        let empty = produced_set(&json!([]));
        assert_eq!(decided_until(1_003, 1_003, &empty), None);
        assert_eq!(
            decided_until(1_003, 1_003 + EMPTY_RANGE_MARGIN - 1, &empty),
            None
        );
        // It caught up: 1_001 and 1_002 were skipped, 1_003 produced.
        let produced = produced_set(&json!([1_003]));
        assert_eq!(decided_until(1_003, 1_004, &produced), Some(1_003));
    }

    #[test]
    fn an_empty_range_is_skipped_once_the_tip_is_well_past_it() {
        let empty = produced_set(&json!([]));
        assert_eq!(
            decided_until(1_499, 1_499 + EMPTY_RANGE_MARGIN, &empty),
            Some(1_499)
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
