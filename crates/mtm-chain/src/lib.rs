//! Off-chain Solana toolkit shared by every service that touches the chain.

mod confirm;
pub mod keys;
#[cfg(any(test, feature = "test-util"))]
pub mod mock;
pub mod tag;
#[cfg(test)]
mod tests;
mod transport;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use solana_commitment_config::CommitmentConfig;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_instruction::Instruction;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client::rpc_client::{RpcClientConfig, SerializableTransaction};
#[cfg(any(test, feature = "test-util"))]
use solana_rpc_client::rpc_sender::RpcSender;
use solana_rpc_client_api::client_error::{Error as ClientError, ErrorKind as RpcErrorKind};
use solana_rpc_client_api::config::{RpcSendTransactionConfig, RpcSimulateTransactionConfig};
use solana_rpc_client_api::request::{RpcError, RpcResponseErrorData};
use solana_signature::Signature;
use solana_signer::Signer;
use solana_transaction::Transaction;
use solana_transaction::versioned::VersionedTransaction;
use thiserror::Error;
use tokio::time::{Instant, sleep, sleep_until};
use tracing::{debug, warn};

use crate::confirm::{Confirmer, Verdict, Watch};
use crate::transport::Transport;

/// Controls transaction construction, submission retries, and confirmation.
#[derive(Clone, Copy, Debug)]
pub struct SubmissionConfig {
    /// The simulation's limit, and the ceiling of the sent one.
    pub compute_unit_limit: u32,
    /// Send with the limit the simulation needs plus headroom (see
    /// [`sized_compute_unit_limit`]) instead of `compute_unit_limit`:
    /// leaders pack blocks by requested CU, and the priority fee is paid
    /// per requested unit.
    pub size_compute_units: bool,
    pub compute_unit_price_micro_lamports: u64,
    /// Blockhash, simulation, preflight and confirmation commitment.
    pub commitment: CommitmentConfig,
    /// Send attempts while the response is lost (same signed bytes).
    pub max_send_attempts: usize,
    /// Hard cap on the wait. Expiry of the blockhash normally decides first.
    pub confirmation_timeout: Duration,
    /// Resend the same signed bytes this often until the transaction lands
    /// or its blockhash expires (`None`: send once).
    pub rebroadcast_interval: Option<Duration>,
    /// Give up as ambiguous after this many consecutive failed status reads,
    /// once they also span [`Self::status_error_window`].
    pub max_status_errors: u32,
    /// How long status reads must keep failing before giving up as
    /// ambiguous: an RPC restart or a load-balancer blip is not final.
    pub status_error_window: Duration,
}

/// Default for `status_error_window`.
pub const DEFAULT_STATUS_ERROR_WINDOW: Duration = Duration::from_secs(15);

