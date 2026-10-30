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
use crate::rpc::{is_not_yet, is_skipped};
use crate::state::Shared;
use crate::store::Write;

/// Slots per `getBlocks` range.
const RANGE: u64 = 500;

enum Fetched {
    Produced(Value),
    /// Produced, block not fetched (`slots` mode).
    Unread,
    Skipped,
    Unavailable,
}

pub async fn run(shared: Arc<Shared>, raw_dir: PathBuf) {
    loop {
        if let Err(error) = step(&shared, &raw_dir).await {
            warn!(error = %format!("{error:#}"), "block pipeline step failed");
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

async fn step(shared: &Shared, raw_dir: &Path) -> Result<()> {
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
    if lag > shared.cfg.max_lag_slots {
        warn!(
            from = cursor,
            to = tip,
            "block pipeline fell behind; skipping ahead"
        );
        counter!("chain_block_gap_slots_total").increment(lag);
        shared
            .store
            .event(
                now_us(),
                Some(cursor),
                kind::GAP,
                &format!("{cursor}-{tip}"),
                json!({
                    "from": cursor, "to": tip, "slots": lag,
                }),
            )
            .await?;
        shared.cursor.store(tip, Ordering::Relaxed);
        return Ok(());
    }
    let end = tip.min(cursor + RANGE - 1);
    let produced: BTreeSet<u64> = shared
        .rpc
        .call(
            "getBlocks",
            json!([cursor, end, { "commitment": "confirmed" }]),
        )
        .await?
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_u64)
        .collect();
    if let Err(error) = ensure_leaders(shared, cursor, end).await {
        warn!(error = %format!("{error:#}"), "leader schedule unavailable");
    }
    let mut blocks = stream::iter(cursor..=end)
        .map(|slot| {
            let produced = produced.contains(&slot);
            async move {
                let fetched = if produced {
                    fetch(shared, slot).await
                } else {
                    Fetched::Skipped
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
    for attempt in 1..=shared.cfg.block_attempts.max(1) {
        match shared.rpc.call("getBlock", params.clone()).await {
            Ok(block) if block.is_null() => return Fetched::Skipped,
            Ok(block) => return Fetched::Produced(block),
            Err(error) if is_skipped(&error) => return Fetched::Skipped,
            Err(error) if is_not_yet(&error) || attempt < 3 => {
                tracing::debug!(slot, attempt, error = %format!("{error:#}"), "block not ready");
                tokio::time::sleep(Duration::from_millis(400 * u64::from(attempt))).await;
            }
            Err(error) => {
                warn!(slot, error = %format!("{error:#}"), "getBlock failed");
                return Fetched::Unavailable;
            }
        }
    }
    warn!(slot, "block still unavailable after retries");
    Fetched::Unavailable
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
