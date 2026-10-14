//! CLI-side plumbing shared by the per-venue orchestrator binaries:
//! Doppler-injected keypairs, transaction argument groups, mint-pair
//! enumeration, and the ephemeral-user lifecycle
//! (save recovery key → fund from faucet → run flow → return funds).

use std::collections::BTreeSet;
use std::future::Future;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{env, fs};

use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use mtm_chain::{ChainClient, SubmissionConfig, SubmissionOutcome};
use serde_json::{Value, json};
use solana_commitment_config::CommitmentConfig;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_signer::Signer;

use crate::spl;

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Commitment {
    Processed,
    Confirmed,
    Finalized,
}

impl Commitment {
    pub fn config(self) -> CommitmentConfig {
        match self {
            Self::Processed => CommitmentConfig::processed(),
            Self::Confirmed => CommitmentConfig::confirmed(),
            Self::Finalized => CommitmentConfig::finalized(),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Processed => "processed",
            Self::Confirmed => "confirmed",
            Self::Finalized => "finalized",
        }
    }
}

/// Transaction settings shared by every venue command.
#[derive(Debug, Args)]
pub struct TransactionArgs {
    #[arg(long, default_value_t = 50)]
    pub slippage_bps: u16,
    #[arg(long, default_value_t = 1_400_000)]
    pub compute_unit_limit: u32,
    #[arg(long, default_value_t = 0)]
    pub priority_fee_micro_lamports: u64,
    #[arg(long, value_enum, default_value_t = Commitment::Confirmed)]
    pub commitment: Commitment,
}

impl TransactionArgs {
    pub fn submission(&self) -> SubmissionConfig {
        SubmissionConfig {
            compute_unit_limit: self.compute_unit_limit,
            compute_unit_price_micro_lamports: self.priority_fee_micro_lamports,
            commitment: self.commitment.config(),
            ..SubmissionConfig::default()
        }
    }
}

/// Ephemeral-user settings shared by every `user-flow` command.
#[derive(Debug, Args)]
pub struct EphemeralUserArgs {
    #[arg(long, default_value = "KEYPAIR_FAUCET")]
    pub faucet_keypair_env: String,
    #[arg(long, default_value = "KEYPAIR_TOKEN_MINT_AUTHORITY")]
    pub mint_authority_keypair_env: String,
    /// Lamports sent by the faucet to each ephemeral user before its flow.
    #[arg(long, default_value_t = 1_000_000_000)]
    pub user_lamports: u64,
}

pub async fn connected_chain(rpc_url: &str, commitment: Commitment) -> Result<ChainClient> {
    let chain = ChainClient::new(rpc_url, commitment.name())?;
    chain
        .current_slot()
        .await
        .with_context(|| format!("connect to {rpc_url}"))?;
    Ok(chain)
}

pub fn keypair_from_environment(name: &str) -> Result<Keypair> {
    keypair_from_encoded(
        &env::var(name).with_context(|| format!("environment variable {name} is unset"))?,
    )
    .with_context(|| format!("{name} is not a valid keypair"))
}

pub fn signer_from_environment(name: &str) -> Result<Arc<dyn Signer + Send + Sync>> {
    Ok(Arc::new(keypair_from_environment(name)?))
}

pub fn keypair_from_encoded(encoded: &str) -> Result<Keypair> {
    let bytes: Vec<u8> =
        serde_json::from_str(encoded).context("keypair must be a JSON-encoded byte array")?;
    if bytes.len() != 64 {
        bail!("keypair must contain exactly 64 byte values")
    }
    Keypair::try_from(bytes.as_slice()).context("invalid keypair bytes")
}

/// Use an explicit public key, or read it from a named variable that holds
/// either the address itself or the keypair. Accepting an address lets
/// commands that only need it run without the secret in their environment.
pub fn resolve_pubkey(value: Option<Pubkey>, keypair_env: &str, label: &str) -> Result<Pubkey> {
    if let Some(pubkey) = value {
        return Ok(pubkey);
    }
    if let Some(pubkey) = env::var(keypair_env)
        .ok()
        .and_then(|value| value.trim().parse::<Pubkey>().ok())
    {
        return Ok(pubkey);
    }
    keypair_from_environment(keypair_env)
        .map(|keypair| keypair.pubkey())
        .with_context(|| format!("derive {label} address from {keypair_env}"))
}

