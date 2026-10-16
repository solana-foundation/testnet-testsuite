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
const SLOT_BATCH: u64 = 2_000;

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
        // The cursor is the next slot chain-watch will record.
        let recorded = health.cursor_slot.saturating_sub(1);
        if health.cursor_slot > 0 && recorded >= next_slot {
            let to = recorded.min(next_slot + SLOT_BATCH - 1);
            match app.chain.slots(next_slot, to).await {
                Ok(slots) => {
                    next_slot = next_after(next_slot, &slots);
                    app.hub.push_slots(slots);
                }
                Err(error) => warn!(error = %error, "reading slots from chain-watch failed"),
            }
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
}
