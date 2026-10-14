//! Full-flow orchestration built on top of [`orca_whirlpool_client`].
//!
//! The orchestrator owns sequencing and test-token funding. It receives signers
//! from its caller and never reads keypair files or environment variables.

use std::sync::Arc;

use mtm_chain::{ChainClient, SubmissionConfig};
use mtm_dex_harness::funding::{FundingError, Grant, TestTokenFunder};
use orca_whirlpool_client::{
    ClosePositionOutcome, CollectOutcome, CreatePoolOutcome, CreatePoolParams,
    DecreaseLiquidityParams, IncreaseLiquidityParams, OpenPositionParams, OrcaWhirlpoolClient,
    OrcaWhirlpoolError, PositionOutcome, SwapExactInParams, SwapExactOutParams, SwapOutcome,
    TokenAmount,
};
pub use orca_whirlpool_client::{
    EnsureConfigOutcome, EnsureConfigParams, TickArraySeedOutcome, TransactionOutcome,
};
use rust_decimal::Decimal;
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use thiserror::Error;
use tracing::{Instrument, info, info_span};

pub type Result<T> = std::result::Result<T, OrchestrationError>;

/// Derive the canonical pool address for a config, unordered mint pair, and
/// tick spacing.
pub fn derive_pool_address(
    program_id: Pubkey,
    whirlpools_config: Pubkey,
    mint_a: Pubkey,
    mint_b: Pubkey,
    tick_spacing: u16,
) -> Result<Pubkey> {
    Ok(orca_whirlpool_client::derive_pool_address(
        program_id,
        whirlpools_config,
        mint_a,
        mint_b,
        tick_spacing,
    )?)
}

/// Admin-only orchestration for the WhirlpoolsConfig and its fee tier.
#[derive(Clone)]
pub struct OrcaWhirlpoolAdminOrchestrator {
    client: OrcaWhirlpoolClient,
    admin: Pubkey,
}

impl OrcaWhirlpoolAdminOrchestrator {
    pub fn new(
        chain: ChainClient,
        program_id: Pubkey,
        admin: Arc<dyn Signer + Send + Sync>,
        compute_unit_limit: u32,
        priority_fee_micro_lamports: u64,
        confirmation_commitment: CommitmentConfig,
    ) -> Result<Self> {
        let admin_key = admin.pubkey();
        let client = OrcaWhirlpoolClient::new(
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
            admin: admin_key,
        })
    }

    pub fn program_id(&self) -> Pubkey {
        self.client.program_id()
    }

    pub fn admin(&self) -> Pubkey {
        self.admin
    }

    pub async fn ensure_config(
        &self,
        config_signer: &dyn Signer,
        params: EnsureConfigParams,
    ) -> Result<EnsureConfigOutcome> {
        Ok(self.client.ensure_config(config_signer, params).await?)
    }
}

/// Transaction settings shared by funding and Orca actions.
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

/// Human-readable parameters for creating one pool.
#[derive(Clone, Copy, Debug)]
pub struct PoolCreationParams {
    pub whirlpools_config: Pubkey,
    pub tick_spacing: u16,
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    /// Mint B per mint A. The orchestrator normalizes it to pool mint order.
    pub initial_price: Decimal,
    /// Tick arrays covering this range (mint B per mint A) are initialized by
    /// the pool creator so user flows inside it pay no tick-array rent.
    pub seed_lower_price: Decimal,
    pub seed_upper_price: Decimal,
}

