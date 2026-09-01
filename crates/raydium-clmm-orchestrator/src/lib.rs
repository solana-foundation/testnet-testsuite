//! Full-flow orchestration built on top of [`raydium_clmm_client`].
//!
//! The orchestrator owns sequencing and test-token funding. It receives signers
//! from its caller and never reads keypair files or environment variables.

use std::str::FromStr;
use std::sync::Arc;

use mtm_chain::{ChainClient, SubmissionConfig, SubmissionError};
use raydium_clmm_client::{
    ClosePositionOutcome, CollectOutcome, CreatePoolOutcome, CreatePoolParams,
    DecreaseLiquidityParams, IncreaseLiquidityParams, OpenPositionParams, PositionOutcome,
    RaydiumClmmClient, RaydiumClmmError, SwapExactInParams, SwapExactOutParams, SwapOutcome,
    TokenAmount,
};
pub use raydium_clmm_client::{EnsureAmmConfigOutcome, EnsureAmmConfigParams, TransactionOutcome};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use solana_account::Account;
use solana_commitment_config::CommitmentConfig;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use thiserror::Error;
use tracing::{info, info_span};

const MINT_BASE_LEN: usize = 82;
const MINT_AUTHORITY_OPTION_OFFSET: usize = 0;
const MINT_AUTHORITY_OFFSET: usize = 4;
const MINT_AUTHORITY_END: usize = 36;
const MINT_DECIMALS_OFFSET: usize = 44;

pub type Result<T> = std::result::Result<T, OrchestrationError>;

/// Admin-only orchestration for canonical Raydium program configuration.
#[derive(Clone)]
pub struct RaydiumClmmAdminOrchestrator {
    client: RaydiumClmmClient,
    program_id: Pubkey,
    admin: Pubkey,
}

impl RaydiumClmmAdminOrchestrator {
    pub fn new(
        chain: ChainClient,
        program_id: Pubkey,
        admin: Arc<dyn Signer + Send + Sync>,
        compute_unit_limit: u32,
        priority_fee_micro_lamports: u64,
        confirmation_commitment: CommitmentConfig,
    ) -> Result<Self> {
        let admin_key = admin.pubkey();
        let client = RaydiumClmmClient::new(
            chain,
            program_id,
            admin,
            0,
            compute_unit_limit,
            priority_fee_micro_lamports,
            confirmation_commitment,
        )?;
        Ok(Self {
            client,
            program_id,
            admin: admin_key,
        })
    }

    pub fn program_id(&self) -> Pubkey {
        self.program_id
    }

    pub fn admin(&self) -> Pubkey {
        self.admin
    }

    pub async fn ensure_amm_config(
        &self,
        params: EnsureAmmConfigParams,
    ) -> Result<EnsureAmmConfigOutcome> {
        Ok(self.client.ensure_amm_config(params).await?)
    }
}

/// Transaction settings shared by funding and Raydium actions.
#[derive(Clone, Copy, Debug)]
pub struct OrchestrationConfig {
    pub slippage_bps: u16,
    pub compute_unit_limit: u32,
    pub priority_fee_micro_lamports: u64,
    pub confirmation_commitment: CommitmentConfig,
}

impl Default for OrchestrationConfig {
    fn default() -> Self {
        Self {
            slippage_bps: 50,
            compute_unit_limit: 1_400_000,
            priority_fee_micro_lamports: 0,
            confirmation_commitment: CommitmentConfig::confirmed(),
        }
    }
}

