//! High-level async actions for a single Raydium concentrated-liquidity pool.
//!
//! Inputs and outputs use the workspace's Agave 4.x modular types. The pinned
//! Raydium interface remains behind a private Solana 2.x adapter.

mod adapter;
mod quote;

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use adapter::{current_pubkey, instruction, legacy_pubkey};
use anchor_lang::prelude::AccountMeta as LegacyAccountMeta;
use anchor_lang::solana_program::instruction::Instruction as LegacyInstruction;
use anchor_lang::{AccountDeserialize, InstructionData, ToAccountMetas};
use metrics::{counter, histogram};
use mtm_chain::{ChainClient, SubmissionConfig, SubmissionError};
use raydium_clmm_interface::libraries::{fixed_point_64, liquidity_math, tick_math};
use raydium_clmm_interface::states::{
    self, AmmConfig, PersonalPositionState, PoolState, TickArrayBitmapExtension, TickArrayState,
};
use rust_decimal::Decimal;
use rust_decimal::prelude::{FromPrimitive, MathematicalOps, ToPrimitive};
use solana_account::Account;
use solana_commitment_config::CommitmentConfig;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_signer::Signer;
use spl_token_2022::extension::transfer_fee::TransferFeeConfig;
use spl_token_2022::extension::{BaseStateWithExtensions, ExtensionType, StateWithExtensions};
use spl_token_2022::state::{Account as TokenAccount, AccountState, Mint};
use thiserror::Error;
use tracing::{Instrument, error, info, info_span};

const MAX_SWAP_TICK_ARRAYS: usize = 6;
const TICK_ARRAY_DISCOVERY_LIMIT: usize = 24;

pub type Result<T> = std::result::Result<T, RaydiumClmmError>;

/// Derive the canonical pool address for a config and an unordered mint pair.
pub fn derive_pool_address(
    program_id: Pubkey,
    amm_config: Pubkey,
    mint_a: Pubkey,
    mint_b: Pubkey,
) -> Result<Pubkey> {
    let (mint_0, mint_1) = if mint_a < mint_b {
        (mint_a, mint_b)
    } else if mint_a > mint_b {
        (mint_b, mint_a)
    } else {
        return Err(RaydiumClmmError::InvalidInput(
            "pool mints must be different".to_owned(),
        ));
    };
    Ok(pool_address_for(program_id, amm_config, mint_0, mint_1))
}

#[derive(Clone)]
pub struct RaydiumClmmClient {
    chain: ChainClient,
    program_id: Pubkey,
    payer: Arc<dyn Signer + Send + Sync>,
    slippage_bps: u16,
    submission: SubmissionConfig,
}

