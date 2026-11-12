//! One confirmer per [`ChainClient`](crate::ChainClient): every transaction
//! waiting for its commitment is a pending entry, and one task polls them
//! all, so RPC cost per tick does not grow with the number in flight:
//!
//! - one `getBlockHeight` per commitment level in use, for expiry;
//! - one `getSignatureStatuses` per [`MAX_STATUS_BATCH`] signatures.
//!
//! The height is read before the statuses, so a status read in the tick
//! that sees the blockhash expire is the final check: nothing found then
//! means the transaction can no longer land.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use solana_commitment_config::CommitmentConfig;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_signature::Signature;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, sleep};
use tracing::debug;

/// `getSignatureStatuses` accepts at most this many signatures.
pub(crate) const MAX_STATUS_BATCH: usize = 256;
/// A blockhash is valid for this many blocks after the one it names.
const BLOCKHASH_VALIDITY_BLOCKS: u64 = 150;
/// Without the blockhash's `last_valid_block_height`, expiry is assumed
/// this many blocks later than the validity window counted from the first
/// height read: the blockhash is older than that, and the node that issued
/// it may be ahead of ours. Late is safe; early is a false `Expired`.
const UNKNOWN_EXPIRY_MARGIN_BLOCKS: u64 = 32;

/// What to watch for: one sent transaction.
pub(crate) struct Watch {
    pub signature: Signature,
    pub commitment: CommitmentConfig,
    /// `None`: estimated from the first block height read (see
    /// [`UNKNOWN_EXPIRY_MARGIN_BLOCKS`]).
    pub last_valid_block_height: Option<u64>,
    /// Hard cap, whatever the blockhash says.
    pub timeout: Duration,
    /// Consecutive failed status reads before giving up as ambiguous.
    pub max_status_errors: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// At the watched commitment, succeeded.
    Landed {
        slot: u64,
    },
    /// At the watched commitment, failed on chain.
    Failed {
        slot: u64,
        error: String,
    },
    /// The blockhash expired and the final status read found nothing.
    Expired,
    Ambiguous(String),
}

struct Pending {
    watch: Watch,
    deadline: Instant,
    errors: u32,
    reply: oneshot::Sender<Verdict>,
}

/// A status as the confirmer needs it.
pub(crate) struct Seen {
    pub slot: u64,
    pub error: Option<String>,
    /// Whether the status satisfies the watched commitment.
    pub at_commitment: bool,
}

impl Pending {
    fn on_status(
        &mut self,
        status: Option<Seen>,
        height: Option<u64>,
        now: Instant,
    ) -> Option<Verdict> {
        self.errors = 0;
        match status {
            Some(Seen {
                slot,
                error,
                at_commitment: true,
            }) => Some(match error {
                Some(error) => Verdict::Failed { slot, error },
                None => Verdict::Landed { slot },
            }),
            // Landed below the commitment: it cannot expire now, but its
            // fork can still be dropped (then it is `None` next tick).
            Some(_) => self.past_deadline(now),
            None => {
                if let Some(height) = height {
                    let last_valid = *self.watch.last_valid_block_height.get_or_insert(
                        height + BLOCKHASH_VALIDITY_BLOCKS + UNKNOWN_EXPIRY_MARGIN_BLOCKS,
                    );
                    if height > last_valid {
                        return Some(Verdict::Expired);
                    }
                }
                self.past_deadline(now)
            }
        }
    }

    fn on_error(&mut self, error: &str, now: Instant) -> Option<Verdict> {
        self.errors += 1;
        if self.errors >= self.watch.max_status_errors.max(1) {
            return Some(Verdict::Ambiguous(error.to_owned()));
        }
        self.past_deadline(now)
    }

    fn past_deadline(&self, now: Instant) -> Option<Verdict> {
        (now >= self.deadline)
            .then(|| Verdict::Ambiguous(format!("no confirmation within {:?}", self.watch.timeout)))
    }
}

pub(crate) struct Confirmer {
    rpc: Arc<RpcClient>,
    interval: Duration,
    /// Feeds the polling task; replaced when the task is gone (its runtime
    /// shut down), so a client outlives the runtime that first used it.
    queue: Mutex<Option<mpsc::UnboundedSender<Pending>>>,
}