pub fn unique_mints(mints: Vec<Pubkey>, source: &str) -> Result<Vec<Pubkey>> {
    let mints: BTreeSet<Pubkey> = mints.into_iter().collect();
    if mints.len() < 2 {
        bail!("{source} must provide at least two distinct mints")
    }
    Ok(mints.into_iter().collect())
}

pub fn unordered_pairs(mints: &[Pubkey]) -> Vec<(Pubkey, Pubkey)> {
    let mut pairs = Vec::new();
    for (i, mint_a) in mints.iter().enumerate() {
        for mint_b in &mints[i + 1..] {
            pairs.push((*mint_a, *mint_b));
        }
    }
    pairs
}

pub fn transaction_json(
    signature: &Signature,
    confirmation_slot: u64,
    simulated_compute_units: u64,
) -> Value {
    json!({ "signature": signature.to_string(), "confirmationSlot": confirmation_slot, "simulatedComputeUnits": simulated_compute_units })
}

pub fn submission_json(outcome: &SubmissionOutcome) -> Value {
    transaction_json(
        &outcome.signature,
        outcome.confirmation_slot,
        outcome.simulated_compute_units,
    )
}

/// Funds returned from one ephemeral user to the faucet.
#[derive(Clone, Debug)]
pub struct Recovery {
    pub token_returns: Vec<SubmissionOutcome>,
    pub lamports_returned: u64,
    pub lamport_return: Option<SubmissionOutcome>,
}

impl Recovery {
    pub fn json(&self) -> Value {
        json!({ "lamports": self.lamports_returned, "lamportTransaction": self.lamport_return.as_ref().map(submission_json), "tokenTransactions": self.token_returns.iter().map(submission_json).collect::<Vec<_>>() })
    }
}

/// One completed ephemeral-user flow against one target (pool or market).
pub struct EphemeralRun<P, T> {
    pub target: P,
    pub user: Pubkey,
    pub startup_funding: SubmissionOutcome,
    pub outcome: T,
    pub recovery: Recovery,
}

/// A flow target plus the mints whose balances must return to the faucet.
pub struct EphemeralTarget<P> {
    pub target: P,
    pub label: String,
    pub mints: Vec<Pubkey>,
}

type FundedUser<P> = (
    EphemeralTarget<P>,
    Arc<dyn Signer + Send + Sync>,
    PathBuf,
    SubmissionOutcome,
);

pub struct EphemeralConfig {
    pub recovery_dir: PathBuf,
    pub user_lamports: u64,
    pub submission: SubmissionConfig,
}