impl RaydiumClmmClient {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chain: ChainClient,
        program_id: Pubkey,
        payer: Arc<dyn Signer + Send + Sync>,
        slippage_bps: u16,
        compute_unit_limit: u32,
        priority_fee_micro_lamports: u64,
        confirmation_commitment: CommitmentConfig,
    ) -> Result<Self> {
        if slippage_bps > 10_000 {
            return Err(RaydiumClmmError::InvalidInput(
                "slippage_bps cannot exceed 10,000".to_owned(),
            ));
        }
        if compute_unit_limit == 0 {
            return Err(RaydiumClmmError::InvalidInput(
                "compute_unit_limit must be greater than zero".to_owned(),
            ));
        }
        Ok(Self {
            chain,
            program_id,
            payer,
            slippage_bps,
            submission: SubmissionConfig {
                compute_unit_limit,
                compute_unit_price_micro_lamports: priority_fee_micro_lamports,
                commitment: confirmation_commitment,
                ..SubmissionConfig::default()
            },
        })
    }

    pub fn payer(&self) -> Pubkey {
        self.payer.pubkey()
    }

    /// Create the canonical AMM config, or validate an identical existing config.
    ///
    /// The injected payer must match the admin public key compiled into the
    /// deployed Raydium program when creation is required.
    pub async fn ensure_amm_config(
        &self,
        params: EnsureAmmConfigParams,
    ) -> Result<EnsureAmmConfigOutcome> {
        let config_address = self.amm_config_address(params.index);
        self.run("ensure_amm_config", config_address, async move {
            validate_amm_config_params(params)?;
            let accounts = self.fetch_optional(&[self.program_id, config_address]).await?;
            let program = require_account(
                &[self.program_id, config_address],
                &accounts,
                0,
                "Raydium program",
            )?;
            if !program.executable {
                return Err(RaydiumClmmError::MalformedAccount {
                    address: self.program_id,
                    reason: "Raydium program account is not executable".to_owned(),
                });
            }
            if let Some(account) = accounts[1].as_ref() {
                let config: AmmConfig = self.decode_program_account(config_address, account)?;
                let owner = current_pubkey(config.owner);
                let fund_owner = current_pubkey(config.fund_owner);
                if config.index != params.index
                    || config.tick_spacing != params.tick_spacing
                    || config.trade_fee_rate != params.trade_fee_rate
                    || config.protocol_fee_rate != params.protocol_fee_rate
                    || config.fund_fee_rate != params.fund_fee_rate
                    || owner != self.payer()
                    || fund_owner != self.payer()
                {
                    return Err(RaydiumClmmError::InvalidInput(format!(
                        "existing AMM config {config_address} does not match the requested admin or fee settings"
                    )));
                }
                return Ok(EnsureAmmConfigOutcome {
                    transaction: None,
                    created: false,
                    amm_config: config_address,
                    admin: owner,
                    params,
                });
            }

            let ix = self.raydium_instruction(
                raydium_clmm_interface::accounts::CreateAmmConfig {
                    owner: legacy_pubkey(self.payer()),
                    amm_config: legacy_pubkey(config_address),
                    system_program: anchor_lang::system_program::ID,
                },
                raydium_clmm_interface::instruction::CreateAmmConfig {
                    index: params.index,
                    tick_spacing: params.tick_spacing,
                    trade_fee_rate: params.trade_fee_rate,
                    protocol_fee_rate: params.protocol_fee_rate,
                    fund_fee_rate: params.fund_fee_rate,
                },
                vec![],
            );
            let transaction = self.submit(vec![ix], &[]).await?;
            Ok(EnsureAmmConfigOutcome {
                transaction: Some(transaction),
                created: true,
                amm_config: config_address,
                admin: self.payer(),
                params,
            })
        })
        .await
    }

    pub async fn create_pool(&self, params: CreatePoolParams) -> Result<CreatePoolOutcome> {
        self.run("create_pool", params.amm_config, async move {
            ensure_positive(params.initial_price, "initial_price")?;
            let open_time = params.open_time.unwrap_or(0);
            let (mint_0, mint_1, normalized_price) = if params.mint_a < params.mint_b {
                (params.mint_a, params.mint_b, params.initial_price)
            } else if params.mint_a > params.mint_b {
                (
                    params.mint_b,
                    params.mint_a,
                    Decimal::ONE
                        .checked_div(params.initial_price)
                        .ok_or_else(|| {
                            RaydiumClmmError::Quote("price inversion failed".to_owned())
                        })?,
                )
            } else {
                return Err(RaydiumClmmError::InvalidInput(
                    "pool mints must be different".to_owned(),
                ));
            };
            let pool = self.pool_address(params.amm_config, mint_0, mint_1);
            let support_0 = self.support_mint_address(mint_0);
            let support_1 = self.support_mint_address(mint_1);
            let keys = [
                params.amm_config,
                mint_0,
                mint_1,
                pool,
                support_0,
                support_1,
            ];
            let accounts = self.fetch_optional(&keys).await?;
            let config_account = require_account(&keys, &accounts, 0, "amm config")?;
            let config: AmmConfig =
                self.decode_program_account(params.amm_config, config_account)?;
            let expected_config = pda(
                &[
                    states::AMM_CONFIG_SEED.as_bytes(),
                    &config.index.to_be_bytes(),
                ],
                self.program_id,
            );
            if expected_config != params.amm_config {
                return Err(RaydiumClmmError::MalformedAccount {
                    address: params.amm_config,
                    reason: "AMM config is not its canonical PDA".to_owned(),
                });
            }
            let mint_info_0 =
                self.decode_mint(mint_0, require_account(&keys, &accounts, 1, "mint 0")?)?;
            let mint_info_1 =
                self.decode_mint(mint_1, require_account(&keys, &accounts, 2, "mint 1")?)?;
            if accounts[3].is_some() {
                return Err(RaydiumClmmError::InvalidInput(format!(
                    "pool {pool} is already initialized"
                )));
            }
            self.validate_supported_mint(&mint_info_0, accounts[4].as_ref())?;
            self.validate_supported_mint(&mint_info_1, accounts[5].as_ref())?;

            let sqrt_price_x64 =
                price_to_sqrt_x64(normalized_price, mint_info_0.decimals, mint_info_1.decimals)?;
            tick_math::get_tick_at_sqrt_price(sqrt_price_x64)
                .map_err(|error| RaydiumClmmError::Quote(error.to_string()))?;
            let addresses = self.derive_pool_addresses(pool, mint_0, mint_1);
            let mut remaining = Vec::new();
            if accounts[4].is_some() {
                remaining.push(LegacyAccountMeta::new_readonly(
                    legacy_pubkey(support_0),
                    false,
                ));
            }
            if accounts[5].is_some() {
                remaining.push(LegacyAccountMeta::new_readonly(
                    legacy_pubkey(support_1),
                    false,
                ));
            }
            let ix = self.raydium_instruction(
                raydium_clmm_interface::accounts::CreatePool {
                    pool_creator: legacy_pubkey(self.payer()),
                    amm_config: legacy_pubkey(params.amm_config),
                    pool_state: legacy_pubkey(pool),
                    token_mint_0: legacy_pubkey(mint_0),
                    token_mint_1: legacy_pubkey(mint_1),
                    token_vault_0: legacy_pubkey(addresses.token_vault_0),
                    token_vault_1: legacy_pubkey(addresses.token_vault_1),
                    observation_state: legacy_pubkey(addresses.observation),
                    tick_array_bitmap: legacy_pubkey(addresses.tick_array_bitmap),
                    token_program_0: legacy_pubkey(mint_info_0.token_program),
                    token_program_1: legacy_pubkey(mint_info_1.token_program),
                    system_program: anchor_lang::system_program::ID,
                    rent: anchor_lang::solana_program::sysvar::rent::ID,
                },
                raydium_clmm_interface::instruction::CreatePool {
                    sqrt_price_x64,
                    open_time,
                },
                remaining,
            );
            let transaction = self.submit(vec![ix], &[]).await?;
            Ok(CreatePoolOutcome {
                transaction,
                quote: PoolCreationQuote {
                    mint_0,
                    mint_1,
                    normalized_initial_price: normalized_price,
                    sqrt_price_x64,
                    open_time,
                },
                accounts: addresses,
            })
        })
        .await
    }

    pub async fn open_position(&self, params: OpenPositionParams) -> Result<PositionOutcome> {
        self.run("open_position", params.pool, async move {
            let bundle = self.load_pool(params.pool).await?;
            let quote = self.position_quote(
                &bundle,
                params.lower_price,
                params.upper_price,
                params.input,
            )?;
            let mut instructions = self.position_ata_instructions(&bundle);
            self.ensure_position_balances(&bundle, &quote)?;
            let nft_mint = Keypair::new();
            let position_nft_mint = nft_mint.pubkey();
            let accounts = self.position_addresses(
                params.pool,
                position_nft_mint,
                quote.tick_lower,
                quote.tick_upper,
                bundle.tick_spacing,
            );
            self.validate_position_tick_arrays(&accounts, false).await?;
            let remaining = vec![LegacyAccountMeta::new(
                legacy_pubkey(bundle.tick_array_bitmap),
                false,
            )];
            let ix = self.raydium_instruction(
                raydium_clmm_interface::accounts::OpenPositionWithToken22Nft {
                    payer: legacy_pubkey(self.payer()),
                    position_nft_owner: legacy_pubkey(self.payer()),
                    position_nft_mint: legacy_pubkey(position_nft_mint),
                    position_nft_account: legacy_pubkey(accounts.position_nft_account),
                    pool_state: legacy_pubkey(params.pool),
                    protocol_position: legacy_pubkey(accounts.protocol_position),
                    tick_array_lower: legacy_pubkey(accounts.tick_array_lower),
                    tick_array_upper: legacy_pubkey(accounts.tick_array_upper),
                    personal_position: legacy_pubkey(accounts.personal_position),
                    token_account_0: legacy_pubkey(bundle.user_token_0),
                    token_account_1: legacy_pubkey(bundle.user_token_1),
                    token_vault_0: legacy_pubkey(bundle.token_vault_0),
                    token_vault_1: legacy_pubkey(bundle.token_vault_1),
                    rent: anchor_lang::solana_program::sysvar::rent::ID,
                    system_program: anchor_lang::system_program::ID,
                    token_program: spl_token::id(),
                    associated_token_program: spl_associated_token_account::id(),
                    token_program_2022: spl_token_2022::id(),
                    vault_0_mint: legacy_pubkey(bundle.mint_0.key),
                    vault_1_mint: legacy_pubkey(bundle.mint_1.key),
                },
                raydium_clmm_interface::instruction::OpenPositionWithToken22Nft {
                    liquidity: quote.liquidity,
                    amount_0_max: quote.amount_0_max,
                    amount_1_max: quote.amount_1_max,
                    tick_lower_index: quote.tick_lower,
                    tick_upper_index: quote.tick_upper,
                    tick_array_lower_start_index: accounts.tick_array_lower_start,
                    tick_array_upper_start_index: accounts.tick_array_upper_start,
                    with_metadata: params.with_metadata,
                    base_flag: None,
                },
                remaining,
            );
            instructions.push(ix);
            let transaction = self.submit(instructions, &[&nft_mint]).await?;
            Ok(PositionOutcome {
                transaction,
                quote,
                accounts,
            })
        })
        .await
    }

    pub async fn increase_liquidity(
        &self,
        params: IncreaseLiquidityParams,
    ) -> Result<PositionOutcome> {
        self.run("increase_liquidity", params.position_mint, async move {
            let position = self.load_position(params.position_mint).await?;
            let bundle = self.load_pool(position.pool).await?;
            let quote = self.position_quote_for_ticks(&bundle, &position, params.input)?;
            self.ensure_position_balances(&bundle, &quote)?;
            let mut instructions = self.position_ata_instructions(&bundle);
            let accounts = self.position_addresses(
                position.pool,
                params.position_mint,
                position.tick_lower,
                position.tick_upper,
                bundle.tick_spacing,
            );
            self.validate_position_tick_arrays(&accounts, true).await?;
            let remaining = vec![LegacyAccountMeta::new(
                legacy_pubkey(bundle.tick_array_bitmap),
                false,
            )];
            let ix = self.raydium_instruction(
                raydium_clmm_interface::accounts::IncreaseLiquidityV2 {
                    nft_owner: legacy_pubkey(self.payer()),
                    nft_account: legacy_pubkey(position.nft_account),
                    pool_state: legacy_pubkey(position.pool),
                    protocol_position: legacy_pubkey(accounts.protocol_position),
                    personal_position: legacy_pubkey(position.address),
                    tick_array_lower: legacy_pubkey(accounts.tick_array_lower),
                    tick_array_upper: legacy_pubkey(accounts.tick_array_upper),
                    token_account_0: legacy_pubkey(bundle.user_token_0),
                    token_account_1: legacy_pubkey(bundle.user_token_1),
                    token_vault_0: legacy_pubkey(bundle.token_vault_0),
                    token_vault_1: legacy_pubkey(bundle.token_vault_1),
                    token_program: spl_token::id(),
                    token_program_2022: spl_token_2022::id(),
                    vault_0_mint: legacy_pubkey(bundle.mint_0.key),
                    vault_1_mint: legacy_pubkey(bundle.mint_1.key),
                },
                raydium_clmm_interface::instruction::IncreaseLiquidityV2 {
                    liquidity: quote.liquidity,
                    amount_0_max: quote.amount_0_max,
                    amount_1_max: quote.amount_1_max,
                    base_flag: None,
                },
                remaining,
            );
            instructions.push(ix);
            let transaction = self.submit(instructions, &[]).await?;
            Ok(PositionOutcome {
                transaction,
                quote,
                accounts,
            })
        })
        .await
    }

    pub async fn decrease_liquidity(
        &self,
        params: DecreaseLiquidityParams,
    ) -> Result<PositionOutcome> {
        self.run("decrease_liquidity", params.position_mint, async move {
            let position = self.load_position(params.position_mint).await?;
            let bundle = self.load_pool(position.pool).await?;
            let liquidity = match params.amount {
                Some(amount) => self
                    .position_quote_for_ticks(&bundle, &position, amount)?
                    .liquidity,
                None => position.liquidity,
            };
            if liquidity == 0 || liquidity > position.liquidity {
                return Err(RaydiumClmmError::InvalidInput(
                    "decrease amount must resolve to non-zero liquidity no greater than the position"
                        .to_owned(),
                ));
            }
            let quote = self.decrease_quote(&bundle, &position, liquidity)?;
            let accounts = self.position_addresses(
                position.pool,
                params.position_mint,
                position.tick_lower,
                position.tick_upper,
                bundle.tick_spacing,
            );
            self.validate_position_tick_arrays(&accounts, true).await?;
            let mut instructions = self.position_ata_instructions(&bundle);
            let (mut reward_instructions, remaining) = self.reward_accounts(&bundle).await?;
            instructions.append(&mut reward_instructions);
            instructions.push(self.decrease_instruction(
                &bundle,
                &position,
                &accounts,
                &quote,
                remaining,
            ));
            let transaction = self.submit(instructions, &[]).await?;
            Ok(PositionOutcome {
                transaction,
                quote,
                accounts,
            })
        })
        .await
    }

    /// Collect trading fees and initialized rewards using Raydium's supported
    /// zero-liquidity decrease operation.
    pub async fn collect_position(&self, position_mint: Pubkey) -> Result<CollectOutcome> {
        self.run("collect_position", position_mint, async move {
            let position = self.load_position(position_mint).await?;
            let bundle = self.load_pool(position.pool).await?;
            let accounts = self.position_addresses(
                position.pool,
                position_mint,
                position.tick_lower,
                position.tick_upper,
                bundle.tick_spacing,
            );
            self.validate_position_tick_arrays(&accounts, true).await?;
            let quote = self.decrease_quote(&bundle, &position, 0)?;
            let mut instructions = self.position_ata_instructions(&bundle);
            let (mut reward_instructions, remaining) = self.reward_accounts(&bundle).await?;
            instructions.append(&mut reward_instructions);
            instructions
                .push(self.decrease_instruction(&bundle, &position, &accounts, &quote, remaining));
            let transaction = self.submit(instructions, &[]).await?;
            Ok(CollectOutcome {
                transaction,
                position: accounts,
            })
        })
        .await
    }

    /// Token amounts the payer's position would return if fully withdrawn
    /// now (at the current pool price), plus fees already owed.
    pub async fn position_value(&self, position_mint: Pubkey) -> Result<PositionValue> {
        let position = self.load_position(position_mint).await?;
        let bundle = self.load_pool(position.pool).await?;
        let (amount_0, amount_1) = if position.liquidity == 0 {
            (0, 0)
        } else {
            let quote = self.decrease_quote(&bundle, &position, position.liquidity)?;
            (quote.amount_0_raw, quote.amount_1_raw)
        };
        // Fees earned since the position's last update live in the pool's
        // fee growth, not in `fees_owed` yet.
        let spacing = bundle.pool.tick_spacing;
        let ticks = [position.tick_lower, position.tick_upper];
        let keys = ticks.map(|tick| {
            self.tick_array_address(
                position.pool,
                TickArrayState::get_array_start_index(tick, spacing),
            )
        });
        let arrays = self.fetch_optional(&keys).await?;
        let mut outside = [(0_u128, 0_u128); 2];
        for (index, tick) in ticks.into_iter().enumerate() {
            let array: TickArrayState = self.decode_program_account(
                keys[index],
                require_account(&keys, &arrays, index, "position tick array")?,
            )?;
            let state = array.get_tick_state(tick, spacing).map_err(|error| {
                RaydiumClmmError::Quote(format!("position tick {tick}: {error}"))
            })?;
            outside[index] = (
                state.fee_growth_outside_0_x64,
                state.fee_growth_outside_1_x64,
            );
        }
        let pool = &bundle.pool;
        let accrued = |global, lower, upper, checkpoint| {
            mtm_math::clmm::fees_accrued(
                position.liquidity,
                global,
                lower,
                upper,
                pool.tick_current,
                position.tick_lower,
                position.tick_upper,
                checkpoint,
            )
        };
        let fees_0 = accrued(
            pool.fee_growth_global_0_x64,
            outside[0].0,
            outside[1].0,
            position.fee_checkpoint.0,
        );
        let fees_1 = accrued(
            pool.fee_growth_global_1_x64,
            outside[0].1,
            outside[1].1,
            position.fee_checkpoint.1,
        );
        Ok(PositionValue {
            pool: position.pool,
            mint_a: bundle.mint_0.key,
            mint_b: bundle.mint_1.key,
            amount_a: amount_0
                .saturating_add(position.fees_owed_0)
                .saturating_add(fees_0),
            amount_b: amount_1
                .saturating_add(position.fees_owed_1)
                .saturating_add(fees_1),
            liquidity: position.liquidity,
        })
    }

    pub async fn close_position(&self, position_mint: Pubkey) -> Result<ClosePositionOutcome> {
        self.run("close_position", position_mint, async move {
            let position = self.load_position(position_mint).await?;
            if position.liquidity != 0
                || position.fees_owed_0 != 0
                || position.fees_owed_1 != 0
                || position.rewards_owed.iter().any(|amount| *amount != 0)
            {
                return Err(RaydiumClmmError::InvalidInput(
                    "position must have zero liquidity, fees, and rewards before close".to_owned(),
                ));
            }
            let remaining = vec![LegacyAccountMeta::new(legacy_pubkey(position.pool), false)];
            let ix = self.raydium_instruction(
                raydium_clmm_interface::accounts::ClosePosition {
                    nft_owner: legacy_pubkey(self.payer()),
                    position_nft_mint: legacy_pubkey(position_mint),
                    position_nft_account: legacy_pubkey(position.nft_account),
                    personal_position: legacy_pubkey(position.address),
                    system_program: anchor_lang::system_program::ID,
                    token_program: legacy_pubkey(position.nft_token_program),
                },
                raydium_clmm_interface::instruction::ClosePosition,
                remaining,
            );
            let transaction = self.submit(vec![ix], &[]).await?;
            Ok(ClosePositionOutcome {
                transaction,
                closed_position: position.address,
                closed_nft_mint: position_mint,
                closed_nft_account: position.nft_account,
                pool: position.pool,
            })
        })
        .await
    }

    pub async fn swap_exact_in(&self, params: SwapExactInParams) -> Result<SwapOutcome> {
        self.swap(
            params.pool,
            params.input_mint,
            params.output_mint,
            params.amount_in,
            true,
        )
        .await
    }

    pub async fn swap_exact_out(&self, params: SwapExactOutParams) -> Result<SwapOutcome> {
        self.swap(
            params.pool,
            params.input_mint,
            params.output_mint,
            params.amount_out,
            false,
        )
        .await
    }

    /// Fetch everything a swap on `pool` needs, in both directions. Quoting
    /// and instruction building on the snapshot are pure, so callers (the
    /// router) can price many sizes and build for any owner from one fetch.
    pub async fn load_swap_snapshot(&self, pool: Pubkey) -> Result<SwapSnapshot> {
        let bundle = self.load_pool(pool).await?;
        let config: AmmConfig =
            self.decode_program_account(bundle.amm_config, &bundle.config_account)?;
        if bundle.pool.get_dynamic_fee_info().is_some() {
            return Err(RaydiumClmmError::Quote(
                "dynamic-fee pools are outside this single-pool quote implementation".to_owned(),
            ));
        }
        let zero_for_one = self
            .load_swap_tick_arrays(&bundle, true)
            .await
            .map_err(|error| error.to_string());
        let one_for_zero = self
            .load_swap_tick_arrays(&bundle, false)
            .await
            .map_err(|error| error.to_string());
        Ok(SwapSnapshot {
            program_id: self.program_id,
            bundle,
            config,
            zero_for_one,
            one_for_zero,
        })
    }

    async fn swap(
        &self,
        pool_key: Pubkey,
        input_mint: Pubkey,
        output_mint: Pubkey,
        amount: Decimal,
        exact_in: bool,
    ) -> Result<SwapOutcome> {
        let action = if exact_in {
            "swap_exact_in"
        } else {
            "swap_exact_out"
        };
        self.run(action, pool_key, async move {
            ensure_positive(amount, "swap amount")?;
            let snapshot = self.load_swap_snapshot(pool_key).await?;
            let (input, output) = snapshot.direction(input_mint, output_mint)?.1;
            let requested_raw = ui_to_raw(
                amount,
                if exact_in {
                    input.decimals
                } else {
                    output.decimals
                },
            )?;
            let quote = snapshot.quote(
                input_mint,
                output_mint,
                requested_raw,
                exact_in,
                self.slippage_bps,
            )?;
            let input_ata = associated_token_address(self.payer(), input.key, input.token_program);
            let output_ata =
                associated_token_address(self.payer(), output.key, output.token_program);
            let ata_accounts = self.fetch_optional(&[input_ata, output_ata]).await?;
            let input_balance =
                self.validate_token_account(input_ata, ata_accounts[0].as_ref(), input, true)?;
            let required = quote.max_input_raw();
            if input_balance < required {
                return Err(RaydiumClmmError::InsufficientBalance {
                    mint: input.key,
                    required,
                    available: input_balance,
                });
            }
            self.validate_token_account(output_ata, ata_accounts[1].as_ref(), output, false)?;
            let instructions = snapshot.swap_instructions(self.payer(), &quote)?;
            let accounts = snapshot.swap_accounts(self.payer(), &quote)?;
            let transaction = self.submit(instructions, &[]).await?;
            Ok(SwapOutcome {
                transaction,
                quote,
                accounts,
            })
        })
        .await
    }

    async fn run<T, F>(&self, action: &'static str, target: Pubkey, future: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        let started = Instant::now();
        let span = info_span!("raydium_clmm_action", action, %target);
        let result = future.instrument(span).await;
        let status = if result.is_ok() { "confirmed" } else { "error" };
        counter!("raydium_clmm_actions_total", "action" => action, "status" => status).increment(1);
        histogram!("raydium_clmm_action_latency_seconds", "action" => action)
            .record(started.elapsed().as_secs_f64());
        match &result {
            Ok(_) => {
                info!(action, %target, latency_ms = started.elapsed().as_millis(), "Raydium action complete")
            }
            Err(problem) => {
                error!(action, %target, error = %problem, latency_ms = started.elapsed().as_millis(), "Raydium action failed")
            }
        }
        result
    }

    async fn submit(
        &self,
        instructions: Vec<Instruction>,
        additional_signers: &[&dyn Signer],
    ) -> Result<TransactionOutcome> {
        match self
            .chain
            .submit(
                instructions,
                self.payer.as_ref(),
                additional_signers,
                self.submission,
            )
            .await
        {
            Ok(outcome) => {
                histogram!("raydium_clmm_simulated_compute_units")
                    .record(outcome.simulated_compute_units as f64);
                info!(
                    signature = %outcome.signature,
                    slot = outcome.confirmation_slot,
                    compute_units = outcome.simulated_compute_units,
                    "Raydium transaction confirmed"
                );
                Ok(TransactionOutcome {
                    signature: outcome.signature,
                    confirmation_slot: outcome.confirmation_slot,
                    simulated_compute_units: outcome.simulated_compute_units,
                })
            }
            Err(SubmissionError::Simulation { error, logs }) => {
                Err(RaydiumClmmError::SimulationFailure { error, logs })
            }
            Err(SubmissionError::SimulationRpc(error)) => {
                Err(RaydiumClmmError::SimulationFailure {
                    error,
                    logs: vec![],
                })
            }
            Err(SubmissionError::AmbiguousConfirmation { signature, reason }) => {
                Err(RaydiumClmmError::AmbiguousConfirmation { signature, reason })
            }
            Err(error) => Err(RaydiumClmmError::SubmissionFailure(error.to_string())),
        }
    }

    fn raydium_instruction<A, I>(
        &self,
        accounts: A,
        args: I,
        remaining: Vec<LegacyAccountMeta>,
    ) -> Instruction
    where
        A: ToAccountMetas,
        I: InstructionData,
    {
        let mut metas = accounts.to_account_metas(None);
        metas.extend(remaining);
        instruction(LegacyInstruction {
            program_id: legacy_pubkey(self.program_id),
            accounts: metas,
            data: args.data(),
        })
    }

    async fn fetch_optional(&self, keys: &[Pubkey]) -> Result<Vec<Option<Account>>> {
        self.chain
            .rpc()
            .get_multiple_accounts(keys)
            .await
            .map_err(|error| RaydiumClmmError::Rpc(error.to_string()))
    }

    fn decode_program_account<T: AccountDeserialize>(
        &self,
        key: Pubkey,
        account: &Account,
    ) -> Result<T> {
        if account.owner != self.program_id {
            return Err(RaydiumClmmError::MalformedAccount {
                address: key,
                reason: format!("expected owner {}, got {}", self.program_id, account.owner),
            });
        }
        let mut data = account.data.as_slice();
        T::try_deserialize(&mut data).map_err(|error| RaydiumClmmError::MalformedAccount {
            address: key,
            reason: error.to_string(),
        })
    }

    fn decode_mint(&self, key: Pubkey, account: &Account) -> Result<MintInfo> {
        let token_program = account.owner;
        if token_program != token_program_id() && token_program != token_2022_program_id() {
            return Err(RaydiumClmmError::UnsupportedToken {
                mint: key,
                reason: format!("unsupported token program {token_program}"),
            });
        }
        let state = StateWithExtensions::<Mint>::unpack(&account.data).map_err(|error| {
            RaydiumClmmError::MalformedAccount {
                address: key,
                reason: error.to_string(),
            }
        })?;
        Ok(MintInfo {
            key,
            token_program,
            decimals: state.base.decimals,
            data: account.data.clone(),
        })
    }

    fn validate_supported_mint(&self, mint: &MintInfo, support: Option<&Account>) -> Result<()> {
        if mint.token_program == token_program_id() {
            return Ok(());
        }
        if let Some(account) = support {
            let support_key = self.support_mint_address(mint.key);
            let state: states::SupportMintAssociated =
                self.decode_program_account(support_key, account)?;
            if current_pubkey(state.mint) != mint.key {
                return Err(RaydiumClmmError::MalformedAccount {
                    address: support_key,
                    reason: "support-mint PDA contains a different mint".to_owned(),
                });
            }
            return Ok(());
        }
        let state = StateWithExtensions::<Mint>::unpack(&mint.data).map_err(|error| {
            RaydiumClmmError::MalformedAccount {
                address: mint.key,
                reason: error.to_string(),
            }
        })?;
        let allowed = [
            ExtensionType::TransferFeeConfig,
            ExtensionType::MetadataPointer,
            ExtensionType::TokenMetadata,
            ExtensionType::InterestBearingConfig,
            ExtensionType::ScaledUiAmount,
        ];
        let unsupported: Vec<String> = state
            .get_extension_types()
            .map_err(|error| RaydiumClmmError::MalformedAccount {
                address: mint.key,
                reason: error.to_string(),
            })?
            .into_iter()
            .filter(|extension| !allowed.contains(extension))
            .map(|extension| format!("{extension:?}"))
            .collect();
        if unsupported.is_empty() {
            Ok(())
        } else {
            Err(RaydiumClmmError::UnsupportedToken {
                mint: mint.key,
                reason: format!(
                    "unsupported Token-2022 extensions: {}",
                    unsupported.join(", ")
                ),
            })
        }
    }

    async fn load_pool(&self, key: Pubkey) -> Result<PoolBundle> {
        let first = self.fetch_optional(&[key]).await?;
        let account = require_account(&[key], &first, 0, "pool")?;
        let pool: PoolState = self.decode_program_account(key, account)?;
        let amm_config = current_pubkey(pool.amm_config);
        let mint_0_key = current_pubkey(pool.token_mint_0);
        let mint_1_key = current_pubkey(pool.token_mint_1);
        let token_vault_0 = current_pubkey(pool.token_vault_0);
        let token_vault_1 = current_pubkey(pool.token_vault_1);
        let observation = current_pubkey(pool.observation_key);
        let tick_array_bitmap = self.tick_array_bitmap_address(key);
        // The payer's token accounts depend on each mint's token program, which
        // is only known once the mints load: fetch both candidates in the same
        // batch (with the Clock sysvar for the transfer-fee epoch) rather than
        // in two more round trips.
        let payer_atas = [mint_0_key, mint_1_key].map(|mint| {
            [token_program_id(), token_2022_program_id()]
                .map(|program| associated_token_address(self.payer(), mint, program))
        });
        let keys = [
            amm_config,
            mint_0_key,
            mint_1_key,
            tick_array_bitmap,
            token_vault_0,
            token_vault_1,
            observation,
            clock_sysvar_id(),
            payer_atas[0][0],
            payer_atas[0][1],
            payer_atas[1][0],
            payer_atas[1][1],
        ];
        let accounts = self.fetch_optional(&keys).await?;
        let config_account = require_account(&keys, &accounts, 0, "amm config")?.clone();
        let config: AmmConfig = self.decode_program_account(amm_config, &config_account)?;
        let expected_config = pda(
            &[
                states::AMM_CONFIG_SEED.as_bytes(),
                &config.index.to_be_bytes(),
            ],
            self.program_id,
        );
        if expected_config != amm_config {
            return Err(RaydiumClmmError::MalformedAccount {
                address: amm_config,
                reason: "AMM config is not its canonical PDA".to_owned(),
            });
        }
        let tick_spacing = pool.tick_spacing;
        if config.tick_spacing != tick_spacing {
            return Err(RaydiumClmmError::MalformedAccount {
                address: key,
                reason: "pool tick spacing does not match its config".to_owned(),
            });
        }
        let mint_0 =
            self.decode_mint(mint_0_key, require_account(&keys, &accounts, 1, "mint 0")?)?;
        let mint_1 =
            self.decode_mint(mint_1_key, require_account(&keys, &accounts, 2, "mint 1")?)?;
        if mint_0_key >= mint_1_key || self.pool_address(amm_config, mint_0_key, mint_1_key) != key
        {
            return Err(RaydiumClmmError::MalformedAccount {
                address: key,
                reason: "pool mint order or canonical PDA is invalid".to_owned(),
            });
        }
        if pool.mint_decimals_0 != mint_0.decimals || pool.mint_decimals_1 != mint_1.decimals {
            return Err(RaydiumClmmError::MalformedAccount {
                address: key,
                reason: "pool mint decimals do not match mint accounts".to_owned(),
            });
        }
        let bitmap: TickArrayBitmapExtension = self.decode_program_account(
            tick_array_bitmap,
            require_account(&keys, &accounts, 3, "tick-array bitmap")?,
        )?;
        self.validate_vault(
            key,
            token_vault_0,
            require_account(&keys, &accounts, 4, "vault 0")?,
            &mint_0,
        )?;
        self.validate_vault(
            key,
            token_vault_1,
            require_account(&keys, &accounts, 5, "vault 1")?,
            &mint_1,
        )?;
        let observation_account = require_account(&keys, &accounts, 6, "observation")?;
        if observation_account.owner != self.program_id {
            return Err(RaydiumClmmError::MalformedAccount {
                address: observation,
                reason: "observation account has the wrong owner".to_owned(),
            });
        }
        let epoch = clock_epoch(require_account(&keys, &accounts, 7, "clock sysvar")?)?;
        let payer_ata = |index: usize, mint: &MintInfo| {
            let slot = 8 + 2 * index + usize::from(mint.token_program == token_2022_program_id());
            (keys[slot], accounts[slot].as_ref())
        };
        let (user_token_0, user_account_0) = payer_ata(0, &mint_0);
        let (user_token_1, user_account_1) = payer_ata(1, &mint_1);
        let balance_0 =
            self.validate_token_account(user_token_0, user_account_0, &mint_0, false)?;
        let balance_1 =
            self.validate_token_account(user_token_1, user_account_1, &mint_1, false)?;
        Ok(PoolBundle {
            pool,
            config_account,
            amm_config,
            mint_0,
            mint_1,
            token_vault_0,
            token_vault_1,
            observation,
            tick_array_bitmap,
            bitmap,
            tick_spacing,
            user_token_0,
            user_token_1,
            balance_0,
            balance_1,
            epoch,
            key,
        })
    }

    fn validate_vault(
        &self,
        pool: Pubkey,
        key: Pubkey,
        account: &Account,
        mint: &MintInfo,
    ) -> Result<()> {
        if account.owner != mint.token_program {
            return Err(RaydiumClmmError::MalformedAccount {
                address: key,
                reason: "vault is owned by the wrong token program".to_owned(),
            });
        }
        let state =
            StateWithExtensions::<TokenAccount>::unpack(&account.data).map_err(|error| {
                RaydiumClmmError::MalformedAccount {
                    address: key,
                    reason: error.to_string(),
                }
            })?;
        if current_pubkey(state.base.mint) != mint.key || current_pubkey(state.base.owner) != pool {
            return Err(RaydiumClmmError::MalformedAccount {
                address: key,
                reason: "vault mint or authority does not match the pool".to_owned(),
            });
        }
        Ok(())
    }

    fn validate_token_account(
        &self,
        key: Pubkey,
        account: Option<&Account>,
        mint: &MintInfo,
        required: bool,
    ) -> Result<u64> {
        let Some(account) = account else {
            if required {
                return Err(RaydiumClmmError::InsufficientBalance {
                    mint: mint.key,
                    required: 1,
                    available: 0,
                });
            }
            return Ok(0);
        };
        if account.owner != mint.token_program {
            return Err(RaydiumClmmError::MalformedAccount {
                address: key,
                reason: "token account is owned by the wrong token program".to_owned(),
            });
        }
        let state =
            StateWithExtensions::<TokenAccount>::unpack(&account.data).map_err(|error| {
                RaydiumClmmError::MalformedAccount {
                    address: key,
                    reason: error.to_string(),
                }
            })?;
        if current_pubkey(state.base.mint) != mint.key
            || current_pubkey(state.base.owner) != self.payer()
        {
            return Err(RaydiumClmmError::MalformedAccount {
                address: key,
                reason: "token account mint or authority is invalid".to_owned(),
            });
        }
        if state.base.state == AccountState::Frozen {
            return Err(RaydiumClmmError::MalformedAccount {
                address: key,
                reason: "fungible token account is frozen".to_owned(),
            });
        }
        Ok(state.base.amount)
    }

    /// The wallet must cover what the program will transfer at the quoted
    /// price (plus transfer fees), not the slippage-inflated maximum: the
    /// program moves the computed amount and only checks it against the max.
    fn ensure_position_balances(&self, bundle: &PoolBundle, quote: &LiquidityQuote) -> Result<()> {
        for (mint, amount, balance) in [
            (&bundle.mint_0, quote.amount_0_raw, bundle.balance_0),
            (&bundle.mint_1, quote.amount_1_raw, bundle.balance_1),
        ] {
            let required = amount
                .checked_add(inverse_transfer_fee(mint, bundle.epoch, amount)?)
                .ok_or_else(|| RaydiumClmmError::Quote("required deposit overflow".to_owned()))?;
            if balance < required {
                return Err(RaydiumClmmError::InsufficientBalance {
                    mint: mint.key,
                    required,
                    available: balance,
                });
            }
        }
        Ok(())
    }

    async fn load_position(&self, mint: Pubkey) -> Result<PositionView> {
        let address = self.personal_position_address(mint);
        let nft_token_2022 = associated_token_address(self.payer(), mint, token_2022_program_id());
        let nft_classic = associated_token_address(self.payer(), mint, token_program_id());
        let keys = [address, mint, nft_token_2022, nft_classic];
        let accounts = self.fetch_optional(&keys).await?;
        let state: PersonalPositionState = self.decode_program_account(
            address,
            require_account(&keys, &accounts, 0, "personal position")?,
        )?;
        if current_pubkey(state.nft_mint) != mint {
            return Err(RaydiumClmmError::MalformedAccount {
                address,
                reason: "personal position contains a different NFT mint".to_owned(),
            });
        }
        let mint_account = require_account(&keys, &accounts, 1, "position NFT mint")?;
        let nft_token_program = mint_account.owner;
        let nft_mint_state =
            StateWithExtensions::<Mint>::unpack(&mint_account.data).map_err(|error| {
                RaydiumClmmError::MalformedAccount {
                    address: mint,
                    reason: error.to_string(),
                }
            })?;
        if nft_mint_state.base.decimals != 0 || nft_mint_state.base.supply != 1 {
            return Err(RaydiumClmmError::MalformedAccount {
                address: mint,
                reason: "position NFT mint must have decimals 0 and supply 1".to_owned(),
            });
        }
        let (nft_account, nft_account_data) = if nft_token_program == token_2022_program_id() {
            (nft_token_2022, accounts[2].as_ref())
        } else if nft_token_program == token_program_id() {
            (nft_classic, accounts[3].as_ref())
        } else {
            return Err(RaydiumClmmError::MalformedAccount {
                address: mint,
                reason: "position NFT uses an unsupported token program".to_owned(),
            });
        };
        let nft_data = nft_account_data.ok_or(RaydiumClmmError::InsufficientBalance {
            mint,
            required: 1,
            available: 0,
        })?;
        if nft_data.owner != nft_token_program {
            return Err(RaydiumClmmError::MalformedAccount {
                address: nft_account,
                reason: "position NFT account owner is invalid".to_owned(),
            });
        }
        let nft_state =
            StateWithExtensions::<TokenAccount>::unpack(&nft_data.data).map_err(|error| {
                RaydiumClmmError::MalformedAccount {
                    address: nft_account,
                    reason: error.to_string(),
                }
            })?;
        if nft_state.base.amount != 1
            || current_pubkey(nft_state.base.mint) != mint
            || current_pubkey(nft_state.base.owner) != self.payer()
        {
            return Err(RaydiumClmmError::InsufficientBalance {
                mint,
                required: 1,
                available: nft_state.base.amount,
            });
        }
        Ok(PositionView {
            address,
            pool: current_pubkey(state.pool_id),
            nft_account,
            nft_token_program,
            tick_lower: state.tick_lower_index,
            tick_upper: state.tick_upper_index,
            liquidity: state.liquidity,
            fees_owed_0: state.token_fees_owed_0,
            fee_checkpoint: (
                state.fee_growth_inside_0_last_x64,
                state.fee_growth_inside_1_last_x64,
            ),
            fees_owed_1: state.token_fees_owed_1,
            rewards_owed: state.reward_infos.map(|reward| reward.reward_amount_owed),
        })
    }

    fn position_quote(
        &self,
        bundle: &PoolBundle,
        lower_price: Decimal,
        upper_price: Decimal,
        input: TokenAmount,
    ) -> Result<LiquidityQuote> {
        ensure_positive(lower_price, "lower_price")?;
        ensure_positive(upper_price, "upper_price")?;
        if lower_price >= upper_price {
            return Err(RaydiumClmmError::InvalidInput(
                "lower_price must be less than upper_price".to_owned(),
            ));
        }
        let lower_sqrt =
            price_to_sqrt_x64(lower_price, bundle.mint_0.decimals, bundle.mint_1.decimals)?;
        let upper_sqrt =
            price_to_sqrt_x64(upper_price, bundle.mint_0.decimals, bundle.mint_1.decimals)?;
        let lower_raw = tick_math::get_tick_at_sqrt_price(lower_sqrt)
            .map_err(|error| RaydiumClmmError::Quote(error.to_string()))?;
        let upper_raw = tick_math::get_tick_at_sqrt_price(upper_sqrt)
            .map_err(|error| RaydiumClmmError::Quote(error.to_string()))?;
        let spacing = i32::from(bundle.tick_spacing);
        let tick_lower = lower_raw.div_euclid(spacing) * spacing;
        let tick_upper = if upper_raw.rem_euclid(spacing) == 0 {
            upper_raw
        } else {
            upper_raw
                .div_euclid(spacing)
                .checked_add(1)
                .and_then(|value| value.checked_mul(spacing))
                .ok_or_else(|| RaydiumClmmError::Quote("upper tick overflow".to_owned()))?
        };
        if tick_lower >= tick_upper {
            return Err(RaydiumClmmError::InvalidInput(
                "price range is narrower than one tick spacing".to_owned(),
            ));
        }
        let position = PositionView {
            address: Pubkey::default(),
            pool: Pubkey::default(),
            nft_account: Pubkey::default(),
            nft_token_program: Pubkey::default(),
            tick_lower,
            tick_upper,
            liquidity: 0,
            fees_owed_0: 0,
            fees_owed_1: 0,
            fee_checkpoint: (0, 0),
            rewards_owed: [0; 3],
        };
        self.position_quote_for_ticks(bundle, &position, input)
    }

    fn position_quote_for_ticks(
        &self,
        bundle: &PoolBundle,
        position: &PositionView,
        input: TokenAmount,
    ) -> Result<LiquidityQuote> {
        ensure_positive(input.amount, "token amount")?;
        let input_mint = if input.mint == bundle.mint_0.key {
            &bundle.mint_0
        } else if input.mint == bundle.mint_1.key {
            &bundle.mint_1
        } else {
            return Err(RaydiumClmmError::InvalidInput(
                "input mint does not belong to the pool".to_owned(),
            ));
        };
        let input_amount_raw = ui_to_raw(input.amount, input_mint.decimals)?;
        let lower_sqrt = tick_math::get_sqrt_price_at_tick(position.tick_lower)
            .map_err(|error| RaydiumClmmError::Quote(error.to_string()))?;
        let upper_sqrt = tick_math::get_sqrt_price_at_tick(position.tick_upper)
            .map_err(|error| RaydiumClmmError::Quote(error.to_string()))?;
        let current_sqrt = bundle.pool.sqrt_price_x64;
        let liquidity = if input.mint == bundle.mint_0.key {
            liquidity_math::get_liquidity_from_single_amount_0(
                current_sqrt,
                lower_sqrt,
                upper_sqrt,
                input_amount_raw,
            )
        } else {
            liquidity_math::get_liquidity_from_single_amount_1(
                current_sqrt,
                lower_sqrt,
                upper_sqrt,
                input_amount_raw,
            )
        }
        .map_err(|error| RaydiumClmmError::Quote(error.to_string()))?;
        if liquidity == 0 {
            return Err(RaydiumClmmError::Quote(
                "token amount resolves to zero liquidity".to_owned(),
            ));
        }
        let tick_current = bundle.pool.tick_current;
        let (amount_0, amount_1) = liquidity_math::get_delta_amounts_signed(
            tick_current,
            current_sqrt,
            position.tick_lower,
            position.tick_upper,
            i128::try_from(liquidity)
                .map_err(|_| RaydiumClmmError::Quote("liquidity exceeds i128".to_owned()))?,
        )
        .map_err(|error| RaydiumClmmError::Quote(error.to_string()))?;
        let epoch = bundle.epoch;
        let amount_0_slipped = apply_slippage(amount_0, self.slippage_bps, true)?;
        let amount_1_slipped = apply_slippage(amount_1, self.slippage_bps, true)?;
        let amount_0_max = amount_0_slipped
            .checked_add(inverse_transfer_fee(
                &bundle.mint_0,
                epoch,
                amount_0_slipped,
            )?)
            .ok_or_else(|| RaydiumClmmError::Quote("token 0 maximum overflow".to_owned()))?;
        let amount_1_max = amount_1_slipped
            .checked_add(inverse_transfer_fee(
                &bundle.mint_1,
                epoch,
                amount_1_slipped,
            )?)
            .ok_or_else(|| RaydiumClmmError::Quote("token 1 maximum overflow".to_owned()))?;
        Ok(LiquidityQuote {
            liquidity,
            tick_lower: position.tick_lower,
            tick_upper: position.tick_upper,
            input_mint: input.mint,
            input_amount_raw,
            amount_0_raw: amount_0,
            amount_1_raw: amount_1,
            amount_0_max,
            amount_1_max,
            amount_0_min: 0,
            amount_1_min: 0,
        })
    }

    fn decrease_quote(
        &self,
        bundle: &PoolBundle,
        position: &PositionView,
        liquidity: u128,
    ) -> Result<LiquidityQuote> {
        let (amount_0, amount_1) = liquidity_math::get_delta_amounts_signed(
            bundle.pool.tick_current,
            bundle.pool.sqrt_price_x64,
            position.tick_lower,
            position.tick_upper,
            -i128::try_from(liquidity)
                .map_err(|_| RaydiumClmmError::Quote("liquidity exceeds i128".to_owned()))?,
        )
        .map_err(|error| RaydiumClmmError::Quote(error.to_string()))?;
        let epoch = bundle.epoch;
        let min_0_before_fee = apply_slippage(amount_0, self.slippage_bps, false)?;
        let min_1_before_fee = apply_slippage(amount_1, self.slippage_bps, false)?;
        let amount_0_min = min_0_before_fee
            .checked_sub(transfer_fee(&bundle.mint_0, epoch, min_0_before_fee)?)
            .ok_or_else(|| RaydiumClmmError::Quote("token 0 minimum underflow".to_owned()))?;
        let amount_1_min = min_1_before_fee
            .checked_sub(transfer_fee(&bundle.mint_1, epoch, min_1_before_fee)?)
            .ok_or_else(|| RaydiumClmmError::Quote("token 1 minimum underflow".to_owned()))?;
        Ok(LiquidityQuote {
            liquidity,
            tick_lower: position.tick_lower,
            tick_upper: position.tick_upper,
            input_mint: Pubkey::default(),
            input_amount_raw: 0,
            amount_0_raw: amount_0,
            amount_1_raw: amount_1,
            amount_0_max: 0,
            amount_1_max: 0,
            amount_0_min,
            amount_1_min,
        })
    }

    fn decrease_instruction(
        &self,
        bundle: &PoolBundle,
        position: &PositionView,
        accounts: &PositionAccounts,
        quote: &LiquidityQuote,
        remaining: Vec<LegacyAccountMeta>,
    ) -> Instruction {
        self.raydium_instruction(
            raydium_clmm_interface::accounts::DecreaseLiquidityV2 {
                nft_owner: legacy_pubkey(self.payer()),
                nft_account: legacy_pubkey(position.nft_account),
                personal_position: legacy_pubkey(position.address),
                pool_state: legacy_pubkey(position.pool),
                protocol_position: legacy_pubkey(accounts.protocol_position),
                token_vault_0: legacy_pubkey(bundle.token_vault_0),
                token_vault_1: legacy_pubkey(bundle.token_vault_1),
                tick_array_lower: legacy_pubkey(accounts.tick_array_lower),
                tick_array_upper: legacy_pubkey(accounts.tick_array_upper),
                recipient_token_account_0: legacy_pubkey(bundle.user_token_0),
                recipient_token_account_1: legacy_pubkey(bundle.user_token_1),
                token_program: spl_token::id(),
                token_program_2022: spl_token_2022::id(),
                memo_program: spl_memo::id(),
                vault_0_mint: legacy_pubkey(bundle.mint_0.key),
                vault_1_mint: legacy_pubkey(bundle.mint_1.key),
            },
            raydium_clmm_interface::instruction::DecreaseLiquidityV2 {
                liquidity: quote.liquidity,
                amount_0_min: quote.amount_0_min,
                amount_1_min: quote.amount_1_min,
            },
            remaining,
        )
    }

    async fn reward_accounts(
        &self,
        bundle: &PoolBundle,
    ) -> Result<(Vec<Instruction>, Vec<LegacyAccountMeta>)> {
        let mut remaining = vec![LegacyAccountMeta::new(
            legacy_pubkey(bundle.tick_array_bitmap),
            false,
        )];
        let rewards: Vec<(Pubkey, Pubkey)> = bundle
            .pool
            .reward_infos
            .iter()
            .filter_map(|reward| {
                let mint = current_pubkey(reward.token_mint);
                let vault = current_pubkey(reward.token_vault);
                (mint != Pubkey::default() && vault != Pubkey::default()).then_some((mint, vault))
            })
            .collect();
        if rewards.is_empty() {
            return Ok((vec![], remaining));
        }
        let mut keys = Vec::with_capacity(rewards.len() * 2);
        for (mint, vault) in &rewards {
            keys.push(*mint);
            keys.push(*vault);
        }
        let accounts = self.fetch_optional(&keys).await?;
        let mut instructions = Vec::new();
        for (index, (mint, vault)) in rewards.into_iter().enumerate() {
            let mint_info = self.decode_mint(
                mint,
                require_account(&keys, &accounts, index * 2, "reward mint")?,
            )?;
            self.validate_vault(
                bundle.key,
                vault,
                require_account(&keys, &accounts, index * 2 + 1, "reward vault")?,
                &mint_info,
            )?;
            let user_ata = associated_token_address(self.payer(), mint, mint_info.token_program);
            instructions.push(ata_instruction(self.payer(), &mint_info));
            remaining.push(LegacyAccountMeta::new(legacy_pubkey(vault), false));
            remaining.push(LegacyAccountMeta::new(legacy_pubkey(user_ata), false));
            remaining.push(LegacyAccountMeta::new_readonly(legacy_pubkey(mint), false));
        }
        Ok((instructions, remaining))
    }

    async fn load_swap_tick_arrays(
        &self,
        bundle: &PoolBundle,
        zero_for_one: bool,
    ) -> Result<(VecDeque<TickArrayState>, Vec<Pubkey>)> {
        let (_, mut start) = bundle
            .pool
            .get_first_initialized_tick_array(&Some(bundle.bitmap), zero_for_one)
            .map_err(|error| RaydiumClmmError::Quote(error.to_string()))?;
        let mut starts = vec![start];
        for _ in 1..TICK_ARRAY_DISCOVERY_LIMIT {
            let next = bundle
                .pool
                .next_initialized_tick_array_start_index(&Some(bundle.bitmap), start, zero_for_one)
                .map_err(|error| RaydiumClmmError::Quote(error.to_string()))?;
            let Some(next) = next else { break };
            starts.push(next);
            start = next;
        }
        let keys: Vec<Pubkey> = starts
            .iter()
            .map(|start| self.tick_array_address(bundle.key, *start))
            .collect();
        let accounts = self.fetch_optional(&keys).await?;
        let mut arrays = VecDeque::new();
        for (index, key) in keys.iter().enumerate() {
            let array: TickArrayState = self.decode_program_account(
                *key,
                require_account(&keys, &accounts, index, "tick array")?,
            )?;
            if current_pubkey(array.pool_id) != bundle.key
                || array.start_tick_index != starts[index]
            {
                return Err(RaydiumClmmError::MalformedAccount {
                    address: *key,
                    reason: "tick array pool or start index mismatch".to_owned(),
                });
            }
            arrays.push_back(array);
        }
        Ok((arrays, keys))
    }

    async fn validate_position_tick_arrays(
        &self,
        position: &PositionAccounts,
        required: bool,
    ) -> Result<()> {
        let keys = [position.tick_array_lower, position.tick_array_upper];
        let accounts = self.fetch_optional(&keys).await?;
        for (index, expected_start) in [
            position.tick_array_lower_start,
            position.tick_array_upper_start,
        ]
        .into_iter()
        .enumerate()
        {
            let Some(account) = accounts[index].as_ref() else {
                if required {
                    return Err(RaydiumClmmError::MalformedAccount {
                        address: keys[index],
                        reason: "position tick array is missing".to_owned(),
                    });
                }
                continue;
            };
            let array: TickArrayState = self.decode_program_account(keys[index], account)?;
            if current_pubkey(array.pool_id) != position.pool
                || array.start_tick_index != expected_start
            {
                return Err(RaydiumClmmError::MalformedAccount {
                    address: keys[index],
                    reason: "position tick array pool or start index mismatch".to_owned(),
                });
            }
        }
        Ok(())
    }

    fn position_ata_instructions(&self, bundle: &PoolBundle) -> Vec<Instruction> {
        vec![
            ata_instruction(self.payer(), &bundle.mint_0),
            ata_instruction(self.payer(), &bundle.mint_1),
        ]
    }

    /// Canonical pool PDA for `config` and the pool-ordered mints
    /// (`mint_0 < mint_1`).
    pub fn pool_address(&self, config: Pubkey, mint_0: Pubkey, mint_1: Pubkey) -> Pubkey {
        pool_address_for(self.program_id, config, mint_0, mint_1)
    }

    fn amm_config_address(&self, index: u16) -> Pubkey {
        pda(
            &[states::AMM_CONFIG_SEED.as_bytes(), &index.to_be_bytes()],
            self.program_id,
        )
    }

    fn derive_pool_addresses(&self, pool: Pubkey, mint_0: Pubkey, mint_1: Pubkey) -> PoolAccounts {
        PoolAccounts {
            pool,
            token_vault_0: pda(
                &[
                    states::POOL_VAULT_SEED.as_bytes(),
                    pool.as_ref(),
                    mint_0.as_ref(),
                ],
                self.program_id,
            ),
            token_vault_1: pda(
                &[
                    states::POOL_VAULT_SEED.as_bytes(),
                    pool.as_ref(),
                    mint_1.as_ref(),
                ],
                self.program_id,
            ),
            observation: pda(
                &[states::OBSERVATION_SEED.as_bytes(), pool.as_ref()],
                self.program_id,
            ),
            tick_array_bitmap: self.tick_array_bitmap_address(pool),
        }
    }

    fn position_addresses(
        &self,
        pool: Pubkey,
        mint: Pubkey,
        lower: i32,
        upper: i32,
        spacing: u16,
    ) -> PositionAccounts {
        let lower_start = TickArrayState::get_array_start_index(lower, spacing);
        let upper_start = TickArrayState::get_array_start_index(upper, spacing);
        PositionAccounts {
            pool,
            position_nft_mint: mint,
            position_nft_account: associated_token_address(
                self.payer(),
                mint,
                token_2022_program_id(),
            ),
            personal_position: self.personal_position_address(mint),
            protocol_position: pda(
                &[
                    states::POSITION_SEED.as_bytes(),
                    pool.as_ref(),
                    &lower.to_be_bytes(),
                    &upper.to_be_bytes(),
                ],
                self.program_id,
            ),
            tick_array_lower: self.tick_array_address(pool, lower_start),
            tick_array_upper: self.tick_array_address(pool, upper_start),
            tick_array_lower_start: lower_start,
            tick_array_upper_start: upper_start,
        }
    }

    fn personal_position_address(&self, mint: Pubkey) -> Pubkey {
        pda(
            &[states::POSITION_SEED.as_bytes(), mint.as_ref()],
            self.program_id,
        )
    }

    fn tick_array_address(&self, pool: Pubkey, start: i32) -> Pubkey {
        tick_array_address_for(self.program_id, pool, start)
    }

    fn tick_array_bitmap_address(&self, pool: Pubkey) -> Pubkey {
        pda(
            &[
                states::POOL_TICK_ARRAY_BITMAP_SEED.as_bytes(),
                pool.as_ref(),
            ],
            self.program_id,
        )
    }

    fn support_mint_address(&self, mint: Pubkey) -> Pubkey {
        pda(
            &[states::SUPPORT_MINT_SEED.as_bytes(), mint.as_ref()],
            self.program_id,
        )
    }
}

