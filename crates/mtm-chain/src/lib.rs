//! Off-chain Solana toolkit shared by every service that touches the chain.

mod confirm;
pub mod keys;
#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use solana_commitment_config::CommitmentConfig;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_instruction::Instruction;
use solana_rpc_client::http_sender::HttpSender;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client::rpc_client::{RpcClientConfig, SerializableTransaction};
use solana_rpc_client::rpc_sender::{RpcSender, RpcTransportStats};
use solana_rpc_client_api::client_error::{
    Error as ClientError, ErrorKind as RpcErrorKind, Result as ClientResult,
};
use solana_rpc_client_api::config::{RpcSendTransactionConfig, RpcSimulateTransactionConfig};
use solana_rpc_client_api::request::{RpcError, RpcRequest, RpcResponseErrorData};
use solana_signature::Signature;
use solana_signer::Signer;
use solana_transaction::Transaction;
use solana_transaction::versioned::VersionedTransaction;
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio::time::sleep;
use tracing::{debug, warn};

use crate::confirm::{Confirmer, Verdict, Watch};

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
    /// Give up as ambiguous after this many consecutive failed status reads.
    pub max_status_errors: u32,
}

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
    /// answered with another HTTP error status (5xx): not taken.
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
    /// Give up as ambiguous after this many consecutive failed status reads.
    pub max_status_errors: u32,
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
            last_valid_block_height: None,
            rebroadcast_interval: None,
        }
    }
}

/// Per-client transport settings.
#[derive(Clone, Copy, Debug)]
pub struct ChainOptions {
    /// RPC requests in flight at once, over every clone of the client and
    /// its [`ChainClient::with_commitment`] views; the rest queue.
    pub max_in_flight: usize,
    /// How often the shared confirmer polls pending transactions.
    pub confirm_interval: Duration,
}

/// Default for [`ChainOptions::max_in_flight`].
pub const DEFAULT_MAX_IN_FLIGHT: usize = 128;

impl Default for ChainOptions {
    fn default() -> Self {
        Self {
            max_in_flight: DEFAULT_MAX_IN_FLIGHT,
            confirm_interval: Duration::from_millis(400),
        }
    }
}

/// The client's transport: every request waits for a permit first.
#[derive(Clone)]
struct Limited {
    inner: Arc<dyn RpcSender + Send + Sync>,
    permits: Arc<Semaphore>,
}

impl RpcSender for Limited {
    // `RpcSender` is an `async_trait`; this is its expanded signature.
    fn send<'life0, 'async_trait>(
        &'life0 self,
        request: RpcRequest,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = ClientResult<serde_json::Value>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let _permit = self
                .permits
                .acquire()
                .await
                .expect("the RPC semaphore is never closed");
            self.inner.send(request, params).await
        })
    }

    fn get_transport_stats(&self) -> RpcTransportStats {
        self.inner.get_transport_stats()
    }

    fn url(&self) -> String {
        self.inner.url()
    }
}

#[derive(Clone)]
pub struct ChainClient {
    rpc: Arc<RpcClient>,
    transport: Limited,
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
        Ok(Self::from_sender(
            HttpSender::new(http_url.into()),
            commitment,
            options,
        ))
    }

    /// A client over any transport (the seam the state machine tests use).
    pub(crate) fn from_sender(
        sender: impl RpcSender + Send + Sync + 'static,
        commitment: CommitmentConfig,
        options: ChainOptions,
    ) -> Self {
        let transport = Limited {
            inner: Arc::new(sender),
            permits: Arc::new(Semaphore::new(options.max_in_flight.max(1))),
        };
        let rpc = Arc::new(RpcClient::new_sender(
            transport.clone(),
            RpcClientConfig::with_commitment(commitment),
        ));
        Self {
            confirmer: Arc::new(Confirmer::new(Arc::clone(&rpc), options.confirm_interval)),
            rpc,
            transport,
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
        let commitment = parse_commitment(commitment)?;
        Ok(Self {
            rpc: Arc::new(RpcClient::new_with_timeout_and_commitment(
                http_url.into(),
                timeout,
                commitment,
            )),
        })
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
    /// ([`SubmissionError::Expired`]).
    pub async fn submit(
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
    /// not an error by itself: the status decides.
    pub async fn send_versioned(
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

        let mut verdict = self.confirmer.watch(delivery.watch);
        let verdict = match delivery.rebroadcast_interval {
            None => verdict.await,
            Some(interval) => {
                let resend = RpcSendTransactionConfig {
                    skip_preflight: true,
                    ..delivery.send
                };
                loop {
                    tokio::select! {
                        verdict = &mut verdict => break verdict,
                        () = sleep(interval) => {
                            if let Err(error) = self.rpc.send_transaction_with_config(transaction, resend).await {
                                debug!(%signature, %error, "rebroadcast failed");
                            }
                        }
                    }
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
    /// The connection failed, or another HTTP error status: not taken.
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
        RpcErrorKind::Reqwest(error) if error.status().is_some_and(|status| status.as_u16() == 429) => {
            SendFailure::RateLimited
        }
        RpcErrorKind::Reqwest(error) if error.is_status() || error.is_connect() => {
            SendFailure::Unavailable
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

/// `processed` / `confirmed` / `finalized`.
pub fn parse_commitment(s: &str) -> anyhow::Result<CommitmentConfig> {
    match s {
        "processed" => Ok(CommitmentConfig::processed()),
        "confirmed" => Ok(CommitmentConfig::confirmed()),
        "finalized" => Ok(CommitmentConfig::finalized()),
        other => anyhow::bail!("unknown commitment level: {other}"),
    }
}