/// Human-readable parameters for one user lifecycle on an existing pool.
#[derive(Clone, Copy, Debug)]
pub struct UserFlowParams {
    pub pool: Pubkey,
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    /// UI units minted to the user before the flow begins.
    pub funding_a: Decimal,
    /// UI units minted to the user before the flow begins.
    pub funding_b: Decimal,
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
pub struct PoolCreationOutcome {
    pub create_pool: CreatePoolOutcome,
    pub tick_arrays: TickArraySeedOutcome,
}

#[derive(Clone, Debug)]
pub struct UserFlowOutcome {
    pub funding: FundingOutcome,
    pub open_position: PositionOutcome,
    pub increase_liquidity: PositionOutcome,
    pub swap_exact_in: SwapOutcome,
    pub swap_exact_out: SwapOutcome,
    pub decrease_liquidity: PositionOutcome,
    pub collect_position: CollectOutcome,
    pub close_position: ClosePositionOutcome,
}

#[derive(Clone)]
pub struct OrcaWhirlpoolOrchestrator {
    chain: ChainClient,
    client: OrcaWhirlpoolClient,
    funder: TestTokenFunder,
    program_id: Pubkey,
    payer: Pubkey,
}

impl OrcaWhirlpoolOrchestrator {
    pub fn new(
        chain: ChainClient,
        program_id: Pubkey,
        payer: Arc<dyn Signer + Send + Sync>,
        mint_authority: Arc<dyn Signer + Send + Sync>,
        config: OrchestrationConfig,
    ) -> Result<Self> {
        let client = OrcaWhirlpoolClient::new(
            chain.clone(),
            program_id,
            Arc::clone(&payer),
            config.slippage_bps,
            config.compute_unit_limit,
            config.priority_fee_micro_lamports,
            config.confirmation_commitment,
        )?;
        let funder = TestTokenFunder::new(
            chain.clone(),
            Arc::clone(&payer),
            mint_authority,
            SubmissionConfig {
                compute_unit_limit: config.compute_unit_limit,
                compute_unit_price_micro_lamports: config.priority_fee_micro_lamports,
                commitment: config.confirmation_commitment,
                ..SubmissionConfig::default()
            },
        );
        Ok(Self {
            chain,
            client,
            funder,
            program_id,
            payer: payer.pubkey(),
        })
    }

    pub fn payer(&self) -> Pubkey {
        self.payer
    }

    /// Create one Whirlpool and seed its tick arrays. Pool creation is
    /// intentionally non-idempotent; tick-array seeding is idempotent.
    pub async fn create_pool_flow(
        &self,
        params: PoolCreationParams,
    ) -> Result<PoolCreationOutcome> {
        validate_pool_creation_params(params)?;
        let span = info_span!(
            "orca_whirlpool_pool_creation",
            whirlpools_config = %params.whirlpools_config,
            mint_a = %params.mint_a,
            mint_b = %params.mint_b,
            payer = %self.payer(),
        );
        // `.instrument` rather than `span.enter()`: an entered guard held across
        // `.await` attributes other tasks' events to this span.
        async {
        self.validate_program().await?;
        let create_pool = self
            .client
            .create_pool(CreatePoolParams {
                whirlpools_config: params.whirlpools_config,
                mint_a: params.mint_a,
                mint_b: params.mint_b,
                tick_spacing: params.tick_spacing,
                initial_price: params.initial_price,
            })
            .await?;
        let (lower, upper) = normalized_range(
            params.mint_a,
            params.mint_b,
            params.seed_lower_price,
            params.seed_upper_price,
        )?;
        let tick_arrays = self
            .client
            .ensure_tick_arrays(create_pool.accounts.pool, lower, upper)
            .await?;
        info!(pool = %create_pool.accounts.pool, seeded = tick_arrays.initialized.len(), "Orca Whirlpool created");
        Ok(PoolCreationOutcome {
            create_pool,
            tick_arrays,
        })
        }
        .instrument(span)
        .await
    }

    /// Fund the user, exercise an existing pool, then withdraw, collect, and
    /// close the user's position NFT. This flow never creates a pool.
    pub async fn run_user_flow(&self, params: UserFlowParams) -> Result<UserFlowOutcome> {
        validate_user_flow_params(params)?;
        let span = info_span!(
            "orca_whirlpool_user_flow",
            pool = %params.pool,
            mint_a = %params.mint_a,
            mint_b = %params.mint_b,
            payer = %self.payer(),
        );
        async {
            self.validate_program().await?;
            let funding = self
                .fund_payer(
                    params.mint_a,
                    params.funding_a,
                    params.mint_b,
                    params.funding_b,
                )
                .await?;
            let (lower_price, upper_price) = normalized_range(
                params.mint_a,
                params.mint_b,
                params.lower_price,
                params.upper_price,
            )?;
            let open_position = self
                .client
                .open_position(OpenPositionParams {
                    pool: params.pool,
                    lower_price,
                    upper_price,
                    input: TokenAmount {
                        mint: params.mint_a,
                        amount: params.open_amount,
                    },
                    with_metadata: params.with_metadata,
                })
                .await?;
            let position_mint = open_position.accounts.position_mint;
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
                    pool: params.pool,
                    input_mint: params.mint_a,
                    output_mint: params.mint_b,
                    amount_in: params.exact_in_amount,
                })
                .await?;
            let swap_exact_out = self
                .client
                .swap_exact_out(SwapExactOutParams {
                    pool: params.pool,
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

            info!(pool = %params.pool, %position_mint, "Orca Whirlpool user flow complete");
            Ok(UserFlowOutcome {
                funding,
                open_position,
                increase_liquidity,
                swap_exact_in,
                swap_exact_out,
                decrease_liquidity,
                collect_position,
                close_position,
            })
        }
        .instrument(span)
        .await
    }