/// Human-readable parameters for one complete single-pool lifecycle.
#[derive(Clone, Copy, Debug)]
pub struct FullFlowParams {
    pub amm_config: Pubkey,
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    /// UI units minted to the payer before the flow begins.
    pub funding_a: Decimal,
    /// UI units minted to the payer before the flow begins.
    pub funding_b: Decimal,
    /// Mint B per mint A. The orchestrator normalizes it to pool mint order.
    pub initial_price: Decimal,
    /// Lower price in mint B per mint A.
    pub lower_price: Decimal,
    /// Upper price in mint B per mint A.
    pub upper_price: Decimal,
    /// UI amount of mint A used to open the position.
    pub open_amount: Decimal,
    /// UI amount of mint A used to increase the position.
    pub increase_amount: Decimal,
    /// UI amount of mint A swapped to mint B.
    pub exact_in_amount: Decimal,
    /// UI amount of mint A requested from the exact-output mint B to mint A swap.
    pub exact_out_amount: Decimal,
    pub open_time: Option<u64>,
    pub with_metadata: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct FundingOutcome {
    pub transaction: TransactionOutcome,
    pub token_account_a: Pubkey,
    pub token_account_b: Pubkey,
    pub amount_a_raw: u64,
    pub amount_b_raw: u64,
}

#[derive(Clone, Debug)]
pub struct FullFlowOutcome {
    pub funding: FundingOutcome,
    pub create_pool: CreatePoolOutcome,
    pub open_position: PositionOutcome,
    pub increase_liquidity: PositionOutcome,
    pub swap_exact_in: SwapOutcome,
    pub swap_exact_out: SwapOutcome,
    pub decrease_liquidity: PositionOutcome,
    pub collect_position: CollectOutcome,
    pub close_position: ClosePositionOutcome,
}

#[derive(Clone)]
pub struct RaydiumClmmOrchestrator {
    chain: ChainClient,
    client: RaydiumClmmClient,
    program_id: Pubkey,
    payer: Arc<dyn Signer + Send + Sync>,
    mint_authority: Arc<dyn Signer + Send + Sync>,
    funding_submission: SubmissionConfig,
}

impl RaydiumClmmOrchestrator {
    pub fn new(
        chain: ChainClient,
        program_id: Pubkey,
        payer: Arc<dyn Signer + Send + Sync>,
        mint_authority: Arc<dyn Signer + Send + Sync>,
        config: OrchestrationConfig,
    ) -> Result<Self> {
        let client = RaydiumClmmClient::new(
            chain.clone(),
            program_id,
            Arc::clone(&payer),
            config.slippage_bps,
            config.compute_unit_limit,
            config.priority_fee_micro_lamports,
            config.confirmation_commitment,
        )?;
        let funding_submission = SubmissionConfig {
            compute_unit_limit: config.compute_unit_limit,
            compute_unit_price_micro_lamports: config.priority_fee_micro_lamports,
            commitment: config.confirmation_commitment,
            ..SubmissionConfig::default()
        };
        Ok(Self {
            chain,
            client,
            program_id,
            payer,
            mint_authority,
            funding_submission,
        })
    }

    pub fn payer(&self) -> Pubkey {
        self.payer.pubkey()
    }

    /// Fund the payer, create a pool and position, exercise both swap modes,
    /// collect and withdraw everything, then close the position NFT.
    pub async fn run_full_flow(&self, params: FullFlowParams) -> Result<FullFlowOutcome> {
        validate_flow_params(params)?;
        let span = info_span!(
            "raydium_clmm_full_flow",
            amm_config = %params.amm_config,
            mint_a = %params.mint_a,
            mint_b = %params.mint_b,
            payer = %self.payer(),
        );
        let _entered = span.enter();

        self.validate_prerequisites(params.amm_config).await?;

        let funding = self
            .fund_payer(
                params.mint_a,
                params.funding_a,
                params.mint_b,
                params.funding_b,
            )
            .await?;
        let create_pool = self
            .client
            .create_pool(CreatePoolParams {
                amm_config: params.amm_config,
                mint_a: params.mint_a,
                mint_b: params.mint_b,
                initial_price: params.initial_price,
                open_time: params.open_time,
            })
            .await?;
        let (lower_price, upper_price) = normalized_range(
            params.mint_a,
            params.mint_b,
            params.lower_price,
            params.upper_price,
        )?;
        let pool = create_pool.accounts.pool;
        let open_position = self
            .client
            .open_position(OpenPositionParams {
                pool,
                lower_price,
                upper_price,
                input: TokenAmount {
                    mint: params.mint_a,
                    amount: params.open_amount,
                },
                with_metadata: params.with_metadata,
            })
            .await?;
        let position_mint = open_position.accounts.position_nft_mint;
        let increase_liquidity = self
            .client
            .increase_liquidity(IncreaseLiquidityParams {
                position_mint,
                input: TokenAmount {
                    mint: params.mint_a,
                    amount: params.increase_amount,
                },
            })
            .await?;
        let swap_exact_in = self
            .client
            .swap_exact_in(SwapExactInParams {
                pool,
                input_mint: params.mint_a,
                output_mint: params.mint_b,
                amount_in: params.exact_in_amount,
            })
            .await?;
        let swap_exact_out = self
            .client
            .swap_exact_out(SwapExactOutParams {
                pool,
                input_mint: params.mint_b,
                output_mint: params.mint_a,
                amount_out: params.exact_out_amount,
            })
            .await?;
        let decrease_liquidity = self
            .client
            .decrease_liquidity(DecreaseLiquidityParams {
                position_mint,
                amount: None,
            })
            .await?;
        let collect_position = self.client.collect_position(position_mint).await?;
        let close_position = self.client.close_position(position_mint).await?;

        info!(%pool, %position_mint, "Raydium CLMM full flow complete");
        Ok(FullFlowOutcome {
            funding,
            create_pool,
            open_position,
            increase_liquidity,
            swap_exact_in,
            swap_exact_out,
            decrease_liquidity,
            collect_position,
            close_position,
        })
    }

