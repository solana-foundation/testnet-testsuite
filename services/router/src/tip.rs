//! The chain tip (slot and latest blockhash), refreshed in the background so
//! quotes never wait on RPC for it.

use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use futures::future::BoxFuture;
use mtm_chain::ChainClient;
use solana_hash::Hash;
use tokio::sync::watch;
use tokio::time::Instant;
use tracing::{info, warn};

#[derive(Debug, Clone, Copy)]
pub struct ChainTip {
    pub slot: u64,
    pub blockhash: Hash,
    pub fetched_at: Instant,
}

/// The latest tip the refresher fetched, read without I/O.
#[derive(Clone)]
pub struct TipCell {
    rx: watch::Receiver<Option<ChainTip>>,
    max_age: Duration,
}

impl TipCell {
    /// A cell fed by `tx`; tests and the refresher share this.
    pub fn new(max_age: Duration) -> (watch::Sender<Option<ChainTip>>, Self) {
        let (tx, rx) = watch::channel(None);
        (tx, Self { rx, max_age })
    }

    /// The cached tip, or why it cannot be used (none yet, or older than
    /// `max_age` because the refresher keeps failing).
    pub fn current(&self) -> Result<ChainTip, String> {
        let Some(tip) = *self.rx.borrow() else {
            return Err("no slot/blockhash fetched yet".to_owned());
        };
        let age = tip.fetched_at.elapsed();
        if age > self.max_age {
            return Err(format!(
                "cached slot/blockhash is {} ms old (max {} ms)",
                age.as_millis(),
                self.max_age.as_millis()
            ));
        }
        Ok(tip)
    }
}

/// Fetches the slot and latest blockhash once. Production fetches go
/// through [`chain_fetch`]; tests inject fakes.
pub type TipFetch = Arc<dyn Fn() -> BoxFuture<'static, Result<(u64, Hash), String>> + Send + Sync>;

/// The production fetch: `getSlot` and `getLatestBlockhash` over `chain`,
/// concurrently. Give it a client of its own (not the snapshot loaders'),
/// so loads cannot queue ahead of it.
pub fn chain_fetch(chain: ChainClient) -> TipFetch {
    Arc::new(move || {
        let chain = chain.clone();
        async move {
            let (slot, blockhash) =
                tokio::join!(chain.current_slot(), chain.rpc().get_latest_blockhash());
            match (slot, blockhash) {
                (Ok(slot), Ok(blockhash)) => Ok((slot, blockhash)),
                (Err(error), _) => Err(format!("{error:#}")),
                (_, Err(error)) => Err(format!("get_latest_blockhash failed: {error}")),
            }
        }
        .boxed()
    })
}

/// Refresh the tip every `interval`. Each refresh is abandoned after
/// `timeout`: the HTTP timeout covers one attempt only, and the RPC client
/// sleeps between its 429 retries (up to `Retry-After`, ~2 min), so a single
/// rate-limited call could otherwise stall the refresher until the tip
/// expires. Failures keep the previous tip, which [`TipCell::current`]
/// stops handing out once it is too old.
pub fn spawn_refresher(
    fetch: TipFetch,
    interval: Duration,
    timeout: Duration,
    max_age: Duration,
) -> TipCell {
    let (tx, cell) = TipCell::new(max_age);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut failing = false;
        loop {
            ticker.tick().await;
            let fetched = match tokio::time::timeout(timeout, fetch()).await {
                Ok(fetched) => fetched,
                Err(_) => Err(format!("chain tip refresh timed out after {timeout:?}")),
            };
            match fetched {
                Ok((slot, blockhash)) => {
                    if failing {
                        info!(slot, "chain tip refresh recovered");
                        failing = false;
                    }
                    tx.send_replace(Some(ChainTip {
                        slot,
                        blockhash,
                        fetched_at: Instant::now(),
                    }));
                }
                Err(error) => {
                    metrics::counter!("router_chain_tip_refresh_failures_total").increment(1);
                    // Logged on the transition only: this runs every
                    // `interval` while the RPC is down.
                    if !failing {
                        warn!(error = %error, "chain tip refresh failed");
                        failing = true;
                    }
                }
            }
        }
    });
    cell
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// R2-ROUTER-2: a fetch stuck far past the tip's max age (a 429 retry
    /// sleep inside the RPC client) is abandoned at `timeout`; the next tick
    /// refreshes the tip.
    #[tokio::test(start_paused = true)]
    async fn a_stuck_refresh_does_not_stall_the_tip() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let fetch: TipFetch = Arc::new(move || {
            let call = counter.fetch_add(1, Ordering::SeqCst);
            async move {
                if call == 0 {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
                Ok((call as u64, Hash::new_from_array([1; 32])))
            }
            .boxed()
        });
        let cell = spawn_refresher(
            fetch,
            Duration::from_millis(400),
            Duration::from_secs(1),
            Duration::from_secs(5),
        );
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        let tip = cell.current().expect("refreshed after the stuck fetch");
        assert!(tip.slot >= 1, "fetched after the stuck call: {}", tip.slot);
    }

    #[tokio::test(start_paused = true)]
    async fn tip_is_refused_once_too_old() {
        let (tx, cell) = TipCell::new(Duration::from_secs(5));
        assert!(cell.current().is_err(), "nothing fetched yet");
        tx.send_replace(Some(ChainTip {
            slot: 7,
            blockhash: Hash::new_from_array([9; 32]),
            fetched_at: Instant::now(),
        }));
        assert_eq!(cell.current().expect("fresh").slot, 7);
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(cell.current().is_ok(), "exactly max_age is still usable");
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert!(cell.current().is_err(), "older than max_age");
    }
}