/// Pool, config, mints, and tick arrays for both swap directions, fetched
/// together. Built by [`RaydiumClmmClient::load_swap_snapshot`].
#[derive(Clone)]
pub struct SwapSnapshot {
    program_id: Pubkey,
    bundle: PoolBundle,
    config: AmmConfig,
    zero_for_one: std::result::Result<(VecDeque<TickArrayState>, Vec<Pubkey>), String>,
    one_for_zero: std::result::Result<(VecDeque<TickArrayState>, Vec<Pubkey>), String>,
}

impl SwapSnapshot {
    pub fn pool(&self) -> Pubkey {
        self.bundle.key
    }

    pub fn program_id(&self) -> Pubkey {
        self.program_id
    }

    /// Pool mints in pool order.
    pub fn mints(&self) -> (Pubkey, Pubkey) {
        (self.bundle.mint_0.key, self.bundle.mint_1.key)
    }

    pub fn token_programs(&self) -> (Pubkey, Pubkey) {
        (
            self.bundle.mint_0.token_program,
            self.bundle.mint_1.token_program,
        )
    }

    /// Trade fee in hundredths of a basis point (1,000,000 denominator).
    pub fn trade_fee_rate(&self) -> u32 {
        self.config.trade_fee_rate
    }

    /// `(zero_for_one, (input, output))` for a mint pair.
    fn direction(
        &self,
        input_mint: Pubkey,
        output_mint: Pubkey,
    ) -> Result<(bool, (&MintInfo, &MintInfo))> {
        let bundle = &self.bundle;
        if input_mint == bundle.mint_0.key && output_mint == bundle.mint_1.key {
            Ok((true, (&bundle.mint_0, &bundle.mint_1)))
        } else if input_mint == bundle.mint_1.key && output_mint == bundle.mint_0.key {
            Ok((false, (&bundle.mint_1, &bundle.mint_0)))
        } else {
            Err(RaydiumClmmError::InvalidInput(
                "swap mints do not match the pool".to_owned(),
            ))
        }
    }

