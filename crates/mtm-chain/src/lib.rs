//! Off-chain Solana toolkit shared by every service that touches the chain.

pub mod keys;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use solana_commitment_config::CommitmentConfig;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_instruction::Instruction;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client_api::client_error::ErrorKind as RpcErrorKind;
use solana_rpc_client_api::config::RpcSendTransactionConfig;
use solana_signature::Signature;
use solana_signer::Signer;
use solana_transaction::Transaction;
use thiserror::Error;
use tokio::time::sleep;
use tracing::{debug, warn};

/// Controls transaction construction, submission retries, and confirmation.
#[derive(Clone, Copy, Debug)]
pub struct SubmissionConfig {
    pub compute_unit_limit: u32,
    pub compute_unit_price_micro_lamports: u64,
    pub commitment: CommitmentConfig,
    pub max_send_attempts: usize,
    pub confirmation_timeout: Duration,
    pub poll_interval: Duration,
}

impl Default for SubmissionConfig {
    fn default() -> Self {
        Self {
            compute_unit_limit: 1_400_000,
            compute_unit_price_micro_lamports: 0,
            commitment: CommitmentConfig::confirmed(),
            max_send_attempts: 3,
            confirmation_timeout: Duration::from_secs(45),
            poll_interval: Duration::from_millis(500),
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
    #[error("transaction submission failed: {0}")]
    Submission(String),
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
}

#[derive(Clone)]
pub struct ChainClient {
    rpc: Arc<RpcClient>,
}

impl ChainClient {
    pub fn new(http_url: impl Into<String>, commitment: &str) -> anyhow::Result<Self> {
        let commitment = parse_commitment(commitment)?;
        Ok(Self {
            rpc: Arc::new(RpcClient::new_with_commitment(http_url.into(), commitment)),
        })
    }

    pub fn rpc(&self) -> &RpcClient {
        &self.rpc
    }

    /// Cheap connectivity check; returns the current slot.
    pub async fn current_slot(&self) -> anyhow::Result<u64> {
        self.rpc.get_slot().await.context("get_slot failed")
    }

    /// Build once, simulate, submit, and confirm a transaction.
    ///
    /// Transport retries reuse the exact signed bytes. Once the transaction may
    /// have reached a validator, this method never refreshes its blockhash.
    pub async fn submit(
        &self,
        mut instructions: Vec<Instruction>,
        payer: &dyn Signer,
        additional_signers: &[&dyn Signer],
        config: SubmissionConfig,
    ) -> Result<SubmissionOutcome, SubmissionError> {
        let mut budget = vec![
            ComputeBudgetInstruction::set_compute_unit_limit(config.compute_unit_limit),
            ComputeBudgetInstruction::set_compute_unit_price(
                config.compute_unit_price_micro_lamports,
            ),
        ];
        budget.append(&mut instructions);

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
        let mut transaction = Transaction::new_with_payer(&budget, Some(&payer.pubkey()));
        transaction
            .try_sign(&signers, blockhash)
            .map_err(|error| SubmissionError::Signing(error.to_string()))?;
        let signature = transaction.signatures[0];

        let simulation = self
            .rpc
            .simulate_transaction(&transaction)
            .await
            .map_err(|error| SubmissionError::SimulationRpc(error.to_string()))?;
        let simulated_compute_units = simulation.value.units_consumed.unwrap_or_default();
        if let Some(error) = simulation.value.err {
            return Err(SubmissionError::Simulation {
                error: format!("{error:?}"),
                logs: simulation.value.logs.unwrap_or_default(),
            });
        }

        let attempts = config.max_send_attempts.max(1);
        for attempt in 1..=attempts {
            match self
                .rpc
                .send_transaction_with_config(
                    &transaction,
                    RpcSendTransactionConfig {
                        skip_preflight: true,
                        preflight_commitment: Some(config.commitment.commitment),
                        max_retries: Some(0),
                        ..RpcSendTransactionConfig::default()
                    },
                )
                .await
            {
                Ok(returned_signature) => {
                    if returned_signature != signature {
                        return Err(SubmissionError::AmbiguousConfirmation {
                            signature,
                            reason: "RPC returned a different signature".to_owned(),
                        });
                    }
                    break;
                }
                Err(error) if is_transport_error(error.kind()) && attempt < attempts => {
                    warn!(attempt, %signature, error = %error, "transaction transport retry");
                    sleep(Duration::from_millis(200 * attempt as u64)).await;
                }
                Err(error) if is_transport_error(error.kind()) => {
                    debug!(%signature, error = %error, "submission response remained ambiguous");
                    break;
                }
                Err(error) => return Err(SubmissionError::Submission(error.to_string())),
            }
        }

        let started = Instant::now();
        loop {
            let response = self
                .rpc
                .get_signature_statuses(&[signature])
                .await
                .map_err(|error| SubmissionError::AmbiguousConfirmation {
                    signature,
                    reason: error.to_string(),
                })?;
            if let Some(status) = response.value.into_iter().next().flatten() {
                if let Some(error) = status.err {
                    return Err(SubmissionError::Execution {
                        signature,
                        slot: status.slot,
                        error: format!("{error:?}"),
                    });
                }
                if status.satisfies_commitment(config.commitment) {
                    return Ok(SubmissionOutcome {
                        signature,
                        confirmation_slot: status.slot,
                        simulated_compute_units,
                    });
                }
            }

            let block_height = self.rpc.get_block_height().await.map_err(|error| {
                SubmissionError::AmbiguousConfirmation {
                    signature,
                    reason: error.to_string(),
                }
            })?;
            if block_height > last_valid_block_height {
                return Err(SubmissionError::AmbiguousConfirmation {
                    signature,
                    reason: "blockhash expired without an observed status".to_owned(),
                });
            }
            if started.elapsed() >= config.confirmation_timeout {
                return Err(SubmissionError::AmbiguousConfirmation {
                    signature,
                    reason: "confirmation deadline elapsed".to_owned(),
                });
            }
            sleep(config.poll_interval).await;
        }
    }

    // TODO: account subscription trait with pubsub-ws and yellowstone-geyser backends.
}

fn is_transport_error(error: &RpcErrorKind) -> bool {
    matches!(
        error,
        RpcErrorKind::Io(_)
            | RpcErrorKind::Reqwest(_)
            | RpcErrorKind::Middleware(_)
            | RpcErrorKind::SerdeJson(_)
    )
}

fn parse_commitment(s: &str) -> anyhow::Result<CommitmentConfig> {
    match s {
        "processed" => Ok(CommitmentConfig::processed()),
        "confirmed" => Ok(CommitmentConfig::confirmed()),
        "finalized" => Ok(CommitmentConfig::finalized()),
        other => anyhow::bail!("unknown commitment level: {other}"),
    }
}
