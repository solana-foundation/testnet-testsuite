//! Transaction sending with compute-budget handling and a retry policy tuned
//! for testnet flakiness (fresh blockhash per attempt).

use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_signature::Signature;
use solana_signer::Signer;
use solana_transaction::Transaction;
use tracing::warn;

use crate::ChainClient;

#[derive(Debug, Clone)]
pub struct TxOptions {
    /// Compute-unit limit for the whole transaction.
    pub cu_limit: Option<u32>,
    /// Priority fee in micro-lamports per compute unit.
    pub cu_price_micro_lamports: Option<u64>,
    /// Full send+confirm attempts (each with a fresh blockhash).
    pub max_attempts: u32,
}

impl Default for TxOptions {
    fn default() -> Self {
        Self {
            cu_limit: None,
            cu_price_micro_lamports: None,
            max_attempts: 3,
        }
    }
}

impl ChainClient {
    /// Sign and send a transaction, confirming at the client's commitment.
    /// Prepends compute-budget instructions per `opts`; retries with a fresh
    /// blockhash on failure.
    pub async fn send_instructions(
        &self,
        instructions: &[Instruction],
        payer: &Keypair,
        extra_signers: &[&Keypair],
        opts: &TxOptions,
    ) -> anyhow::Result<Signature> {
        let mut ixs: Vec<Instruction> = Vec::with_capacity(instructions.len() + 2);
        if let Some(limit) = opts.cu_limit {
            ixs.push(ComputeBudgetInstruction::set_compute_unit_limit(limit));
        }
        if let Some(price) = opts.cu_price_micro_lamports {
            ixs.push(ComputeBudgetInstruction::set_compute_unit_price(price));
        }
        ixs.extend_from_slice(instructions);

        let mut signers: Vec<&dyn Signer> = Vec::with_capacity(extra_signers.len() + 1);
        signers.push(payer);
        for signer in extra_signers {
            signers.push(*signer);
        }

        let mut last_err = None;
        for attempt in 1..=opts.max_attempts.max(1) {
            let blockhash = self.rpc().get_latest_blockhash().await?;
            let tx = Transaction::new_signed_with_payer(
                &ixs,
                Some(&payer.pubkey()),
                &signers,
                blockhash,
            );
            match self.rpc().send_and_confirm_transaction(&tx).await {
                Ok(sig) => return Ok(sig),
                Err(e) => {
                    warn!(attempt, error = %e, "transaction send failed");
                    last_err = Some(e);
                }
            }
        }
        Err(anyhow::anyhow!(
            "transaction failed after {} attempts: {}",
            opts.max_attempts,
            last_err.map_or_else(|| "unknown".to_string(), |e| e.to_string())
        ))
    }
}