impl Default for SubmissionConfig {
    fn default() -> Self {
        Self {
            compute_unit_limit: 1_400_000,
            size_compute_units: true,
            compute_unit_price_micro_lamports: 0,
            commitment: CommitmentConfig::confirmed(),
            max_send_attempts: 3,
            confirmation_timeout: Duration::from_secs(120),
            rebroadcast_interval: Some(Duration::from_secs(2)),
            max_status_errors: 5,
            status_error_window: DEFAULT_STATUS_ERROR_WINDOW,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmissionOutcome {
    pub signature: Signature,
    pub confirmation_slot: u64,
    pub simulated_compute_units: u64,
}

#[derive(Debug, Error)]
pub enum SubmissionError {
    #[error("failed to obtain a recent blockhash: {0}")]
    Blockhash(String),
    #[error("failed to sign transaction: {0}")]
    Signing(String),
    #[error("transaction simulation RPC failed: {0}")]
    SimulationRpc(String),
    #[error("transaction simulation failed: {error}")]
    Simulation { error: String, logs: Vec<String> },
    /// The RPC answered the send with an error: the transaction was not
    /// accepted.
    #[error("transaction submission failed: {0}")]
    Submission(String),
    /// The RPC answered the send with HTTP 429: the node did not take it.
    #[error("transaction send rate-limited by the RPC: {0}")]
    RateLimited(String),
    /// The send never reached the RPC (connection failed) or the RPC
    /// refused it with HTTP 503 or a 4xx status: not taken. Other 5xx
    /// (500, 502, 504, ...) can come from a gateway that forwarded the
    /// send, so they are resent and the status decides.
    #[error("RPC unavailable for the transaction send: {0}")]
    Unavailable(String),
    /// Failed on chain, seen at the requested commitment.
    #[error("transaction {signature} failed in slot {slot}: {error}")]
    Execution {
        signature: Signature,
        slot: u64,
        error: String,
    },
    #[error("transaction confirmation is ambiguous for {signature}: {reason}")]
    AmbiguousConfirmation {
        signature: Signature,
        reason: String,
    },
    /// The blockhash expired and a final status check found nothing: the
    /// transaction can no longer land.
    #[error("transaction {signature} expired without landing")]
    Expired { signature: Signature },
}

/// How [`ChainClient::send_versioned`] submits and waits.
#[derive(Clone, Copy, Debug)]
pub struct VersionedSendConfig {
    /// Skip the RPC's preflight simulation (when the caller simulated).
    pub skip_preflight: bool,
    /// Send attempts while the response is lost (same signed bytes).
    pub max_send_attempts: usize,
    /// Hard cap on the wait. Expiry of the blockhash normally decides first.
    pub confirmation_timeout: Duration,
    /// Give up as ambiguous after this many consecutive failed status reads,
    /// once they also span [`Self::status_error_window`].
    pub max_status_errors: u32,
    /// See [`SubmissionConfig::status_error_window`].
    pub status_error_window: Duration,
    /// The blockhash's `last_valid_block_height` when the caller has it;
    /// otherwise expiry is estimated conservatively from the first block
    /// height read after sending.
    pub last_valid_block_height: Option<u64>,
    /// Resend the same signed bytes this often until it lands or expires
    /// (`None`: the node's own retry queue only).
    pub rebroadcast_interval: Option<Duration>,
}

impl Default for VersionedSendConfig {
    fn default() -> Self {
        Self {
            skip_preflight: false,
            max_send_attempts: 3,
            confirmation_timeout: Duration::from_secs(90),
            max_status_errors: 5,
            status_error_window: DEFAULT_STATUS_ERROR_WINDOW,
            last_valid_block_height: None,
            rebroadcast_interval: None,
        }
    }
}

/// Per-client transport settings.
#[derive(Clone, Copy, Debug)]
pub struct ChainOptions {
    /// RPC requests in flight at once, over every clone of the client and
    /// its [`ChainClient::with_commitment`] views; the rest queue. Over
    /// HTTP this counts HTTP exchanges: no permit is held while the HTTP
    /// sender sleeps before retrying an HTTP 429. The confirmer has its
    /// own few permits on top, so status polls never queue behind callers.
    pub max_in_flight: usize,
    /// How often the shared confirmer polls pending transactions.
    pub confirm_interval: Duration,
    /// Per-request HTTP timeout (the HTTP client's 30 s default when `None`).
    pub request_timeout: Option<Duration>,
}

/// Default for [`ChainOptions::max_in_flight`].
pub const DEFAULT_MAX_IN_FLIGHT: usize = 128;

impl Default for ChainOptions {
    fn default() -> Self {
        Self {
            max_in_flight: DEFAULT_MAX_IN_FLIGHT,
            confirm_interval: Duration::from_millis(400),
            request_timeout: None,
        }
    }
}

#[derive(Clone)]
pub struct ChainClient {
    rpc: Arc<RpcClient>,
    /// The callers' transport, shared with [`Self::with_commitment`] views.
    transport: Transport,
    confirmer: Arc<Confirmer>,
}

impl ChainClient {
    pub fn new(http_url: impl Into<String>, commitment: &str) -> anyhow::Result<Self> {
        Self::with_options(http_url, commitment, ChainOptions::default())
    }

    pub fn with_options(
        http_url: impl Into<String>,
        commitment: &str,
        options: ChainOptions,
    ) -> anyhow::Result<Self> {
        let commitment = parse_commitment(commitment)?;
        let (callers, confirmer) = Transport::http(
            http_url.into(),
            options.request_timeout,
            options.max_in_flight,
        )
        .context("failed to build the RPC HTTP client")?;
        Ok(Self::assemble(callers, confirmer, commitment, options))
    }

    /// A client over any transport, e.g. [`mock::MockRpc`] in tests (needs
    /// the `test-util` feature outside this crate). Callers share
    /// `options.max_in_flight` permits per request; the confirmer has its
    /// own.
    #[cfg(any(test, feature = "test-util"))]
    pub fn from_sender(
        sender: impl RpcSender + Send + Sync + 'static,
        commitment: CommitmentConfig,
        options: ChainOptions,
    ) -> Self {
        let callers = Transport::limited(Arc::new(sender), options.max_in_flight);
        let confirmer = callers.sibling(transport::CONFIRMER_MAX_IN_FLIGHT);
        Self::assemble(callers, confirmer, commitment, options)
    }

    fn assemble(
        callers: Transport,
        confirmer: Transport,
        commitment: CommitmentConfig,
        options: ChainOptions,
    ) -> Self {
        let rpc = Arc::new(RpcClient::new_sender(
            callers.clone(),
            RpcClientConfig::with_commitment(commitment),
        ));
        let confirmer_rpc = Arc::new(RpcClient::new_sender(
            confirmer,
            RpcClientConfig::with_commitment(commitment),
        ));
        Self {
            confirmer: Arc::new(Confirmer::new(confirmer_rpc, options.confirm_interval)),
            rpc,
            transport: callers,
        }
    }

    /// The same client (connections, request limit and confirmer) with
    /// another default commitment for [`Self::rpc`] reads and
    /// [`Self::send_versioned`] confirmation; e.g. a searcher reading at
    /// `processed` while it confirms at `confirmed`.
    pub fn with_commitment(&self, commitment: CommitmentConfig) -> Self {
        Self {
            rpc: Arc::new(RpcClient::new_sender(
                self.transport.clone(),
                RpcClientConfig::with_commitment(commitment),
            )),
            transport: self.transport.clone(),
            confirmer: Arc::clone(&self.confirmer),
        }
    }

    /// Like [`ChainClient::new`], but every RPC request fails after `timeout`
    /// instead of the HTTP client's 30 s default.
    pub fn with_timeout(
        http_url: impl Into<String>,
        commitment: &str,
        timeout: Duration,
    ) -> anyhow::Result<Self> {
        Self::with_options(
            http_url,
            commitment,
            ChainOptions {
                request_timeout: Some(timeout),
                ..ChainOptions::default()
            },
        )
    }

    pub fn rpc(&self) -> &RpcClient {
        &self.rpc
    }

    /// Cheap connectivity check; returns the current slot.
    pub async fn current_slot(&self) -> anyhow::Result<u64> {
        self.rpc.get_slot().await.context("get_slot failed")
    }

    /// Build once, simulate, submit, and confirm a transaction at
    /// `config.commitment`.
    ///
    /// Every send (transport retries and rebroadcasts) reuses the exact
    /// signed bytes; the blockhash is never refreshed. The transaction is
    /// rebroadcast until it lands or its blockhash expires
    /// ([`SubmissionError::Expired`]). Counted in `chain_tx_*` under the
    /// current [`tag`].
    pub async fn submit(
        &self,
        instructions: Vec<Instruction>,
        payer: &dyn Signer,
        additional_signers: &[&dyn Signer],
        config: SubmissionConfig,
    ) -> Result<SubmissionOutcome, SubmissionError> {
        let started = Instant::now();
        tag::record_submitted(tag::Path::Submit);
        let result = self
            .submit_inner(instructions, payer, additional_signers, config)
            .await;
        tag::record_outcome(&result, started.elapsed());
        result
    }

    async fn submit_inner(
        &self,
        instructions: Vec<Instruction>,
        payer: &dyn Signer,
        additional_signers: &[&dyn Signer],
        config: SubmissionConfig,
    ) -> Result<SubmissionOutcome, SubmissionError> {
        let (blockhash, last_valid_block_height) = self
            .rpc
            .get_latest_blockhash_with_commitment(config.commitment)
            .await
            .map_err(|error| SubmissionError::Blockhash(error.to_string()))?;

        let mut signers = Vec::with_capacity(additional_signers.len() + 1);
        signers.push(payer);
        for signer in additional_signers {
            if signer.pubkey() != payer.pubkey()
                && !signers
                    .iter()
                    .any(|known| known.pubkey() == signer.pubkey())
            {
                signers.push(*signer);
            }
        }
        let build = |compute_unit_limit: u32| {
            let mut budget = vec![
                ComputeBudgetInstruction::set_compute_unit_limit(compute_unit_limit),
                ComputeBudgetInstruction::set_compute_unit_price(
                    config.compute_unit_price_micro_lamports,
                ),
            ];
            budget.extend(instructions.iter().cloned());
            let mut transaction = Transaction::new_with_payer(&budget, Some(&payer.pubkey()));
            transaction
                .try_sign(&signers, blockhash)
                .map_err(|error| SubmissionError::Signing(error.to_string()))?;
            Ok::<_, SubmissionError>(transaction)
        };
        // Simulated at the ceiling, then (by default) sent with a limit
        // sized to what the simulation used.
        let mut transaction = build(config.compute_unit_limit)?;

        let simulation = self
            .rpc
            .simulate_transaction_with_config(
                &transaction,
                RpcSimulateTransactionConfig {
                    commitment: Some(config.commitment),
                    ..RpcSimulateTransactionConfig::default()
                },
            )
            .await
            .map_err(|error| SubmissionError::SimulationRpc(error.to_string()))?;
        let simulated_compute_units = simulation.value.units_consumed.unwrap_or_default();
        if let Some(error) = simulation.value.err {
            return Err(SubmissionError::Simulation {
                error: format!("{error:?}"),
                logs: simulation.value.logs.unwrap_or_default(),
            });
        }
        if config.size_compute_units
            && let Some(used) = simulation.value.units_consumed
        {
            let limit = sized_compute_unit_limit(used, config.compute_unit_limit);
            if limit != config.compute_unit_limit {
                transaction = build(limit)?;
            }
        }
        let signature = transaction.signatures[0];

        let send = RpcSendTransactionConfig {
            skip_preflight: true,
            preflight_commitment: Some(config.commitment.commitment),
            max_retries: Some(0),
            ..RpcSendTransactionConfig::default()
        };
        let confirmation_slot = self
            .deliver(
                &transaction,
                signature,
                Delivery {
                    send,
                    max_send_attempts: config.max_send_attempts,
                    rebroadcast_interval: config.rebroadcast_interval,
                    watch: Watch {
                        signature,
                        commitment: config.commitment,
                        last_valid_block_height: Some(last_valid_block_height),
                        timeout: config.confirmation_timeout,
                        max_status_errors: config.max_status_errors,
                        status_error_window: config.status_error_window,
                    },
                },
            )
            .await?;
        Ok(SubmissionOutcome {
            signature,
            confirmation_slot,
            simulated_compute_units,
        })
    }

    /// Send an already signed transaction (legacy or versioned, e.g. one a
    /// router built) and wait for the client's commitment.
    ///
    /// Errors are typed for callers that classify outcomes: a preflight
    /// rejection is [`SubmissionError::Simulation`], an RPC rejection of
    /// the send [`SubmissionError::Submission`] (or, when the node did not
    /// take it at all, [`SubmissionError::RateLimited`] /
    /// [`SubmissionError::Unavailable`], with no status polling), an
    /// on-chain failure (at
    /// the commitment) [`SubmissionError::Execution`], expiry
    /// [`SubmissionError::Expired`], and an unknown status
    /// [`SubmissionError::AmbiguousConfirmation`]. A lost send response is
    /// not an error by itself: the status decides. Counted in `chain_tx_*`
    /// under the current [`tag`].
    pub async fn send_versioned(
        &self,
        transaction: &VersionedTransaction,
        config: VersionedSendConfig,
    ) -> Result<SubmissionOutcome, SubmissionError> {
        let started = Instant::now();
        tag::record_submitted(tag::Path::Versioned);
        let result = self.send_versioned_inner(transaction, config).await;
        tag::record_outcome(&result, started.elapsed());
        result
    }

    async fn send_versioned_inner(
        &self,
        transaction: &VersionedTransaction,
        config: VersionedSendConfig,
    ) -> Result<SubmissionOutcome, SubmissionError> {
        let commitment = self.rpc.commitment();
        let signature = *transaction
            .signatures
            .first()
            .ok_or_else(|| SubmissionError::Signing("transaction has no signatures".to_owned()))?;
        let send = RpcSendTransactionConfig {
            skip_preflight: config.skip_preflight,
            preflight_commitment: Some(commitment.commitment),
            ..RpcSendTransactionConfig::default()
        };
        let confirmation_slot = self
            .deliver(
                transaction,
                signature,
                Delivery {
                    send,
                    max_send_attempts: config.max_send_attempts,
                    rebroadcast_interval: config.rebroadcast_interval,
                    watch: Watch {
                        signature,
                        commitment,
                        last_valid_block_height: config.last_valid_block_height,
                        timeout: config.confirmation_timeout,
                        max_status_errors: config.max_status_errors,
                        status_error_window: config.status_error_window,
                    },
                },
            )
            .await?;
        Ok(SubmissionOutcome {
            signature,
            confirmation_slot,
            simulated_compute_units: 0,
        })
    }

    /// Send, then wait for the confirmer's verdict (rebroadcasting the
    /// same bytes meanwhile, when asked). Returns the landing slot.
    async fn deliver(
        &self,
        transaction: &(impl SerializableTransaction + Sync),
        signature: Signature,
        delivery: Delivery,
    ) -> Result<u64, SubmissionError> {
        let attempts = delivery.max_send_attempts.max(1);
        // Once any attempt may have reached the node, a later error does not
        // mean "not sent": the first copy can still land.
        let mut maybe_delivered = false;
        for attempt in 1..=attempts {
            let error = match self
                .rpc
                .send_transaction_with_config(transaction, delivery.send)
                .await
            {
                Ok(_) => break,
                Err(error) => error,
            };
            match classify_send_error(&error) {
                SendFailure::Ambiguous => {
                    maybe_delivered = true;
                    if attempt < attempts {
                        warn!(attempt, %signature, error = %error, "transaction transport retry");
                        tag::record_retry();
                        sleep(Duration::from_millis(200 * attempt as u64)).await;
                    } else {
                        debug!(%signature, error = %error, "send response lost; waiting for status");
                    }
                }
                _ if maybe_delivered => {
                    debug!(%signature, error = %error, "send failed after a lost response; waiting for status");
                    break;
                }
                SendFailure::Preflight { error, logs } => {
                    return Err(SubmissionError::Simulation { error, logs });
                }
                SendFailure::Rejected => {
                    return Err(SubmissionError::Submission(error.to_string()));
                }
                SendFailure::RateLimited => {
                    return Err(SubmissionError::RateLimited(error.to_string()));
                }
                SendFailure::Unavailable => {
                    return Err(SubmissionError::Unavailable(error.to_string()));
                }
            }
        }

        // The confirmer applies the cap after a status read; this one holds
        // when that read (or the whole tick) is stuck.
        let timeout = delivery.watch.timeout;
        let cap = sleep_until(Instant::now() + timeout + CAP_SLACK);
        tokio::pin!(cap);
        let mut verdict = self.confirmer.watch(delivery.watch);
        let resend = RpcSendTransactionConfig {
            skip_preflight: true,
            ..delivery.send
        };
        let mut interval = delivery.rebroadcast_interval;
        let verdict = loop {
            let wait = interval;
            // Raced against the verdict, so a slow resend never delays it.
            let rebroadcast = async move {
                let Some(wait) = wait else {
                    return std::future::pending().await;
                };
                sleep(wait).await;
                if let Err(error) = self
                    .rpc
                    .send_transaction_with_config(transaction, resend)
                    .await
                {
                    debug!(%signature, %error, "rebroadcast failed");
                }
            };
            tokio::select! {
                biased;
                verdict = &mut verdict => break verdict,
                () = &mut cap => {
                    break Ok(Verdict::Ambiguous(format!("no confirmation within {timeout:?}")));
                }
                () = rebroadcast => {
                    interval = interval.map(|interval| {
                        (interval * 2).min(MAX_REBROADCAST_INTERVAL.max(interval))
                    });
                }
            }
        };
        match verdict {
            Ok(Verdict::Landed { slot }) => Ok(slot),
            Ok(Verdict::Failed { slot, error }) => Err(SubmissionError::Execution {
                signature,
                slot,
                error,
            }),
            Ok(Verdict::Expired) => Err(SubmissionError::Expired { signature }),
            Ok(Verdict::Ambiguous(reason)) => {
                Err(SubmissionError::AmbiguousConfirmation { signature, reason })
            }
            Err(_) => Err(SubmissionError::AmbiguousConfirmation {
                signature,
                reason: "the confirmer stopped".to_owned(),
            }),
        }
    }

    // TODO: account subscription trait with pubsub-ws and yellowstone-geyser backends.
}

/// How long past the hard cap a caller still waits for the confirmer's
/// verdict before giving up on its own.
const CAP_SLACK: Duration = Duration::from_secs(2);

/// Rebroadcasts back off (doubling) up to this interval.
const MAX_REBROADCAST_INTERVAL: Duration = Duration::from_secs(8);

/// Smallest limit [`sized_compute_unit_limit`] requests.
pub const MIN_SIZED_COMPUTE_UNITS: u32 = 10_000;

/// The CU limit for a transaction whose simulation used `used`: 20% more
/// (at least [`MIN_SIZED_COMPUTE_UNITS`] more, for state that moves between
/// simulation and landing), capped at `ceiling`.
pub fn sized_compute_unit_limit(used: u64, ceiling: u32) -> u32 {
    let headroom = (used / 5).max(u64::from(MIN_SIZED_COMPUTE_UNITS));
    u32::try_from(used.saturating_add(headroom))
        .unwrap_or(u32::MAX)
        .min(ceiling)
}

/// How [`ChainClient::deliver`] sends and what it waits for.
struct Delivery {
    send: RpcSendTransactionConfig,
    max_send_attempts: usize,
    rebroadcast_interval: Option<Duration>,
    watch: Watch,
}

enum SendFailure {
    /// The node simulated it and the simulation failed.
    Preflight { error: String, logs: Vec<String> },
    /// The node answered with an error: not accepted.
    Rejected,
    /// HTTP 429: the request was refused before the node looked at it.
    RateLimited,
    /// The connection failed, HTTP 503, or a 4xx: not taken.
    Unavailable,
    /// The request may have reached the node.
    Ambiguous,
}

fn classify_send_error(error: &ClientError) -> SendFailure {
    match error.kind() {
        RpcErrorKind::RpcError(RpcError::RpcResponseError {
            data: RpcResponseErrorData::SendTransactionPreflightFailure(result),
            ..
        }) => SendFailure::Preflight {
            error: result
                .err
                .as_ref()
                .map_or_else(|| error.to_string(), |err| format!("{err:?}")),
            logs: result.logs.clone().unwrap_or_default(),
        },
        RpcErrorKind::Reqwest(error) if error.is_status() || error.is_connect() => {
            match error.status().map(|status| status.as_u16()) {
                Some(429) => SendFailure::RateLimited,
                // A gateway's 502/504 (or a 500, 52x) can follow a send
                // the node took.
                Some(status @ 500..=599) if status != 503 => SendFailure::Ambiguous,
                _ => SendFailure::Unavailable,
            }
        }
        // Timeouts, broken bodies and undecodable answers: the request may
        // have been read.
        RpcErrorKind::Io(_)
        | RpcErrorKind::Reqwest(_)
        | RpcErrorKind::Middleware(_)
        | RpcErrorKind::SerdeJson(_)
        // A mismatched or unparsable signature in the answer: the node had
        // the request.
        | RpcErrorKind::RpcError(RpcError::RpcRequestError(_) | RpcError::ParseError(_)) => {
            SendFailure::Ambiguous
        }
        _ => SendFailure::Rejected,
    }
}

/// Whether the RPC refused a request without serving it: an HTTP error
/// status (429, 5xx; after `HttpSender`'s own 429 retries) or a failed
/// connection. Looks through `anyhow` context for the [`ClientError`], so it
/// works on any read wrapped by the caller. A read has no side effects, so
/// any of these is a refusal. Sends are stricter (see `classify_send_error`):
/// a gateway 5xx other than 503 may follow a send the node took, so it is
/// ambiguous there, not refused.
pub fn refused_by_rpc(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<ClientError>().is_some_and(is_refusal))
}

fn is_refusal(error: &ClientError) -> bool {
    matches!(error.kind(), RpcErrorKind::Reqwest(error) if error.is_status() || error.is_connect())
}

/// `processed` / `confirmed` / `finalized`.
pub fn parse_commitment(s: &str) -> anyhow::Result<CommitmentConfig> {
    match s {
        "processed" => Ok(CommitmentConfig::processed()),
        "confirmed" => Ok(CommitmentConfig::confirmed()),
        "finalized" => Ok(CommitmentConfig::finalized()),
        other => anyhow::bail!("unknown commitment level: {other}"),
    }
}