/// Run one fresh, faucet-funded user per target.
///
/// Every user's keypair is persisted (0600, inside a 0700 directory) before
/// it receives SOL, and is deleted only after its funds are back with the
/// faucet. A failed recovery leaves the keypair on disk. Every funded user is
/// run and reclaimed even after another fails, so a failure never strands the
/// SOL already sent to the remaining users; failures are reported together.
pub async fn run_ephemeral_users<P, T, F, Fut>(
    chain: &ChainClient,
    faucet: Arc<dyn Signer + Send + Sync>,
    config: &EphemeralConfig,
    targets: Vec<EphemeralTarget<P>>,
    mut flow: F,
) -> Result<Vec<EphemeralRun<P, T>>>
where
    F: FnMut(&P, Arc<dyn Signer + Send + Sync>) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    prepare_recovery_dir(&config.recovery_dir)?;
    let mut funded: Vec<FundedUser<P>> = Vec::with_capacity(targets.len());
    for target in targets {
        let user = Arc::new(Keypair::new());
        let user_pubkey = user.pubkey();
        let recovery_path = save_recovery_keypair(&config.recovery_dir, &user)?;
        tracing::info!(target = %target.label, user = %user_pubkey, recovery_file = %recovery_path.display(), "saved ephemeral user recovery keypair");
        let user: Arc<dyn Signer + Send + Sync> = user;
        let startup_funding = match transfer_lamports(
            chain,
            faucet.as_ref(),
            faucet.as_ref(),
            user_pubkey,
            config.user_lamports,
            config.submission,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                for (funded_target, funded_user, funded_path, _) in funded {
                    let recovery = reclaim_user_funds(
                        chain,
                        faucet.as_ref(),
                        funded_user.as_ref(),
                        &funded_target.mints,
                        config.submission,
                    )
                    .await;
                    finish_recovery(&funded_target.label, &funded_path, recovery.as_ref());
                }
                return Err(error).context("fund ephemeral user from the faucet");
            }
        };
        funded.push((target, user, recovery_path, startup_funding));
    }

    let total = funded.len();
    let mut runs = Vec::with_capacity(total);
    let mut failures = Vec::new();
    for (target, user, recovery_path, startup_funding) in funded {
        let user_pubkey = user.pubkey();
        tracing::info!(target = %target.label, user = %user_pubkey, "starting ephemeral user flow");
        let result = flow(&target.target, Arc::clone(&user)).await;
        let recovery = reclaim_user_funds(
            chain,
            faucet.as_ref(),
            user.as_ref(),
            &target.mints,
            config.submission,
        )
        .await;
        finish_recovery(&target.label, &recovery_path, recovery.as_ref());
        if let Err(error) = &result {
            tracing::error!(target = %target.label, user = %user_pubkey, error = %error, "ephemeral user flow failed");
        }
        match (result, recovery) {
            (Ok(outcome), Ok(recovery)) => runs.push(EphemeralRun {
                target: target.target,
                user: user_pubkey,
                startup_funding,
                outcome,
                recovery,
            }),
            (result, recovery) => {
                let mut reasons = Vec::new();
                if let Err(error) = result {
                    reasons.push(format!("flow: {error:#}"));
                }
                if let Err(error) = recovery {
                    reasons.push(format!("recovery (keypair retained): {error:#}"));
                }
                failures.push(format!(
                    "{} user {user_pubkey}: {}",
                    target.label,
                    reasons.join("; ")
                ));
            }
        }
    }
    if !failures.is_empty() {
        bail!(
            "{} of {total} ephemeral user flows failed:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
    Ok(runs)
}

/// Delete a user's recovery keypair once its funds are back with the faucet;
/// keep it (and say so) when the recovery failed.
fn finish_recovery(
    label: &str,
    path: &Path,
    recovery: std::result::Result<&Recovery, &anyhow::Error>,
) {
    match recovery {
        Ok(_) => {
            if let Err(error) = fs::remove_file(path) {
                tracing::warn!(target = %label, path = %path.display(), %error, "failed to remove recovered keypair");
            }
        }
        Err(error) => {
            tracing::error!(target = %label, path = %path.display(), error = %error, "failed to return ephemeral user funds; recovery keypair retained");
        }
    }
}

fn prepare_recovery_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)
        .with_context(|| format!("create recovery directory {}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("restrict recovery directory {}", dir.display()))
}

fn save_recovery_keypair(dir: &Path, user: &Keypair) -> Result<PathBuf> {
    let path = dir.join(format!("{}.json", user.pubkey()));
    let bytes = serde_json::to_vec(user.to_bytes().as_slice())?;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("save recovery keypair to {}", path.display()))?
        .write_all(&bytes)
        .with_context(|| format!("write recovery keypair to {}", path.display()))?;
    Ok(path)
}

/// Return every listed token balance (closing the ATAs) and then all SOL.
pub async fn reclaim_user_funds(
    chain: &ChainClient,
    faucet: &dyn Signer,
    user: &dyn Signer,
    mints: &[Pubkey],
    submission: SubmissionConfig,
) -> Result<Recovery> {
    let mut token_returns = Vec::new();
    for mint in mints {
        if let Some(transaction) =
            return_token_balance(chain, faucet, user, *mint, submission).await?
        {
            token_returns.push(transaction);
        }
    }
    // Read at the submission commitment, after the token returns confirmed.
    let lamports = chain
        .rpc()
        .get_balance_with_commitment(&user.pubkey(), submission.commitment)
        .await
        .context("read ephemeral user balance before recovery")?
        .value;
    let lamport_return = if lamports == 0 {
        None
    } else {
        Some(transfer_lamports(chain, faucet, user, faucet.pubkey(), lamports, submission).await?)
    };
    Ok(Recovery {
        token_returns,
        lamports_returned: lamports,
        lamport_return,
    })
}

