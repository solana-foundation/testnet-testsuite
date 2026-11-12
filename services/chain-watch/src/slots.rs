//! The slot clock: `slotSubscribe` for the processed tip and slot pacing,
//! polled `confirmed` / `finalized` tips, epoch progress and performance
//! samples.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chain_watch_client::kind;
use futures::{SinkExt, StreamExt};
use metrics::{counter, gauge, histogram};
use mtm_common::time::now_us;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

use crate::rpc::redact;
use crate::state::Shared;

/// Follow `slotSubscribe`, reconnecting forever.
pub async fn subscribe(shared: Arc<Shared>, ws_url: String) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        match follow(&shared, &ws_url).await {
            Ok(()) => warn!("slot subscription closed; reconnecting"),
            Err(error) => warn!(
                error = %redact(&format!("{error:#}"), &ws_url),
                "slot subscription failed; reconnecting"
            ),
        }
        counter!("chain_ws_reconnects_total").increment(1);
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

async fn follow(shared: &Shared, ws_url: &str) -> Result<()> {
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .context("connecting")?;
    ws.send(Message::text(
        json!({ "jsonrpc": "2.0", "id": 1, "method": "slotSubscribe" }).to_string(),
    ))
    .await?;
    let mut last: Option<(u64, Instant)> = None;
    // A healthy cluster produces a slot every few hundred ms.
    while let Some(message) = tokio::time::timeout(Duration::from_secs(15), ws.next())
        .await
        .context("no slot notification for 15 s")?
    {
        let text = match message? {
            Message::Text(text) => text,
            Message::Ping(payload) => {
                ws.send(Message::Pong(payload)).await?;
                continue;
            }
            Message::Close(_) => return Ok(()),
            _ => continue,
        };
        let value: Value = serde_json::from_str(&text)?;
        let result = &value["params"]["result"];
        let Some(slot) = result["slot"].as_u64() else {
            continue;
        };
        shared.processed.fetch_max(slot, Ordering::Relaxed);
        gauge!("chain_slot", "commitment" => "processed").set(slot as f64);
        if let Some(root) = result["root"].as_u64() {
            gauge!("chain_slot", "commitment" => "root").set(root as f64);
        }
        if let Some((previous, at)) = last
            && slot > previous
        {
            // Per slot, so a burst after a stall doesn't read as fast slots.
            histogram!("chain_slot_interval_seconds")
                .record(at.elapsed().as_secs_f64() / (slot - previous) as f64);
        }
        last = Some((slot, Instant::now()));
    }
    Ok(())
}

/// Poll the confirmed and finalized tips (and processed, when the
/// subscription has gone quiet).
pub async fn poll_commitments(shared: Arc<Shared>) {
    let mut ticker = tokio::time::interval(Duration::from_millis(shared.cfg.commitment_poll_ms));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_processed = (0, Instant::now());
    let mut tick = 0_u64;
    loop {
        ticker.tick().await;
        tick += 1;
        // Finalized trails confirmed by ~32 slots; every fifth tick is enough.
        let commitments: &[&str] = if tick % 5 == 1 {
            &["confirmed", "finalized"]
        } else {
            &["confirmed"]
        };
        for &commitment in commitments {
            match shared
                .rpc
                .call("getSlot", json!([{ "commitment": commitment }]))
                .await
            {
                Ok(slot) => {
                    let Some(slot) = slot.as_u64() else { continue };
                    gauge!("chain_slot", "commitment" => commitment).set(slot as f64);
                    if commitment == "confirmed" {
                        shared.confirmed.fetch_max(slot, Ordering::Relaxed);
                    }
                }
                Err(error) => warn!(commitment, error = %format!("{error:#}"), "getSlot failed"),
            }
        }
        let processed = shared.processed.load(Ordering::Relaxed);
        if processed != last_processed.0 {
            last_processed = (processed, Instant::now());
        } else if last_processed.1.elapsed() > Duration::from_secs(5)
            && let Ok(slot) = shared
                .rpc
                .call("getSlot", json!([{ "commitment": "processed" }]))
                .await
            && let Some(slot) = slot.as_u64()
        {
            shared.processed.fetch_max(slot, Ordering::Relaxed);
            gauge!("chain_slot", "commitment" => "processed").set(slot as f64);
        }
    }
}

/// Epoch progress every 10 s; an `epoch` event and a feature re-poll at
/// each boundary.
pub async fn poll_epoch(shared: Arc<Shared>) {
    let mut current: Option<u64> = None;
    loop {
        match shared
            .rpc
            .call("getEpochInfo", json!([{ "commitment": "confirmed" }]))
            .await
        {
            Ok(info) => {
                let epoch = info["epoch"].as_u64().unwrap_or_default();
                let index = info["slotIndex"].as_u64().unwrap_or_default();
                let length = info["slotsInEpoch"].as_u64().unwrap_or(1).max(1);
                let absolute = info["absoluteSlot"].as_u64().unwrap_or_default();
                gauge!("chain_epoch").set(epoch as f64);
                gauge!("chain_epoch_progress").set(index as f64 / length as f64);
                gauge!("chain_slots_in_epoch").set(length as f64);
                if let Some(count) = info["transactionCount"].as_u64() {
                    gauge!("chain_transaction_count").set(count as f64);
                }
                if current.is_some_and(|previous| previous != epoch) {
                    let first = absolute.saturating_sub(index);
                    info!(epoch, first_slot = first, "epoch boundary");
                    let detail =
                        json!({ "epoch": epoch, "first_slot": first, "slots_in_epoch": length });
                    if let Err(error) = shared
                        .store
                        .event(
                            now_us(),
                            Some(first),
                            kind::EPOCH,
                            &epoch.to_string(),
                            detail,
                        )
                        .await
                    {
                        warn!(error = %format!("{error:#}"), "recording epoch event failed");
                    }
                    shared.epoch_changed.notify_waiters();
                }
                current = Some(epoch);
            }
            Err(error) => warn!(error = %format!("{error:#}"), "getEpochInfo failed"),
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

/// Cluster-wide TPS from the node's performance samples (60 s windows):
/// the cross-check for the per-block counts, and the only TPS source when
/// blocks are not fetched in full.
pub async fn poll_performance(shared: Arc<Shared>) {
    loop {
        match shared
            .rpc
            .call("getRecentPerformanceSamples", json!([1]))
            .await
        {
            Ok(samples) => {
                if let Some(sample) = samples.get(0) {
                    let period = sample["samplePeriodSecs"].as_f64().unwrap_or(60.0).max(1.0);
                    let total = sample["numTransactions"].as_f64().unwrap_or_default();
                    let non_vote = sample["numNonVoteTransactions"]
                        .as_f64()
                        .unwrap_or_default();
                    let slots = sample["numSlots"].as_f64().unwrap_or_default();
                    gauge!("chain_perf_tps", "kind" => "total").set(total / period);
                    gauge!("chain_perf_tps", "kind" => "non_vote").set(non_vote / period);
                    if slots > 0.0 {
                        let seconds = period / slots;
                        gauge!("chain_perf_slot_seconds").set(seconds);
                        if let Ok(mut s) = shared.slot_seconds.write() {
                            *s = seconds;
                        }
                    }
                }
            }
            Err(error) => {
                warn!(error = %format!("{error:#}"), "getRecentPerformanceSamples failed")
            }
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}