    pub async fn fund_payer(
        &self,
        mint_a: Pubkey,
        amount_a: Decimal,
        mint_b: Pubkey,
        amount_b: Decimal,
    ) -> Result<FundingOutcome> {
        let funded = self
            .funder
            .fund(&[
                Grant {
                    mint: mint_a,
                    amount: amount_a,
                },
                Grant {
                    mint: mint_b,
                    amount: amount_b,
                },
            ])
            .await?;
        Ok(FundingOutcome {
            transaction: TransactionOutcome {
                signature: funded.transaction.signature,
                confirmation_slot: funded.transaction.confirmation_slot,
                simulated_compute_units: funded.transaction.simulated_compute_units,
            },
            token_account_a: funded.grants[0].token_account,
            token_account_b: funded.grants[1].token_account,
            amount_a_raw: funded.grants[0].amount_raw,
            amount_b_raw: funded.grants[1].amount_raw,
        })
    }

    async fn validate_program(&self) -> Result<()> {
        let program = self
            .chain
            .rpc()
            .get_multiple_accounts(&[self.program_id])
            .await
            .map_err(|error| OrchestrationError::Rpc(error.to_string()))?
            .into_iter()
            .next()
            .flatten()
            .ok_or(OrchestrationError::MissingProgram(self.program_id))?;
        if !program.executable {
            return Err(OrchestrationError::InvalidProgram(self.program_id));
        }
        Ok(())
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

fn validate_pool_creation_params(params: PoolCreationParams) -> Result<()> {
    if params.mint_a == params.mint_b {
        return Err(OrchestrationError::InvalidInput(
            "mint_a and mint_b must differ".to_owned(),
        ));
    }
    for (name, value) in [
        ("initial_price", params.initial_price),
        ("seed_lower_price", params.seed_lower_price),
        ("seed_upper_price", params.seed_upper_price),
    ] {
        ensure_positive(value, name)?;
    }
    if params.seed_lower_price >= params.seed_upper_price {
        return Err(OrchestrationError::InvalidInput(
            "seed_lower_price must be less than seed_upper_price".to_owned(),
        ));
    }
    Ok(())
}

fn validate_user_flow_params(params: UserFlowParams) -> Result<()> {
    if params.mint_a == params.mint_b {
        return Err(OrchestrationError::InvalidInput(
            "mint_a and mint_b must differ".to_owned(),
        ));
    }
    for (name, value) in [
        ("funding_a", params.funding_a),
        ("funding_b", params.funding_b),
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

#[derive(Debug, Error)]
pub enum OrchestrationError {
    #[error("invalid orchestration input: {0}")]
    InvalidInput(String),
    #[error("Whirlpool program account {0} does not exist")]
    MissingProgram(Pubkey),
    #[error("Whirlpool program account {0} is not executable")]
    InvalidProgram(Pubkey),
    #[error("RPC request failed: {0}")]
    Rpc(String),
    #[error(transparent)]
    Funding(#[from] FundingError),
    #[error(transparent)]
    Orca(#[from] OrcaWhirlpoolError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_inverts_when_mints_are_reversed() {
        let low = Pubkey::new_from_array([1; 32]);
        let high = Pubkey::new_from_array([2; 32]);
        let (lower, upper) =
            normalized_range(high, low, Decimal::new(5, 1), Decimal::TWO).expect("inverts");
        assert_eq!((lower, upper), (Decimal::new(5, 1), Decimal::TWO));
        let (lower, upper) =
            normalized_range(low, high, Decimal::new(25, 2), Decimal::TWO).expect("passes");
        assert_eq!((lower, upper), (Decimal::new(25, 2), Decimal::TWO));
    }
}