    pub async fn fund_payer(
        &self,
        mint_a: Pubkey,
        amount_a: Decimal,
        mint_b: Pubkey,
        amount_b: Decimal,
    ) -> Result<FundingOutcome> {
        ensure_positive(amount_a, "funding_a")?;
        ensure_positive(amount_b, "funding_b")?;
        if mint_a == mint_b {
            return Err(OrchestrationError::InvalidInput(
                "funding mints must be different".to_owned(),
            ));
        }
        let accounts = self
            .chain
            .rpc()
            .get_multiple_accounts(&[mint_a, mint_b])
            .await
            .map_err(|error| OrchestrationError::Rpc(error.to_string()))?;
        let info_a = decode_mint(
            mint_a,
            accounts.first().and_then(Option::as_ref),
            self.mint_authority.pubkey(),
        )?;
        let info_b = decode_mint(
            mint_b,
            accounts.get(1).and_then(Option::as_ref),
            self.mint_authority.pubkey(),
        )?;
        let amount_a_raw = ui_to_raw(amount_a, info_a.decimals)?;
        let amount_b_raw = ui_to_raw(amount_b, info_b.decimals)?;
        let token_account_a = associated_token_address(self.payer(), mint_a, info_a.token_program)?;
        let token_account_b = associated_token_address(self.payer(), mint_b, info_b.token_program)?;
        let instructions = vec![
            create_ata_instruction(
                self.payer(),
                self.payer(),
                mint_a,
                info_a.token_program,
                token_account_a,
            )?,
            mint_to_checked_instruction(
                mint_a,
                token_account_a,
                self.mint_authority.pubkey(),
                info_a.token_program,
                amount_a_raw,
                info_a.decimals,
            ),
            create_ata_instruction(
                self.payer(),
                self.payer(),
                mint_b,
                info_b.token_program,
                token_account_b,
            )?,
            mint_to_checked_instruction(
                mint_b,
                token_account_b,
                self.mint_authority.pubkey(),
                info_b.token_program,
                amount_b_raw,
                info_b.decimals,
            ),
        ];
        let submitted = self
            .chain
            .submit(
                instructions,
                self.payer.as_ref(),
                &[self.mint_authority.as_ref()],
                self.funding_submission,
            )
            .await?;
        Ok(FundingOutcome {
            transaction: TransactionOutcome {
                signature: submitted.signature,
                confirmation_slot: submitted.confirmation_slot,
                simulated_compute_units: submitted.simulated_compute_units,
            },
            token_account_a,
            token_account_b,
            amount_a_raw,
            amount_b_raw,
        })
    }