impl Confirmer {
    pub fn new(rpc: Arc<RpcClient>, interval: Duration) -> Self {
        Self {
            rpc,
            interval,
            queue: Mutex::new(None),
        }
    }

    /// Start watching; the receiver gets exactly one verdict (or is closed
    /// if the polling task dies).
    pub fn watch(&self, watch: Watch) -> oneshot::Receiver<Verdict> {
        let (reply, receiver) = oneshot::channel();
        let mut pending = Pending {
            deadline: Instant::now() + watch.timeout,
            watch,
            errors: 0,
            reply,
        };
        let mut queue = self.queue.lock().expect("confirmer queue lock");
        if let Some(sender) = queue.as_ref() {
            match sender.send(pending) {
                Ok(()) => return receiver,
                Err(mpsc::error::SendError(returned)) => pending = returned,
            }
        }
        let (sender, entries) = mpsc::unbounded_channel();
        sender
            .send(pending)
            .unwrap_or_else(|_| unreachable!("receiver is alive"));
        tokio::spawn(run(entries, Arc::clone(&self.rpc), self.interval));
        *queue = Some(sender);
        receiver
    }
}

async fn run(
    mut entries: mpsc::UnboundedReceiver<Pending>,
    rpc: Arc<RpcClient>,
    interval: Duration,
) {
    let mut pending: Vec<Pending> = Vec::new();
    loop {
        if pending.is_empty() {
            // Idle until the next registration; exit with the last client.
            let Some(first) = entries.recv().await else {
                return;
            };
            pending.push(first);
        }
        sleep(interval).await;
        while let Ok(next) = entries.try_recv() {
            pending.push(next);
        }
        // Callers that stopped waiting (a timeout around the send).
        pending.retain(|entry| !entry.reply.is_closed());
        pending = tick(&rpc, pending).await;
    }
}

/// One poll of every pending entry; returns those still pending.
async fn tick(rpc: &RpcClient, mut pending: Vec<Pending>) -> Vec<Pending> {
    let mut heights: Vec<(CommitmentConfig, Option<u64>)> = Vec::new();
    for entry in &pending {
        let commitment = entry.watch.commitment;
        if heights.iter().all(|(known, _)| *known != commitment) {
            let height = rpc
                .get_block_height_with_commitment(commitment)
                .await
                .inspect_err(|error| debug!(%error, "block height read failed"))
                .ok();
            heights.push((commitment, height));
        }
    }
    let height = |commitment: CommitmentConfig| {
        heights
            .iter()
            .find(|(known, _)| *known == commitment)
            .and_then(|(_, height)| *height)
    };

    let mut verdicts: Vec<Option<Verdict>> = vec![None; pending.len()];
    for start in (0..pending.len()).step_by(MAX_STATUS_BATCH) {
        let end = (start + MAX_STATUS_BATCH).min(pending.len());
        let signatures: Vec<Signature> = pending[start..end]
            .iter()
            .map(|entry| entry.watch.signature)
            .collect();
        let response = rpc.get_signature_statuses(&signatures).await;
        let now = Instant::now();
        match response {
            Ok(response) => {
                let mut statuses = response.value.into_iter();
                for (entry, verdict) in pending[start..end]
                    .iter_mut()
                    .zip(&mut verdicts[start..end])
                {
                    let commitment = entry.watch.commitment;
                    let status = statuses.next().flatten().map(|status| Seen {
                        slot: status.slot,
                        at_commitment: status.satisfies_commitment(commitment),
                        error: status.err.map(|error| format!("{error:?}")),
                    });
                    *verdict = entry.on_status(status, height(commitment), now);
                }
            }
            Err(error) => {
                let error = error.to_string();
                debug!(%error, signatures = signatures.len(), "status read failed");
                for (entry, verdict) in pending[start..end]
                    .iter_mut()
                    .zip(&mut verdicts[start..end])
                {
                    *verdict = entry.on_error(&error, now);
                }
            }
        }
    }

    pending
        .into_iter()
        .zip(verdicts)
        .filter_map(|(entry, verdict)| match verdict {
            Some(verdict) => {
                // The caller may have stopped waiting meanwhile.
                let _ = entry.reply.send(verdict);
                None
            }
            None => Some(entry),
        })
        .collect()
}