    /// Quote a swap in raw atoms: `amount_raw` is the input for exact-in and
    /// the output for exact-out. Transfer fees use the snapshot's epoch.
    pub fn quote(
        &self,
        input_mint: Pubkey,
        output_mint: Pubkey,
        amount_raw: u64,
        exact_in: bool,
        slippage_bps: u16,
    ) -> Result<SwapQuote> {
        if amount_raw == 0 {
            return Err(RaydiumClmmError::InvalidInput(
                "swap amount must be greater than zero".to_owned(),
            ));
        }
        if slippage_bps > 10_000 {
            return Err(RaydiumClmmError::InvalidInput(
                "slippage_bps cannot exceed 10,000".to_owned(),
            ));
        }
        let bundle = &self.bundle;
        let (zero_for_one, (input, output)) = self.direction(input_mint, output_mint)?;
        let epoch = bundle.epoch;
        let (instruction_amount, quote_amount) = if exact_in {
            let input_fee = transfer_fee(input, epoch, amount_raw)?;
            (
                amount_raw,
                amount_raw.checked_sub(input_fee).ok_or_else(|| {
                    RaydiumClmmError::Quote("input transfer fee exceeds amount".to_owned())
                })?,
            )
        } else {
            let output_fee = inverse_transfer_fee(output, epoch, amount_raw)?;
            let gross_output = amount_raw.checked_add(output_fee).ok_or_else(|| {
                RaydiumClmmError::Quote("output transfer fee overflow".to_owned())
            })?;
            (amount_raw, gross_output)
        };
        let (arrays, array_keys) = if zero_for_one {
            &self.zero_for_one
        } else {
            &self.one_for_zero
        }
        .as_ref()
        .map_err(|error| RaydiumClmmError::Quote(error.clone()))?;
        let mut arrays = arrays.clone();
        let (calculated, used_starts) = quote::quote_swap(
            quote_amount,
            zero_for_one,
            exact_in,
            &self.config,
            &bundle.pool,
            &bundle.bitmap,
            &mut arrays,
        )
        .map_err(RaydiumClmmError::Quote)?;
        if used_starts.len() > MAX_SWAP_TICK_ARRAYS {
            return Err(RaydiumClmmError::Quote(format!(
                "swap requires {} tick arrays; the single-pool transaction limit is {}",
                used_starts.len(),
                MAX_SWAP_TICK_ARRAYS
            )));
        }
        let used_keys: Vec<Pubkey> = used_starts
            .iter()
            .map(|start| tick_array_address_for(self.program_id, bundle.key, *start))
            .collect();
        if used_keys.is_empty() || !used_keys.iter().all(|key| array_keys.contains(key)) {
            return Err(RaydiumClmmError::MalformedAccount {
                address: bundle.key,
                reason: "quote selected an unfetched tick array".to_owned(),
            });
        }
        let (other_amount_threshold, expected_input_raw, expected_output_raw) = if exact_in {
            // The program checks the threshold against the output account's
            // balance change, which is net of the output mint's transfer fee.
            let output_fee = transfer_fee(output, epoch, calculated)?;
            let net_output = calculated.checked_sub(output_fee).ok_or_else(|| {
                RaydiumClmmError::Quote("output transfer fee exceeds quote".to_owned())
            })?;
            (
                apply_slippage(net_output, slippage_bps, false)?,
                amount_raw,
                net_output,
            )
        } else {
            let slipped = apply_slippage(calculated, slippage_bps, true)?;
            let input_fee = inverse_transfer_fee(input, epoch, slipped)?;
            let expected_input_fee = inverse_transfer_fee(input, epoch, calculated)?;
            (
                slipped
                    .checked_add(input_fee)
                    .ok_or_else(|| RaydiumClmmError::Quote("maximum input overflow".to_owned()))?,
                calculated
                    .checked_add(expected_input_fee)
                    .ok_or_else(|| RaydiumClmmError::Quote("expected input overflow".to_owned()))?,
                amount_raw,
            )
        };
        if exact_in && expected_output_raw == 0 {
            return Err(RaydiumClmmError::Quote(
                "swap amount quotes to zero output".to_owned(),
            ));
        }
        Ok(SwapQuote {
            exact_in,
            input_mint,
            output_mint,
            instruction_amount,
            expected_input_raw,
            expected_output_raw,
            other_amount_threshold,
            tick_arrays: used_keys,
        })
    }