    async fn validate_prerequisites(&self, amm_config: Pubkey) -> Result<()> {
        let accounts = self
            .chain
            .rpc()
            .get_multiple_accounts(&[self.program_id, amm_config])
            .await
            .map_err(|error| OrchestrationError::Rpc(error.to_string()))?;
        let program = accounts
            .first()
            .and_then(Option::as_ref)
            .ok_or(OrchestrationError::MissingProgram(self.program_id))?;
        if !program.executable {
            return Err(OrchestrationError::InvalidProgram(self.program_id));
        }
        let config = accounts
            .get(1)
            .and_then(Option::as_ref)
            .ok_or(OrchestrationError::MissingAmmConfig(amm_config))?;
        if config.owner != self.program_id {
            return Err(OrchestrationError::MalformedAmmConfig {
                config: amm_config,
                expected_owner: self.program_id,
                actual_owner: config.owner,
            });
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct MintInfo {
    token_program: Pubkey,
    decimals: u8,
}

fn decode_mint(
    key: Pubkey,
    account: Option<&Account>,
    expected_authority: Pubkey,
) -> Result<MintInfo> {
    let account = account.ok_or(OrchestrationError::MissingMint(key))?;
    let classic = pubkey("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")?;
    let token_2022 = pubkey("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb")?;
    if account.owner != classic && account.owner != token_2022 {
        return Err(OrchestrationError::UnsupportedMint {
            mint: key,
            reason: format!("unsupported token program {}", account.owner),
        });
    }
    if account.data.len() < MINT_BASE_LEN {
        return Err(OrchestrationError::MalformedMint {
            mint: key,
            reason: format!("mint data is only {} bytes", account.data.len()),
        });
    }
    let authority_tag = u32::from_le_bytes(
        account.data[MINT_AUTHORITY_OPTION_OFFSET..MINT_AUTHORITY_OFFSET]
            .try_into()
            .map_err(|_| OrchestrationError::MalformedMint {
                mint: key,
                reason: "invalid mint authority option".to_owned(),
            })?,
    );
    if authority_tag != 1 {
        return Err(OrchestrationError::UnsupportedMint {
            mint: key,
            reason: "mint has no mint authority".to_owned(),
        });
    }
    let authority = Pubkey::new_from_array(
        account.data[MINT_AUTHORITY_OFFSET..MINT_AUTHORITY_END]
            .try_into()
            .map_err(|_| OrchestrationError::MalformedMint {
                mint: key,
                reason: "invalid mint authority".to_owned(),
            })?,
    );
    if authority != expected_authority {
        return Err(OrchestrationError::UnsupportedMint {
            mint: key,
            reason: format!(
                "mint authority is {authority}, but injected signer is {expected_authority}"
            ),
        });
    }
    Ok(MintInfo {
        token_program: account.owner,
        decimals: account.data[MINT_DECIMALS_OFFSET],
    })
}

fn associated_token_address(owner: Pubkey, mint: Pubkey, token_program: Pubkey) -> Result<Pubkey> {
    let associated_program = pubkey("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL")?;
    Ok(Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &associated_program,
    )
    .0)
}

fn create_ata_instruction(
    payer: Pubkey,
    owner: Pubkey,
    mint: Pubkey,
    token_program: Pubkey,
    ata: Pubkey,
) -> Result<Instruction> {
    Ok(Instruction {
        program_id: pubkey("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL")?,
        accounts: vec![
            AccountMeta::new(payer, true),
            AccountMeta::new(ata, false),
            AccountMeta::new_readonly(owner, false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(Pubkey::default(), false),
            AccountMeta::new_readonly(token_program, false),
        ],
        data: vec![1],
    })
}

fn mint_to_checked_instruction(
    mint: Pubkey,
    destination: Pubkey,
    authority: Pubkey,
    token_program: Pubkey,
    amount: u64,
    decimals: u8,
) -> Instruction {
    let mut data = Vec::with_capacity(10);
    data.push(14);
    data.extend_from_slice(&amount.to_le_bytes());
    data.push(decimals);
    Instruction {
        program_id: token_program,
        accounts: vec![
            AccountMeta::new(mint, false),
            AccountMeta::new(destination, false),
            AccountMeta::new_readonly(authority, true),
        ],
        data,
    }
}

fn normalized_range(
    mint_a: Pubkey,
    mint_b: Pubkey,
    lower: Decimal,
    upper: Decimal,
) -> Result<(Decimal, Decimal)> {
    if mint_a < mint_b {
        return Ok((lower, upper));
    }
    let normalized_lower = Decimal::ONE.checked_div(upper).ok_or_else(|| {
        OrchestrationError::InvalidInput("upper price inversion failed".to_owned())
    })?;
    let normalized_upper = Decimal::ONE.checked_div(lower).ok_or_else(|| {
        OrchestrationError::InvalidInput("lower price inversion failed".to_owned())
    })?;
    Ok((normalized_lower, normalized_upper))
}

fn validate_flow_params(params: FullFlowParams) -> Result<()> {
    if params.mint_a == params.mint_b {
        return Err(OrchestrationError::InvalidInput(
            "mint_a and mint_b must differ".to_owned(),
        ));
    }
    for (name, value) in [
        ("funding_a", params.funding_a),
        ("funding_b", params.funding_b),
        ("initial_price", params.initial_price),
        ("lower_price", params.lower_price),
        ("upper_price", params.upper_price),
        ("open_amount", params.open_amount),
        ("increase_amount", params.increase_amount),
        ("exact_in_amount", params.exact_in_amount),
        ("exact_out_amount", params.exact_out_amount),
    ] {
        ensure_positive(value, name)?;
    }
    if params.lower_price >= params.upper_price {
        return Err(OrchestrationError::InvalidInput(
            "lower_price must be less than upper_price".to_owned(),
        ));
    }
    if params.initial_price <= params.lower_price || params.initial_price >= params.upper_price {
        return Err(OrchestrationError::InvalidInput(
            "initial_price must lie strictly inside the position range".to_owned(),
        ));
    }
    Ok(())
}

fn ensure_positive(value: Decimal, name: &str) -> Result<()> {
    if value <= Decimal::ZERO {
        return Err(OrchestrationError::InvalidInput(format!(
            "{name} must be greater than zero"
        )));
    }
    Ok(())
}

fn ui_to_raw(amount: Decimal, decimals: u8) -> Result<u64> {
    let factor_raw = 10_u64.checked_pow(u32::from(decimals)).ok_or_else(|| {
        OrchestrationError::InvalidInput(format!(
            "mint decimals {decimals} cannot be represented as a u64 scale"
        ))
    })?;
    let factor = Decimal::from(factor_raw);
    let scaled = amount
        .checked_mul(factor)
        .ok_or_else(|| OrchestrationError::InvalidInput("token amount overflow".to_owned()))?;
    if scaled.fract() != Decimal::ZERO {
        return Err(OrchestrationError::InvalidInput(format!(
            "token amount has more than {decimals} decimal places"
        )));
    }
    scaled
        .to_u64()
        .ok_or_else(|| OrchestrationError::InvalidInput("token amount exceeds u64".to_owned()))
}

fn pubkey(value: &str) -> Result<Pubkey> {
    Pubkey::from_str(value)
        .map_err(|error| OrchestrationError::InvalidInput(format!("invalid program ID: {error}")))
}

#[derive(Debug, Error)]
pub enum OrchestrationError {
    #[error("invalid orchestration input: {0}")]
    InvalidInput(String),
    #[error("Raydium program account {0} does not exist")]
    MissingProgram(Pubkey),
    #[error("Raydium program account {0} is not executable")]
    InvalidProgram(Pubkey),
    #[error("AMM config account {0} does not exist")]
    MissingAmmConfig(Pubkey),
    #[error(
        "AMM config {config} is owned by {actual_owner}; expected selected program {expected_owner}"
    )]
    MalformedAmmConfig {
        config: Pubkey,
        expected_owner: Pubkey,
        actual_owner: Pubkey,
    },
    #[error("mint account {0} does not exist")]
    MissingMint(Pubkey),
    #[error("malformed mint {mint}: {reason}")]
    MalformedMint { mint: Pubkey, reason: String },
    #[error("unsupported mint {mint}: {reason}")]
    UnsupportedMint { mint: Pubkey, reason: String },
    #[error("RPC request failed: {0}")]
    Rpc(String),
    #[error(transparent)]
    FundingSubmission(#[from] SubmissionError),
    #[error(transparent)]
    Raydium(#[from] RaydiumClmmError),
}
