//! Test-token funding: mint UI-denominated amounts of authority-controlled
//! test mints into a payer's ATAs in one simulated, confirmed transaction.

use std::sync::Arc;

use mtm_chain::{ChainClient, SubmissionConfig, SubmissionError, SubmissionOutcome};
use rust_decimal::Decimal;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use thiserror::Error;

use crate::spl;

#[derive(Debug, Error)]
pub enum FundingError {
    #[error("invalid funding input: {0}")]
    InvalidInput(String),
    #[error("mint account {0} does not exist")]
    MissingMint(Pubkey),
    #[error("unsupported mint {mint}: {reason}")]
    UnsupportedMint { mint: Pubkey, reason: String },
    #[error("RPC request failed: {0}")]
    Rpc(String),
    #[error(transparent)]
    Submission(#[from] SubmissionError),
}

#[derive(Clone, Copy, Debug)]
pub struct Grant {
    pub mint: Pubkey,
    /// UI units minted to the recipient.
    pub amount: Decimal,
}

#[derive(Clone, Copy, Debug)]
pub struct GrantOutcome {
    pub mint: Pubkey,
    pub token_account: Pubkey,
    pub token_program: Pubkey,
    pub amount_raw: u64,
    pub decimals: u8,
}

#[derive(Clone, Debug)]
pub struct FundingOutcome {
    pub transaction: SubmissionOutcome,
    pub grants: Vec<GrantOutcome>,
}

/// Mints test tokens to `payer` using an injected mint authority.
#[derive(Clone)]
pub struct TestTokenFunder {
    chain: ChainClient,
    payer: Arc<dyn Signer + Send + Sync>,
    mint_authority: Arc<dyn Signer + Send + Sync>,
    submission: SubmissionConfig,
}

impl TestTokenFunder {
    pub fn new(
        chain: ChainClient,
        payer: Arc<dyn Signer + Send + Sync>,
        mint_authority: Arc<dyn Signer + Send + Sync>,
        submission: SubmissionConfig,
    ) -> Self {
        Self {
            chain,
            payer,
            mint_authority,
            submission,
        }
    }

    /// Create the payer's ATAs idempotently and mint every grant into them.
    pub async fn fund(&self, grants: &[Grant]) -> Result<FundingOutcome, FundingError> {
        if grants.is_empty() {
            return Err(FundingError::InvalidInput(
                "at least one grant is required".to_owned(),
            ));
        }
        for (index, grant) in grants.iter().enumerate() {
            if grants[..index].iter().any(|other| other.mint == grant.mint) {
                return Err(FundingError::InvalidInput(format!(
                    "mint {} is granted more than once",
                    grant.mint
                )));
            }
        }
        let keys: Vec<Pubkey> = grants.iter().map(|grant| grant.mint).collect();
        let accounts = self
            .chain
            .rpc()
            .get_multiple_accounts(&keys)
            .await
            .map_err(|error| FundingError::Rpc(error.to_string()))?;
        let payer = self.payer.pubkey();
        let authority = self.mint_authority.pubkey();
        let mut instructions = Vec::with_capacity(grants.len() * 2);
        let mut outcomes = Vec::with_capacity(grants.len());
        for (grant, account) in grants.iter().zip(accounts) {
            let account = account.ok_or(FundingError::MissingMint(grant.mint))?;
            let mint = spl::decode_mint(account.owner, &account.data).ok_or_else(|| {
                FundingError::UnsupportedMint {
                    mint: grant.mint,
                    reason: format!("not a token mint (owner {})", account.owner),
                }
            })?;
            match mint.mint_authority {
                Some(key) if key == authority => {}
                Some(key) => {
                    return Err(FundingError::UnsupportedMint {
                        mint: grant.mint,
                        reason: format!(
                            "mint authority is {key}, but injected signer is {authority}"
                        ),
                    });
                }
                None => {
                    return Err(FundingError::UnsupportedMint {
                        mint: grant.mint,
                        reason: "mint has no mint authority".to_owned(),
                    });
                }
            }
            let amount_raw =
                spl::ui_to_raw(grant.amount, mint.decimals).map_err(FundingError::InvalidInput)?;
            let token_account =
                spl::associated_token_address(&payer, &grant.mint, &mint.token_program);
            instructions.push(spl::create_associated_token_account_idempotent(
                &payer,
                &payer,
                &grant.mint,
                &mint.token_program,
            ));
            instructions.push(spl::mint_to_checked(
                &mint.token_program,
                &grant.mint,
                &token_account,
                &authority,
                amount_raw,
                mint.decimals,
            ));
            outcomes.push(GrantOutcome {
                mint: grant.mint,
                token_account,
                token_program: mint.token_program,
                amount_raw,
                decimals: mint.decimals,
            });
        }
        let transaction = self
            .chain
            .submit(
                instructions,
                self.payer.as_ref(),
                &[self.mint_authority.as_ref()],
                self.submission,
            )
            .await?;
        Ok(FundingOutcome {
            transaction,
            grants: outcomes,
        })
    }
}