    /// Accounts of `SwapSingleV2` for `owner`, in instruction order, followed
    /// by the bitmap extension and the quote's tick arrays.
    pub fn swap_account_metas(&self, owner: Pubkey, quote: &SwapQuote) -> Result<Vec<AccountMeta>> {
        let accounts = self.swap_accounts(owner, quote)?;
        let (_, (input, output)) = self.direction(quote.input_mint, quote.output_mint)?;
        let mut metas = raydium_clmm_interface::accounts::SwapSingleV2 {
            payer: legacy_pubkey(owner),
            amm_config: legacy_pubkey(self.bundle.amm_config),
            pool_state: legacy_pubkey(self.bundle.key),
            input_token_account: legacy_pubkey(accounts.input_token_account),
            output_token_account: legacy_pubkey(accounts.output_token_account),
            input_vault: legacy_pubkey(accounts.input_vault),
            output_vault: legacy_pubkey(accounts.output_vault),
            observation_state: legacy_pubkey(self.bundle.observation),
            token_program: spl_token::id(),
            token_program_2022: spl_token_2022::id(),
            memo_program: spl_memo::id(),
            input_vault_mint: legacy_pubkey(input.key),
            output_vault_mint: legacy_pubkey(output.key),
        }
        .to_account_metas(None);
        metas.push(LegacyAccountMeta::new_readonly(
            legacy_pubkey(self.bundle.tick_array_bitmap),
            false,
        ));
        metas.extend(
            quote
                .tick_arrays
                .iter()
                .map(|key| LegacyAccountMeta::new(legacy_pubkey(*key), false)),
        );
        Ok(instruction(LegacyInstruction {
            program_id: legacy_pubkey(self.program_id),
            accounts: metas,
            data: vec![],
        })
        .accounts)
    }

