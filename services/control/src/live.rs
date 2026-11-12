//! The live feed: pumps that follow chain-watch and the process table, a
//! broadcast to every websocket client, and the buffers a new client's
//! snapshot is built from.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chain_watch_client::{Event, SlotRecord};
use control_client::{Live, Source, Status};
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::App;

/// Slots kept for a new client: 10 min at 200 ms.
const SLOT_BUFFER: usize = 3_000;
/// Slots fetched per chain-watch call.
const SLOT_BATCH: u32 = 2_000;

pub struct Hub {
    tx: broadcast::Sender<Live>,
    slots: Mutex<VecDeque<SlotRecord>>,
    events: Mutex<Vec<(Source, Event)>>,
}

impl Hub {
    pub fn new() -> Self {
        Self {
            tx: broadcast::channel(4_096).0,
            slots: Mutex::new(VecDeque::new()),
            events: Mutex::new(Vec::new()),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Live> {
        self.tx.subscribe()
    }

    pub fn publish(&self, message: Live) {
        // No receivers is fine: nobody is watching.
        let _ = self.tx.send(message);
    }

    pub fn push_event(&self, source: Source, event: Event) {
        if let Ok(mut events) = self.events.lock() {
            events.push((source, event.clone()));
        }
        self.publish(Live::Event { source, event });
    }

    pub fn snapshot(&self, status: Status) -> Live {
        Live::Snapshot {
            status,
            slots: self
                .slots
                .lock()
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default(),
            events: self.events.lock().map(|e| e.clone()).unwrap_or_default(),
        }
    }

    fn push_slots(&self, slots: Vec<SlotRecord>) {
        if slots.is_empty() {
            return;
        }
        if let Ok(mut buffer) = self.slots.lock() {
            buffer.extend(slots.iter().cloned());
            while buffer.len() > SLOT_BUFFER {
                buffer.pop_front();
            }
        }
        self.publish(Live::Slots { slots });
    }

    fn reset_chain(&self, run_id: &str) {
        if let Ok(mut buffer) = self.slots.lock() {
            buffer.clear();
        }
        if let Ok(mut events) = self.events.lock() {
            events.retain(|(source, _)| *source == Source::Control);
        }
        self.publish(Live::Reset {
            run_id: run_id.to_owned(),
        });
    }
}

/// Follow chain-watch: health, new slots, new events. A new run id (it was
/// restarted) resets the buffers.
pub async fn pump_chain(app: Arc<App>) {
    let mut run_id: Option<String> = None;
    let mut next_slot = 0_u64;
    let mut last_event = 0_i64;
    let mut ticker = tokio::time::interval(Duration::from_millis(500));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut reachable = true;
    loop {
        ticker.tick().await;
        let health = match app.chain.health().await {
            Ok(health) => health,
            Err(error) => {
                // Logged on the transition only: chain-watch may be down for a while.
                if reachable {
                    warn!(error = %error, "chain-watch health failed");
                    reachable = false;
                }
                app.set_chain_health(None);
                continue;
            }
        };
        if !reachable {
            info!("chain-watch reachable again");
            reachable = true;
        }
        if run_id.as_deref() != Some(health.run_id.as_str()) {
            if run_id.is_some() {
                info!(run_id = %health.run_id, "chain-watch run changed");
            }
            app.hub.reset_chain(&health.run_id);
            run_id = Some(health.run_id.clone());
            next_slot = health.cursor_slot.saturating_sub(SLOT_BUFFER as u64);
            last_event = 0;
        }
        app.set_chain_health(Some(health.clone()));
        let pulled = pull_slots(&mut next_slot, health.cursor_slot, |from, limit| {
            app.chain.slots_from(from, limit)
        })
        .await;
        match pulled {
            Ok(slots) => app.hub.push_slots(slots),
            Err(error) => warn!(error = %error, "reading slots from chain-watch failed"),
        }
        match app.chain.events(last_event).await {
            Ok(events) => {
                for event in events {
                    last_event = last_event.max(event.id);
                    app.hub.push_event(Source::Chain, event);
                }
            }
            Err(error) => warn!(error = %error, "reading events from chain-watch failed"),
        }
    }
}

/// One read of the slots chain-watch recorded at or after `next_slot`
/// (`fetch(from, limit)` is [`ChainWatchClient::slots_from`]), advancing
/// `next_slot` past them. Reading "from, limit" rather than a fixed window
/// steps over holes: the start of a run (rows begin at chain-watch's first
/// slot, after `next_slot`) and gaps chain-watch dropped, of any width.
///
/// [`ChainWatchClient::slots_from`]: chain_watch_client::ChainWatchClient::slots_from
async fn pull_slots<F, Fut, E>(
    next_slot: &mut u64,
    cursor_slot: u64,
    fetch: F,
) -> Result<Vec<SlotRecord>, E>
where
    F: FnOnce(u64, u32) -> Fut,
    Fut: Future<Output = Result<Vec<SlotRecord>, E>>,
{
    // The cursor is the next slot chain-watch will record.
    if cursor_slot <= *next_slot {
        return Ok(Vec::new());
    }
    let slots = fetch(*next_slot, SLOT_BATCH).await?;
    *next_slot = next_after(*next_slot, &slots);
    Ok(slots)
}

/// The next slot to ask for after a `/slots` reply. chain-watch commits in
/// batches behind its cursor, so advance past what was returned, not what was
/// asked for: otherwise every not-yet-committed slot is lost to the feed.
fn next_after(next_slot: u64, returned: &[SlotRecord]) -> u64 {
    returned.last().map_or(next_slot, |last| last.slot + 1)
}

/// Publish the status whenever it changes (checked every second).
pub async fn pump_status(app: Arc<App>) {
    let mut last: Option<Status> = None;
    loop {
        let status = app.status().await;
        if last.as_ref() != Some(&status) {
            app.hub.publish(Live::Status {
                status: status.clone(),
            });
            last = Some(status);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
mod tests {
    use chain_watch_client::SlotStatus;

    use super::*;

    fn slot(slot: u64) -> SlotRecord {
        SlotRecord {
            slot,
            leader: None,
            leader_client: None,
            leader_version: None,
            status: SlotStatus::Produced,
            block_time: None,
            block_height: None,
            tx_total: 0,
            tx_vote: 0,
            tx_failed: 0,
            tx_ours: 0,
            tx_ours_failed: 0,
            cu_consumed: None,
            cost_units: None,
            fees_lamports: 0,
            prio_p50: None,
            prio_p90: None,
            prio_max: None,
            observed_at_us: 0,
        }
    }

    #[test]
    fn advances_past_what_was_returned_only() {
        // Asked for 100..=110, only 100..=104 committed yet.
        let returned: Vec<_> = (100..=104).map(slot).collect();
        assert_eq!(next_after(100, &returned), 105);
        // Nothing committed yet: ask again from the same slot.
        assert_eq!(next_after(105, &[]), 105);
    }

    /// chain-watch's store as `/slots?from=&limit=` reads it.
    fn store(rows: &[u64]) -> impl Fn(u64, u32) -> std::future::Ready<Result<Vec<SlotRecord>, ()>> {
        let rows = rows.to_vec();
        move |from, limit| {
            std::future::ready(Ok(rows
                .iter()
                .filter(|s| **s >= from)
                .take(limit as usize)
                .map(|s| slot(*s))
                .collect()))
        }
    }

    /// Pull until caught up with `cursor`; every slot published, in order.
    async fn follow(next_slot: &mut u64, cursor: u64, rows: &[u64]) -> Vec<u64> {
        let fetch = store(rows);
        let mut seen = Vec::new();
        for _ in 0..100 {
            let slots = pull_slots(next_slot, cursor, &fetch).await.expect("fake");
            if slots.is_empty() {
                break;
            }
            seen.extend(slots.iter().map(|s| s.slot));
        }
        seen
    }

    #[tokio::test]
    async fn a_new_run_on_a_high_slot_chain_is_followed() {
        // chain-watch started at 300_000_000 and has recorded 5 slots;
        // control starts SLOT_BUFFER before its cursor, wholly before them.
        let start = 300_000_000;
        let rows: Vec<u64> = (start..start + 5).collect();
        let cursor = start + 5;
        let mut next_slot = cursor - SLOT_BUFFER as u64;
        assert_eq!(follow(&mut next_slot, cursor, &rows).await, rows);
        assert_eq!(next_slot, cursor);
        // Caught up: no call until the cursor moves.
        let none = pull_slots(&mut next_slot, cursor, |_, _| async {
            Err::<Vec<SlotRecord>, _>("must not be called")
        })
        .await;
        assert_eq!(none.map(|s| s.len()), Ok(0));
    }

    #[tokio::test]
    async fn a_gap_wider_than_the_batch_is_stepped_over() {
        let hole = u64::from(SLOT_BATCH) + 1_001;
        let rows: Vec<u64> = (1_000..1_100).chain(1_100 + hole..1_200 + hole).collect();
        let mut next_slot = 1_000;
        let seen = follow(&mut next_slot, 1_200 + hole, &rows).await;
        assert_eq!(seen, rows);
        assert_eq!(next_slot, 1_200 + hole);
    }
}