async fn return_token_balance(
    chain: &ChainClient,
    faucet: &dyn Signer,
    user: &dyn Signer,
    mint: Pubkey,
    submission: SubmissionConfig,
) -> Result<Option<SubmissionOutcome>> {
    let mint_account = chain
        .rpc()
        .get_account(&mint)
        .await
        .with_context(|| format!("read mint {mint} for recovery"))?;
    let summary = spl::decode_mint(mint_account.owner, &mint_account.data)
        .with_context(|| format!("{mint} is not a token mint"))?;
    let token_program = summary.token_program;
    let source = spl::associated_token_address(&user.pubkey(), &mint, &token_program);
    let Some(source_account) = chain
        .rpc()
        .get_account_with_commitment(&source, submission.commitment)
        .await
        .context("read ephemeral user token balance")?
        .value
    else {
        return Ok(None);
    };
    let amount = spl::decode_token_account(&source_account.data)
        .with_context(|| format!("token account {source} is too short to contain a balance"))?
        .amount;
    let destination = spl::associated_token_address(&faucet.pubkey(), &mint, &token_program);
    let mut instructions = Vec::new();
    if amount > 0 {
        instructions.push(spl::create_associated_token_account_idempotent(
            &faucet.pubkey(),
            &faucet.pubkey(),
            &mint,
            &token_program,
        ));
        instructions.push(spl::transfer_checked(
            &token_program,
            &source,
            &mint,
            &destination,
            &user.pubkey(),
            amount,
            summary.decimals,
        ));
    }
    // Closing the now-empty ATA returns its rent to the faucet as well as its
    // token balance, so the generated user does not retain stranded lamports.
    // Token-2022 refuses to close it while transfer fees are withheld on it.
    let withheld = spl::withheld_transfer_fee(&source_account.data);
    if withheld == 0 {
        instructions.push(spl::close_account(
            &token_program,
            &source,
            &faucet.pubkey(),
            &user.pubkey(),
        ));
    } else {
        tracing::warn!(account = %source, withheld, "token account holds withheld transfer fees; leaving it open");
    }
    if instructions.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        chain
            .submit(instructions, faucet, &[user], submission)
            .await?,
    ))
}

pub async fn transfer_lamports(
    chain: &ChainClient,
    fee_payer: &dyn Signer,
    source: &dyn Signer,
    destination: Pubkey,
    lamports: u64,
    submission: SubmissionConfig,
) -> Result<SubmissionOutcome> {
    if lamports == 0 {
        bail!("lamport transfer amount must be greater than zero")
    }
    Ok(chain
        .submit(
            vec![spl::system_transfer(
                &source.pubkey(),
                &destination,
                lamports,
            )],
            fee_payer,
            &[source],
            submission,
        )
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_are_unordered_and_unique() {
        let mints = unique_mints(
            vec![
                Pubkey::new_from_array([3; 32]),
                Pubkey::new_from_array([1; 32]),
                Pubkey::new_from_array([2; 32]),
                Pubkey::new_from_array([1; 32]),
            ],
            "--mint",
        )
        .expect("three distinct mints");
        let pairs = unordered_pairs(&mints);
        assert_eq!(pairs.len(), 3);
        assert!(pairs.iter().all(|(a, b)| a < b));
    }

    #[test]
    fn rejects_single_mint() {
        assert!(unique_mints(vec![Pubkey::new_from_array([1; 32]); 2], "--mint").is_err());
    }

    #[test]
    fn keypair_round_trips_through_json() {
        let keypair = Keypair::new();
        let encoded = serde_json::to_string(keypair.to_bytes().as_slice()).expect("encodes");
        let decoded = keypair_from_encoded(&encoded).expect("decodes");
        assert_eq!(decoded.pubkey(), keypair.pubkey());
        assert!(keypair_from_encoded("[1,2,3]").is_err());
    }
}