    pub fn swap_accounts(&self, owner: Pubkey, quote: &SwapQuote) -> Result<SwapAccounts> {
        let (zero_for_one, (input, output)) =
            self.direction(quote.input_mint, quote.output_mint)?;
        let (input_vault, output_vault) = if zero_for_one {
            (self.bundle.token_vault_0, self.bundle.token_vault_1)
        } else {
            (self.bundle.token_vault_1, self.bundle.token_vault_0)
        };
        Ok(SwapAccounts {
            pool: self.bundle.key,
            input_token_account: associated_token_address(owner, input.key, input.token_program),
            output_token_account: associated_token_address(owner, output.key, output.token_program),
            input_vault,
            output_vault,
            observation: self.bundle.observation,
            tick_array_bitmap: self.bundle.tick_array_bitmap,
            tick_arrays: quote.tick_arrays.clone(),
        })
    }

    /// Idempotent ATA creation for both mints plus one `SwapV2` for `owner`.
    pub fn swap_instructions(&self, owner: Pubkey, quote: &SwapQuote) -> Result<Vec<Instruction>> {
        let (_, (input, output)) = self.direction(quote.input_mint, quote.output_mint)?;
        let accounts = self.swap_account_metas(owner, quote)?;
        let data = raydium_clmm_interface::instruction::SwapV2 {
            amount: quote.instruction_amount,
            other_amount_threshold: quote.other_amount_threshold,
            sqrt_price_limit_x64: 0,
            is_base_input: quote.exact_in,
        }
        .data();
        Ok(vec![
            ata_instruction(owner, input),
            ata_instruction(owner, output),
            Instruction {
                program_id: self.program_id,
                accounts,
                data,
            },
        ])
    }
}

fn tick_array_address_for(program_id: Pubkey, pool: Pubkey, start: i32) -> Pubkey {
    pda(
        &[
            states::TICK_ARRAY_SEED.as_bytes(),
            pool.as_ref(),
            &start.to_be_bytes(),
        ],
        program_id,
    )
}

fn pool_address_for(program_id: Pubkey, config: Pubkey, mint_0: Pubkey, mint_1: Pubkey) -> Pubkey {
    pda(
        &[
            states::POOL_SEED.as_bytes(),
            config.as_ref(),
            mint_0.as_ref(),
            mint_1.as_ref(),
        ],
        program_id,
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransactionOutcome {
    pub signature: Signature,
    pub confirmation_slot: u64,
    pub simulated_compute_units: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnsureAmmConfigParams {
    pub index: u16,
    pub tick_spacing: u16,
    pub trade_fee_rate: u32,
    pub protocol_fee_rate: u32,
    pub fund_fee_rate: u32,
}

#[derive(Clone, Debug)]
pub struct EnsureAmmConfigOutcome {
    pub transaction: Option<TransactionOutcome>,
    pub created: bool,
    pub amm_config: Pubkey,
    pub admin: Pubkey,
    pub params: EnsureAmmConfigParams,
}

#[derive(Clone, Copy, Debug)]
pub struct TokenAmount {
    pub mint: Pubkey,
    pub amount: Decimal,
}

#[derive(Clone, Copy, Debug)]
pub struct CreatePoolParams {
    pub amm_config: Pubkey,
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub initial_price: Decimal,
    pub open_time: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
pub struct OpenPositionParams {
    pub pool: Pubkey,
    pub lower_price: Decimal,
    pub upper_price: Decimal,
    pub input: TokenAmount,
    pub with_metadata: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct IncreaseLiquidityParams {
    pub position_mint: Pubkey,
    pub input: TokenAmount,
}

#[derive(Clone, Copy, Debug)]
pub struct DecreaseLiquidityParams {
    pub position_mint: Pubkey,
    /// `None` removes all liquidity. `Some` derives liquidity from a UI token amount.
    pub amount: Option<TokenAmount>,
}

#[derive(Clone, Copy, Debug)]
pub struct SwapExactInParams {
    pub pool: Pubkey,
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    pub amount_in: Decimal,
}

#[derive(Clone, Copy, Debug)]
pub struct SwapExactOutParams {
    pub pool: Pubkey,
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    pub amount_out: Decimal,
}

#[derive(Clone, Debug)]
pub struct CreatePoolOutcome {
    pub transaction: TransactionOutcome,
    pub quote: PoolCreationQuote,
    pub accounts: PoolAccounts,
}

#[derive(Clone, Copy, Debug)]
pub struct PoolCreationQuote {
    pub mint_0: Pubkey,
    pub mint_1: Pubkey,
    pub normalized_initial_price: Decimal,
    pub sqrt_price_x64: u128,
    pub open_time: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct PoolAccounts {
    pub pool: Pubkey,
    pub token_vault_0: Pubkey,
    pub token_vault_1: Pubkey,
    pub observation: Pubkey,
    pub tick_array_bitmap: Pubkey,
}

#[derive(Clone, Debug)]
pub struct PositionOutcome {
    pub transaction: TransactionOutcome,
    pub quote: LiquidityQuote,
    pub accounts: PositionAccounts,
}

#[derive(Clone, Copy, Debug)]
pub struct LiquidityQuote {
    pub liquidity: u128,
    pub tick_lower: i32,
    pub tick_upper: i32,
    pub input_mint: Pubkey,
    pub input_amount_raw: u64,
    pub amount_0_raw: u64,
    pub amount_1_raw: u64,
    pub amount_0_max: u64,
    pub amount_1_max: u64,
    pub amount_0_min: u64,
    pub amount_1_min: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct PositionAccounts {
    pub pool: Pubkey,
    pub position_nft_mint: Pubkey,
    pub position_nft_account: Pubkey,
    pub personal_position: Pubkey,
    pub protocol_position: Pubkey,
    pub tick_array_lower: Pubkey,
    pub tick_array_upper: Pubkey,
    pub tick_array_lower_start: i32,
    pub tick_array_upper_start: i32,
}

#[derive(Clone, Debug)]
pub struct CollectOutcome {
    pub transaction: TransactionOutcome,
    pub position: PositionAccounts,
}

/// Withdrawable value of one position, in raw atoms of the pool's mints.
#[derive(Clone, Copy, Debug)]
pub struct PositionValue {
    pub pool: Pubkey,
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub amount_a: u64,
    pub amount_b: u64,
    pub liquidity: u128,
}

#[derive(Clone, Debug)]
pub struct ClosePositionOutcome {
    pub transaction: TransactionOutcome,
    pub closed_position: Pubkey,
    pub closed_nft_mint: Pubkey,
    pub closed_nft_account: Pubkey,
    pub pool: Pubkey,
}

#[derive(Clone, Debug)]
pub struct SwapOutcome {
    pub transaction: TransactionOutcome,
    pub quote: SwapQuote,
    pub accounts: SwapAccounts,
}

#[derive(Clone, Debug)]
pub struct SwapQuote {
    pub exact_in: bool,
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    /// `amount` sent to the program: input for exact-in, output for exact-out.
    pub instruction_amount: u64,
    pub expected_input_raw: u64,
    pub expected_output_raw: u64,
    pub other_amount_threshold: u64,
    pub tick_arrays: Vec<Pubkey>,
}

impl SwapQuote {
    /// Most the swap may debit from the input account.
    pub fn max_input_raw(&self) -> u64 {
        if self.exact_in {
            self.instruction_amount
        } else {
            self.other_amount_threshold
        }
    }
}

#[derive(Clone, Debug)]
pub struct SwapAccounts {
    pub pool: Pubkey,
    pub input_token_account: Pubkey,
    pub output_token_account: Pubkey,
    pub input_vault: Pubkey,
    pub output_vault: Pubkey,
    pub observation: Pubkey,
    pub tick_array_bitmap: Pubkey,
    pub tick_arrays: Vec<Pubkey>,
}

#[derive(Debug, Error)]
pub enum RaydiumClmmError {
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("unsupported token {mint}: {reason}")]
    UnsupportedToken { mint: Pubkey, reason: String },
    #[error("malformed account {address}: {reason}")]
    MalformedAccount { address: Pubkey, reason: String },
    #[error("quote failed: {0}")]
    Quote(String),
    #[error("insufficient balance for {mint}: required {required}, available {available}")]
    InsufficientBalance {
        mint: Pubkey,
        required: u64,
        available: u64,
    },
    #[error("RPC request failed: {0}")]
    Rpc(String),
    #[error("simulation failed: {error}")]
    SimulationFailure { error: String, logs: Vec<String> },
    #[error("submission failed: {0}")]
    SubmissionFailure(String),
    #[error("confirmation of {signature} is ambiguous: {reason}")]
    AmbiguousConfirmation {
        signature: Signature,
        reason: String,
    },
}

#[derive(Clone)]
struct MintInfo {
    key: Pubkey,
    token_program: Pubkey,
    decimals: u8,
    data: Vec<u8>,
}

#[derive(Clone)]
struct PoolBundle {
    pool: PoolState,
    config_account: Account,
    amm_config: Pubkey,
    mint_0: MintInfo,
    mint_1: MintInfo,
    token_vault_0: Pubkey,
    token_vault_1: Pubkey,
    observation: Pubkey,
    tick_array_bitmap: Pubkey,
    bitmap: TickArrayBitmapExtension,
    tick_spacing: u16,
    user_token_0: Pubkey,
    user_token_1: Pubkey,
    balance_0: u64,
    balance_1: u64,
    epoch: u64,
    key: Pubkey,
}

struct PositionView {
    address: Pubkey,
    pool: Pubkey,
    nft_account: Pubkey,
    nft_token_program: Pubkey,
    tick_lower: i32,
    tick_upper: i32,
    liquidity: u128,
    fees_owed_0: u64,
    fees_owed_1: u64,
    fee_checkpoint: (u128, u128),
    rewards_owed: [u64; 3],
}

fn require_account<'a>(
    keys: &[Pubkey],
    accounts: &'a [Option<Account>],
    index: usize,
    label: &str,
) -> Result<&'a Account> {
    accounts
        .get(index)
        .and_then(Option::as_ref)
        .ok_or_else(|| RaydiumClmmError::MalformedAccount {
            address: keys.get(index).copied().unwrap_or_default(),
            reason: format!("missing {label}"),
        })
}

fn ensure_positive(value: Decimal, label: &str) -> Result<()> {
    if value <= Decimal::ZERO {
        Err(RaydiumClmmError::InvalidInput(format!(
            "{label} must be greater than zero"
        )))
    } else {
        Ok(())
    }
}

fn validate_amm_config_params(params: EnsureAmmConfigParams) -> Result<()> {
    if params.tick_spacing == 0 || params.tick_spacing > states::MAX_TICK_SPACING {
        return Err(RaydiumClmmError::InvalidInput(format!(
            "tick_spacing must be between 1 and {}",
            states::MAX_TICK_SPACING
        )));
    }
    if params.trade_fee_rate >= states::FEE_RATE_DENOMINATOR_VALUE {
        return Err(RaydiumClmmError::InvalidInput(format!(
            "trade_fee_rate must be less than {}",
            states::FEE_RATE_DENOMINATOR_VALUE
        )));
    }
    let owner_fee_rate = params
        .protocol_fee_rate
        .checked_add(params.fund_fee_rate)
        .ok_or_else(|| RaydiumClmmError::InvalidInput("owner fee rate overflow".to_owned()))?;
    if owner_fee_rate > states::FEE_RATE_DENOMINATOR_VALUE {
        return Err(RaydiumClmmError::InvalidInput(format!(
            "protocol_fee_rate plus fund_fee_rate cannot exceed {}",
            states::FEE_RATE_DENOMINATOR_VALUE
        )));
    }
    Ok(())
}

fn ui_to_raw(amount: Decimal, decimals: u8) -> Result<u64> {
    ensure_positive(amount, "token amount")?;
    let multiplier = decimal_scale(decimals)?;
    let raw = amount
        .checked_mul(multiplier)
        .ok_or_else(|| RaydiumClmmError::Quote("token amount overflow".to_owned()))?;
    if raw.fract() != Decimal::ZERO {
        return Err(RaydiumClmmError::InvalidInput(format!(
            "token amount has more than {decimals} decimal places"
        )));
    }
    raw.to_u64()
        .ok_or_else(|| RaydiumClmmError::Quote("token amount does not fit u64".to_owned()))
}

fn price_to_sqrt_x64(price: Decimal, decimals_0: u8, decimals_1: u8) -> Result<u128> {
    ensure_positive(price, "price")?;
    let scale_0 = decimal_scale(decimals_0)?;
    let scale_1 = decimal_scale(decimals_1)?;
    let raw_price = price
        .checked_mul(scale_1)
        .and_then(|value| value.checked_div(scale_0))
        .ok_or_else(|| RaydiumClmmError::Quote("price decimal normalization failed".to_owned()))?;
    let sqrt = raw_price
        .sqrt()
        .ok_or_else(|| RaydiumClmmError::Quote("price square root failed".to_owned()))?;
    sqrt.checked_mul(
        Decimal::from_u128(fixed_point_64::Q64)
            .ok_or_else(|| RaydiumClmmError::Quote("Q64 conversion failed".to_owned()))?,
    )
    .and_then(|value| value.floor().to_u128())
    .filter(|sqrt| (tick_math::MIN_SQRT_PRICE_X64..=tick_math::MAX_SQRT_PRICE_X64).contains(sqrt))
    .ok_or_else(|| {
        RaydiumClmmError::InvalidInput(format!("price {price} is outside the pool's price range"))
    })
}

fn apply_slippage(amount: u64, bps: u16, increase: bool) -> Result<u64> {
    let denominator = 10_000_u128;
    let numerator = if increase {
        denominator + u128::from(bps)
    } else {
        denominator
            .checked_sub(u128::from(bps))
            .ok_or_else(|| RaydiumClmmError::Quote("slippage underflow".to_owned()))?
    };
    let product = u128::from(amount)
        .checked_mul(numerator)
        .ok_or_else(|| RaydiumClmmError::Quote("slippage multiplication overflow".to_owned()))?;
    let adjusted = if increase {
        product
            .checked_add(denominator - 1)
            .ok_or_else(|| RaydiumClmmError::Quote("slippage rounding overflow".to_owned()))?
            / denominator
    } else {
        product / denominator
    };
    u64::try_from(adjusted)
        .map_err(|_| RaydiumClmmError::Quote("slippage result exceeds u64".to_owned()))
}

fn transfer_fee(mint: &MintInfo, epoch: u64, amount: u64) -> Result<u64> {
    if mint.token_program == token_program_id() {
        return Ok(0);
    }
    let state = StateWithExtensions::<Mint>::unpack(&mint.data).map_err(|error| {
        RaydiumClmmError::MalformedAccount {
            address: mint.key,
            reason: error.to_string(),
        }
    })?;
    match state.get_extension::<TransferFeeConfig>() {
        Ok(config) => config
            .calculate_epoch_fee(epoch, amount)
            .ok_or_else(|| RaydiumClmmError::Quote("transfer fee calculation overflow".to_owned())),
        Err(_) => Ok(0),
    }
}

fn inverse_transfer_fee(mint: &MintInfo, epoch: u64, amount: u64) -> Result<u64> {
    if mint.token_program == token_program_id() {
        return Ok(0);
    }
    let state = StateWithExtensions::<Mint>::unpack(&mint.data).map_err(|error| {
        RaydiumClmmError::MalformedAccount {
            address: mint.key,
            reason: error.to_string(),
        }
    })?;
    match state.get_extension::<TransferFeeConfig>() {
        Ok(config) => config
            .calculate_inverse_epoch_fee(epoch, amount)
            .ok_or_else(|| {
                RaydiumClmmError::Quote("inverse transfer fee calculation overflow".to_owned())
            }),
        Err(_) => Ok(0),
    }
}

fn pda(seeds: &[&[u8]], program_id: Pubkey) -> Pubkey {
    current_pubkey(
        anchor_lang::prelude::Pubkey::find_program_address(seeds, &legacy_pubkey(program_id)).0,
    )
}

fn token_program_id() -> Pubkey {
    current_pubkey(spl_token::id())
}

fn clock_sysvar_id() -> Pubkey {
    current_pubkey(anchor_lang::solana_program::sysvar::clock::ID)
}

/// Epoch from the Clock sysvar (`slot: u64, epoch_start_timestamp: i64,
/// epoch: u64, ...`), the epoch Token-2022 uses for transfer fees.
fn clock_epoch(account: &Account) -> Result<u64> {
    account
        .data
        .get(16..24)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| RaydiumClmmError::MalformedAccount {
            address: clock_sysvar_id(),
            reason: "clock sysvar is shorter than its layout".to_owned(),
        })
}

/// `10^decimals`; mint decimals are untrusted account data.
fn decimal_scale(decimals: u8) -> Result<Decimal> {
    10_u128
        .checked_pow(u32::from(decimals))
        .and_then(Decimal::from_u128)
        .ok_or_else(|| RaydiumClmmError::Quote(format!("10^{decimals} does not fit a decimal")))
}

fn token_2022_program_id() -> Pubkey {
    current_pubkey(spl_token_2022::id())
}

fn associated_token_address(owner: Pubkey, mint: Pubkey, token_program: Pubkey) -> Pubkey {
    current_pubkey(
        spl_associated_token_account::get_associated_token_address_with_program_id(
            &legacy_pubkey(owner),
            &legacy_pubkey(mint),
            &legacy_pubkey(token_program),
        ),
    )
}

fn ata_instruction(payer: Pubkey, mint: &MintInfo) -> Instruction {
    instruction(
        spl_associated_token_account::instruction::create_associated_token_account_idempotent(
            &legacy_pubkey(payer),
            &legacy_pubkey(payer),
            &legacy_pubkey(mint.key),
            &legacy_pubkey(mint.token_program),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_scale_rejects_hostile_decimals() {
        assert_eq!(
            decimal_scale(6).expect("valid test input"),
            Decimal::from(1_000_000)
        );
        assert!(decimal_scale(39).is_err());
        assert!(ui_to_raw(Decimal::ONE, 200).is_err());
    }

    #[test]
    fn ui_to_raw_scales_and_rejects_excess_precision() {
        assert_eq!(
            ui_to_raw(Decimal::new(15, 1), 6).expect("valid test input"),
            1_500_000
        );
        assert!(ui_to_raw(Decimal::new(1, 7), 6).is_err());
    }

    #[test]
    fn price_to_sqrt_x64_is_bounded_by_the_pool_range() {
        assert_eq!(
            price_to_sqrt_x64(Decimal::ONE, 6, 6).expect("valid test input"),
            fixed_point_64::Q64
        );
        // sqrt(1e-28) · 2^64 is far below MIN_SQRT_PRICE_X64.
        assert!(matches!(
            price_to_sqrt_x64(Decimal::new(1, 28), 0, 0),
            Err(RaydiumClmmError::InvalidInput(_))
        ));
    }

    #[test]
    fn clock_epoch_reads_the_epoch_field() {
        let mut data = vec![0_u8; 40];
        data[16..24].copy_from_slice(&7_u64.to_le_bytes());
        let account = Account {
            data,
            ..Account::default()
        };
        assert_eq!(clock_epoch(&account).expect("valid test input"), 7);
        let short = Account {
            data: vec![0; 20],
            ..Account::default()
        };
        assert!(clock_epoch(&short).is_err());
    }

    #[test]
    fn slippage_rounds_against_the_trader() {
        assert_eq!(
            apply_slippage(10_000, 50, true).expect("valid test input"),
            10_050
        );
        assert_eq!(
            apply_slippage(10_000, 50, false).expect("valid test input"),
            9_950
        );
        assert_eq!(apply_slippage(1, 1, true).expect("valid test input"), 2);
        assert_eq!(apply_slippage(1, 1, false).expect("valid test input"), 0);
    }
}
