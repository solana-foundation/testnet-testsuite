//! High-level async actions for a single Orca Whirlpool (concentrated
//! liquidity) pool on a self-deployed Whirlpool program.
//!
//! Inputs and outputs use the workspace's Agave 4.x modular types. The pinned
//! Orca client remains behind a private Solana 2.x adapter. Quotes use
//! `orca_whirlpools_core`, the same integer math the program executes.

mod adapter;

use std::sync::Arc;
use std::time::Instant;

use adapter::{current_pubkey, instruction, legacy_meta, legacy_pubkey};
use metrics::{counter, histogram};
use mtm_chain::{ChainClient, SubmissionConfig, SubmissionError};
use mtm_dex_harness::spl;
use orca_whirlpools_client as orca;
use orca_whirlpools_core::{
    self as core, IncreaseLiquidityQuote, TICK_ARRAY_SIZE, TickArrayFacade, TickArraySequence,
    TickFacade, TransferFee,
};
use rust_decimal::Decimal;
use rust_decimal::prelude::{FromPrimitive, MathematicalOps, ToPrimitive};
use sha2::{Digest, Sha256};
use solana_account::Account;
use solana_commitment_config::CommitmentConfig;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_signer::Signer;
use spl_token_2022::extension::transfer_fee::TransferFeeConfig;
use spl_token_2022::extension::{BaseStateWithExtensions, ExtensionType, StateWithExtensions};
use spl_token_2022::state::Mint;
use thiserror::Error;
use tracing::{Instrument, error, info, info_span};

/// Canonical mainnet Whirlpool program, for reference only; testnet uses a fork.
pub const CANONICAL_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc");
/// Update authority stamped into Token-2022 position metadata. A plain pubkey
/// constant in the program; it never signs.
pub const POSITION_METADATA_UPDATE_AUTH: Pubkey =
    Pubkey::from_str_const("3axbTs2z5GBy6usVbNVoqEgZMng3vZvMnAoX29BFfwhr");
/// Tick arrays initialized per transaction when seeding a price range.
const TICK_ARRAYS_PER_TRANSACTION: usize = 4;
/// Upper bound on tick arrays seeded for one range (each is ~0.07 SOL rent).
const MAX_SEEDED_TICK_ARRAYS: usize = 32;
const PROTOCOL_FEE_RATE_MAX: u16 = 2_500;
const FEE_RATE_MAX: u16 = 30_000;

pub type Result<T> = std::result::Result<T, OrcaWhirlpoolError>;

/// Derive the canonical pool address for a config, unordered mint pair, and
/// tick spacing.
pub fn derive_pool_address(
    program_id: Pubkey,
    whirlpools_config: Pubkey,
    mint_a: Pubkey,
    mint_b: Pubkey,
    tick_spacing: u16,
) -> Result<Pubkey> {
    let (mint_a, mint_b) = ordered_mints(mint_a, mint_b)?;
    Ok(pool_address_for(
        program_id,
        whirlpools_config,
        mint_a,
        mint_b,
        tick_spacing,
    ))
}

#[derive(Clone)]
pub struct OrcaWhirlpoolClient {
    chain: ChainClient,
    program_id: Pubkey,
    payer: Arc<dyn Signer + Send + Sync>,
    slippage_bps: u16,
    submission: SubmissionConfig,
}

impl OrcaWhirlpoolClient {
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
            return Err(OrcaWhirlpoolError::InvalidInput(
                "slippage_bps cannot exceed 10,000".to_owned(),
            ));
        }
        if compute_unit_limit == 0 {
            return Err(OrcaWhirlpoolError::InvalidInput(
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

    pub fn program_id(&self) -> Pubkey {
        self.program_id
    }

    /// Create the WhirlpoolsConfig (a keypair account) and one fee tier, or
    /// validate identical existing ones. The injected payer becomes the fee,
    /// protocol-fee-collection, and reward-emissions authority.
    pub async fn ensure_config(
        &self,
        config_signer: &dyn Signer,
        params: EnsureConfigParams,
    ) -> Result<EnsureConfigOutcome> {
        let config = config_signer.pubkey();
        self.run("ensure_config", config, async move {
            validate_config_params(params)?;
            let fee_tier = self.fee_tier_address(config, params.tick_spacing);
            let keys = [self.program_id, config, fee_tier];
            let accounts = self.fetch_optional(&keys).await?;
            let program = require_account(&keys, &accounts, 0, "Whirlpool program")?;
            if !program.executable {
                return Err(OrcaWhirlpoolError::MalformedAccount {
                    address: self.program_id,
                    reason: "Whirlpool program account is not executable".to_owned(),
                });
            }
            let admin = self.payer();
            let mut config_transaction = None;
            match accounts[1].as_ref() {
                Some(account) => {
                    let existing: orca::WhirlpoolsConfig = self.decode_program_account(
                        config,
                        account,
                        "WhirlpoolsConfig",
                        orca::WhirlpoolsConfig::from_bytes,
                    )?;
                    if current_pubkey(existing.fee_authority) != admin
                        || current_pubkey(existing.collect_protocol_fees_authority) != admin
                        || current_pubkey(existing.reward_emissions_super_authority) != admin
                        || existing.default_protocol_fee_rate != params.default_protocol_fee_rate
                    {
                        return Err(OrcaWhirlpoolError::InvalidInput(format!(
                            "existing WhirlpoolsConfig {config} does not match the requested admin or protocol fee"
                        )));
                    }
                }
                None => {
                    let ix = orca::InitializeConfig {
                        config: legacy_pubkey(config),
                        funder: legacy_pubkey(admin),
                        system_program: legacy_pubkey(spl::SYSTEM_PROGRAM_ID),
                    }
                    .instruction(orca::InitializeConfigInstructionArgs {
                        fee_authority: legacy_pubkey(admin),
                        collect_protocol_fees_authority: legacy_pubkey(admin),
                        reward_emissions_super_authority: legacy_pubkey(admin),
                        default_protocol_fee_rate: params.default_protocol_fee_rate,
                    });
                    config_transaction = Some(
                        self.submit(vec![instruction(ix, self.program_id)], &[config_signer])
                            .await?,
                    );
                }
            }
            let mut fee_tier_transaction = None;
            match accounts[2].as_ref() {
                Some(account) => {
                    let existing: orca::FeeTier = self.decode_program_account(
                        fee_tier,
                        account,
                        "FeeTier",
                        orca::FeeTier::from_bytes,
                    )?;
                    if current_pubkey(existing.whirlpools_config) != config
                        || existing.tick_spacing != params.tick_spacing
                        || existing.default_fee_rate != params.default_fee_rate
                    {
                        return Err(OrcaWhirlpoolError::InvalidInput(format!(
                            "existing fee tier {fee_tier} does not match the requested tick spacing or fee rate"
                        )));
                    }
                }
                None => {
                    let ix = orca::InitializeFeeTier {
                        config: legacy_pubkey(config),
                        fee_tier: legacy_pubkey(fee_tier),
                        funder: legacy_pubkey(admin),
                        fee_authority: legacy_pubkey(admin),
                        system_program: legacy_pubkey(spl::SYSTEM_PROGRAM_ID),
                    }
                    .instruction(orca::InitializeFeeTierInstructionArgs {
                        tick_spacing: params.tick_spacing,
                        default_fee_rate: params.default_fee_rate,
                    });
                    fee_tier_transaction =
                        Some(self.submit(vec![instruction(ix, self.program_id)], &[]).await?);
                }
            }
            Ok(EnsureConfigOutcome {
                created_config: config_transaction.is_some(),
                created_fee_tier: fee_tier_transaction.is_some(),
                config_transaction,
                fee_tier_transaction,
                whirlpools_config: config,
                fee_tier,
                admin,
                params,
            })
        })
        .await
    }

    pub async fn create_pool(&self, params: CreatePoolParams) -> Result<CreatePoolOutcome> {
        self.run("create_pool", params.whirlpools_config, async move {
            ensure_positive(params.initial_price, "initial_price")?;
            let (mint_a, mint_b) = ordered_mints(params.mint_a, params.mint_b)?;
            let normalized_price = if mint_a == params.mint_a {
                params.initial_price
            } else {
                Decimal::ONE
                    .checked_div(params.initial_price)
                    .ok_or_else(|| OrcaWhirlpoolError::Quote("price inversion failed".to_owned()))?
            };
            let config = params.whirlpools_config;
            let pool =
                pool_address_for(self.program_id, config, mint_a, mint_b, params.tick_spacing);
            let fee_tier = self.fee_tier_address(config, params.tick_spacing);
            let badge_a = self.token_badge_address(config, mint_a);
            let badge_b = self.token_badge_address(config, mint_b);
            let keys = [config, fee_tier, mint_a, mint_b, pool, badge_a, badge_b];
            let accounts = self.fetch_optional(&keys).await?;
            let _: orca::WhirlpoolsConfig = self.decode_program_account(
                config,
                require_account(&keys, &accounts, 0, "WhirlpoolsConfig")?,
                "WhirlpoolsConfig",
                orca::WhirlpoolsConfig::from_bytes,
            )?;
            let tier: orca::FeeTier = self.decode_program_account(
                fee_tier,
                require_account(&keys, &accounts, 1, "fee tier")?,
                "FeeTier",
                orca::FeeTier::from_bytes,
            )?;
            if current_pubkey(tier.whirlpools_config) != config {
                return Err(OrcaWhirlpoolError::MalformedAccount {
                    address: fee_tier,
                    reason: "fee tier belongs to a different config".to_owned(),
                });
            }
            let mint_info_a = decode_mint(mint_a, require_account(&keys, &accounts, 2, "mint A")?)?;
            let mint_info_b = decode_mint(mint_b, require_account(&keys, &accounts, 3, "mint B")?)?;
            if accounts[4].is_some() {
                return Err(OrcaWhirlpoolError::InvalidInput(format!(
                    "pool {pool} is already initialized"
                )));
            }
            validate_supported_mint(&mint_info_a, accounts[5].is_some())?;
            validate_supported_mint(&mint_info_b, accounts[6].is_some())?;

            let sqrt_price =
                price_to_sqrt_x64(normalized_price, mint_info_a.decimals, mint_info_b.decimals)?;
            let vault_a = Keypair::new();
            let vault_b = Keypair::new();
            let ix = orca::InitializePoolV2 {
                whirlpools_config: legacy_pubkey(config),
                token_mint_a: legacy_pubkey(mint_a),
                token_mint_b: legacy_pubkey(mint_b),
                token_badge_a: legacy_pubkey(badge_a),
                token_badge_b: legacy_pubkey(badge_b),
                funder: legacy_pubkey(self.payer()),
                whirlpool: legacy_pubkey(pool),
                token_vault_a: legacy_pubkey(vault_a.pubkey()),
                token_vault_b: legacy_pubkey(vault_b.pubkey()),
                fee_tier: legacy_pubkey(fee_tier),
                token_program_a: legacy_pubkey(mint_info_a.token_program),
                token_program_b: legacy_pubkey(mint_info_b.token_program),
                system_program: legacy_pubkey(spl::SYSTEM_PROGRAM_ID),
                rent: legacy_pubkey(spl::RENT_SYSVAR_ID),
            }
            .instruction(orca::InitializePoolV2InstructionArgs {
                tick_spacing: params.tick_spacing,
                initial_sqrt_price: sqrt_price,
            });
            let transaction = self
                .submit(
                    vec![instruction(ix, self.program_id)],
                    &[&vault_a, &vault_b],
                )
                .await?;
            Ok(CreatePoolOutcome {
                transaction,
                quote: PoolCreationQuote {
                    mint_a,
                    mint_b,
                    normalized_initial_price: normalized_price,
                    sqrt_price,
                    tick_spacing: params.tick_spacing,
                },
                accounts: PoolAccounts {
                    pool,
                    token_vault_a: vault_a.pubkey(),
                    token_vault_b: vault_b.pubkey(),
                    fee_tier,
                    oracle: self.oracle_address(pool),
                },
            })
        })
        .await
    }

    /// Initialize every missing tick array covering `[lower_price, upper_price]`
    /// (mint B per mint A, pool order) plus the array holding the current tick.
    /// Swaps work without them (sparse swap), but positions require their
    /// boundary arrays, so seeding at pool creation spares each user the rent.
    pub async fn ensure_tick_arrays(
        &self,
        pool: Pubkey,
        lower_price: Decimal,
        upper_price: Decimal,
    ) -> Result<TickArraySeedOutcome> {
        self.run("ensure_tick_arrays", pool, async move {
            let bundle = self.load_pool(pool).await?;
            let (tick_lower, tick_upper) =
                self.price_range_to_ticks(&bundle, lower_price, upper_price)?;
            let span = tick_array_span(bundle.whirlpool.tick_spacing);
            let current_start = core::get_tick_array_start_tick_index(
                bundle.whirlpool.tick_current_index,
                bundle.whirlpool.tick_spacing,
            );
            let first = core::get_tick_array_start_tick_index(tick_lower, bundle.whirlpool.tick_spacing)
                .min(current_start);
            let last = core::get_tick_array_start_tick_index(tick_upper, bundle.whirlpool.tick_spacing)
                .max(current_start);
            let count = usize::try_from((last - first) / span + 1).map_err(|_| {
                OrcaWhirlpoolError::Quote("tick array range overflow".to_owned())
            })?;
            if count > MAX_SEEDED_TICK_ARRAYS {
                return Err(OrcaWhirlpoolError::InvalidInput(format!(
                    "range needs {count} tick arrays; at most {MAX_SEEDED_TICK_ARRAYS} are seeded at once"
                )));
            }
            let starts: Vec<i32> = (0..count as i32).map(|i| first + i * span).collect();
            let keys: Vec<Pubkey> = starts
                .iter()
                .map(|start| self.tick_array_address(pool, *start))
                .collect();
            let existing = self.fetch_optional(&keys).await?;
            let missing: Vec<(i32, Pubkey)> = starts
                .iter()
                .zip(&keys)
                .zip(&existing)
                .filter(|(_, account)| account.is_none())
                .map(|((start, key), _)| (*start, *key))
                .collect();
            let mut transactions = Vec::new();
            for chunk in missing.chunks(TICK_ARRAYS_PER_TRANSACTION) {
                let instructions = chunk
                    .iter()
                    .map(|(start, key)| self.initialize_tick_array_instruction(pool, *start, *key))
                    .collect();
                transactions.push(self.submit(instructions, &[]).await?);
            }
            Ok(TickArraySeedOutcome {
                pool,
                tick_arrays: keys,
                initialized: missing.into_iter().map(|(_, key)| key).collect(),
                transactions,
            })
        })
        .await
    }

    /// Open a Token-2022 position NFT and deposit liquidity in one transaction.
    pub async fn open_position(&self, params: OpenPositionParams) -> Result<PositionOutcome> {
        self.run("open_position", params.pool, async move {
            let bundle = self.load_pool(params.pool).await?;
            let (tick_lower, tick_upper) =
                self.price_range_to_ticks(&bundle, params.lower_price, params.upper_price)?;
            let quote = self.liquidity_quote(&bundle, tick_lower, tick_upper, params.input)?;
            ensure_balances(&bundle, &quote)?;
            let position_mint = Keypair::new();
            let accounts = self.position_addresses(
                params.pool,
                position_mint.pubkey(),
                tick_lower,
                tick_upper,
                bundle.whirlpool.tick_spacing,
            );
            let mut instructions = self.position_ata_instructions(&bundle);
            instructions.extend(self.missing_tick_array_instructions(&accounts).await?);
            let open = orca::OpenPositionWithTokenExtensions {
                funder: legacy_pubkey(self.payer()),
                owner: legacy_pubkey(self.payer()),
                position: legacy_pubkey(accounts.position),
                position_mint: legacy_pubkey(accounts.position_mint),
                position_token_account: legacy_pubkey(accounts.position_token_account),
                whirlpool: legacy_pubkey(params.pool),
                token2022_program: legacy_pubkey(spl::TOKEN_2022_PROGRAM_ID),
                system_program: legacy_pubkey(spl::SYSTEM_PROGRAM_ID),
                associated_token_program: legacy_pubkey(spl::ASSOCIATED_TOKEN_PROGRAM_ID),
                metadata_update_auth: legacy_pubkey(POSITION_METADATA_UPDATE_AUTH),
            }
            .instruction(orca::OpenPositionWithTokenExtensionsInstructionArgs {
                tick_lower_index: tick_lower,
                tick_upper_index: tick_upper,
                with_token_metadata_extension: params.with_metadata,
            });
            instructions.push(instruction(open, self.program_id));
            instructions.push(self.increase_instruction(&bundle, &accounts, &quote));
            let transaction = self.submit(instructions, &[&position_mint]).await?;
            Ok(PositionOutcome {
                transaction,
                quote: quote.into(),
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
            let quote = self.liquidity_quote(
                &bundle,
                position.tick_lower,
                position.tick_upper,
                params.input,
            )?;
            ensure_balances(&bundle, &quote)?;
            let accounts = self.position_addresses(
                position.pool,
                params.position_mint,
                position.tick_lower,
                position.tick_upper,
                bundle.whirlpool.tick_spacing,
            );
            self.require_tick_arrays(&accounts).await?;
            let mut instructions = self.position_ata_instructions(&bundle);
            instructions.push(self.increase_instruction(&bundle, &accounts, &quote));
            let transaction = self.submit(instructions, &[]).await?;
            Ok(PositionOutcome {
                transaction,
                quote: quote.into(),
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
                Some(amount) => self.withdraw_liquidity(&bundle, &position, amount)?,
                None => position.liquidity,
            };
            if liquidity == 0 || liquidity > position.liquidity {
                return Err(OrcaWhirlpoolError::InvalidInput(
                    "decrease amount must resolve to non-zero liquidity no greater than the position"
                        .to_owned(),
                ));
            }
            let quote = core::decrease_liquidity_quote(
                liquidity,
                self.slippage_bps,
                bundle.whirlpool.sqrt_price,
                position.tick_lower,
                position.tick_upper,
                bundle.transfer_fee_a,
                bundle.transfer_fee_b,
            )
            .map_err(quote_error)?;
            let quote = widen_decrease_bounds(
                quote,
                bundle.price_and_fees(),
                position.tick_lower,
                position.tick_upper,
                self.slippage_bps,
            )?;
            let accounts = self.position_addresses(
                position.pool,
                params.position_mint,
                position.tick_lower,
                position.tick_upper,
                bundle.whirlpool.tick_spacing,
            );
            self.require_tick_arrays(&accounts).await?;
            let mut instructions = self.position_ata_instructions(&bundle);
            let ix = orca::DecreaseLiquidityV2 {
                whirlpool: legacy_pubkey(bundle.key),
                token_program_a: legacy_pubkey(bundle.mint_a.token_program),
                token_program_b: legacy_pubkey(bundle.mint_b.token_program),
                memo_program: legacy_pubkey(spl::MEMO_PROGRAM_ID),
                position_authority: legacy_pubkey(self.payer()),
                position: legacy_pubkey(accounts.position),
                position_token_account: legacy_pubkey(accounts.position_token_account),
                token_mint_a: legacy_pubkey(bundle.mint_a.key),
                token_mint_b: legacy_pubkey(bundle.mint_b.key),
                token_owner_account_a: legacy_pubkey(bundle.user_token_a),
                token_owner_account_b: legacy_pubkey(bundle.user_token_b),
                token_vault_a: legacy_pubkey(bundle.token_vault_a),
                token_vault_b: legacy_pubkey(bundle.token_vault_b),
                tick_array_lower: legacy_pubkey(accounts.tick_array_lower),
                tick_array_upper: legacy_pubkey(accounts.tick_array_upper),
            }
            .instruction(orca::DecreaseLiquidityV2InstructionArgs {
                liquidity_amount: liquidity,
                token_min_a: quote.token_min_a,
                token_min_b: quote.token_min_b,
                remaining_accounts_info: None,
            });
            instructions.push(instruction(ix, self.program_id));
            let transaction = self.submit(instructions, &[]).await?;
            Ok(PositionOutcome {
                transaction,
                quote: LiquidityQuote {
                    liquidity,
                    tick_lower: position.tick_lower,
                    tick_upper: position.tick_upper,
                    amount_a_raw: quote.token_est_a,
                    amount_b_raw: quote.token_est_b,
                    amount_a_max: 0,
                    amount_b_max: 0,
                    amount_a_min: quote.token_min_a,
                    amount_b_min: quote.token_min_b,
                },
                accounts,
            })
        })
        .await
    }

    /// Collect trading fees and every initialized reward. Positions that still
    /// hold liquidity refresh their fee checkpoints first.
    pub async fn collect_position(&self, position_mint: Pubkey) -> Result<CollectOutcome> {
        self.run("collect_position", position_mint, async move {
            let position = self.load_position(position_mint).await?;
            let bundle = self.load_pool(position.pool).await?;
            let accounts = self.position_addresses(
                position.pool,
                position_mint,
                position.tick_lower,
                position.tick_upper,
                bundle.whirlpool.tick_spacing,
            );
            let mut instructions = self.position_ata_instructions(&bundle);
            if position.liquidity > 0 {
                self.require_tick_arrays(&accounts).await?;
                let ix = orca::UpdateFeesAndRewards {
                    whirlpool: legacy_pubkey(bundle.key),
                    position: legacy_pubkey(accounts.position),
                    tick_array_lower: legacy_pubkey(accounts.tick_array_lower),
                    tick_array_upper: legacy_pubkey(accounts.tick_array_upper),
                }
                .instruction();
                instructions.push(instruction(ix, self.program_id));
            }
            let fees = orca::CollectFeesV2 {
                whirlpool: legacy_pubkey(bundle.key),
                position_authority: legacy_pubkey(self.payer()),
                position: legacy_pubkey(accounts.position),
                position_token_account: legacy_pubkey(accounts.position_token_account),
                token_mint_a: legacy_pubkey(bundle.mint_a.key),
                token_mint_b: legacy_pubkey(bundle.mint_b.key),
                token_owner_account_a: legacy_pubkey(bundle.user_token_a),
                token_vault_a: legacy_pubkey(bundle.token_vault_a),
                token_owner_account_b: legacy_pubkey(bundle.user_token_b),
                token_vault_b: legacy_pubkey(bundle.token_vault_b),
                token_program_a: legacy_pubkey(bundle.mint_a.token_program),
                token_program_b: legacy_pubkey(bundle.mint_b.token_program),
                memo_program: legacy_pubkey(spl::MEMO_PROGRAM_ID),
            }
            .instruction(orca::CollectFeesV2InstructionArgs {
                remaining_accounts_info: None,
            });
            instructions.push(instruction(fees, self.program_id));
            instructions.extend(self.reward_instructions(&bundle, &accounts).await?);
            let transaction = self.submit(instructions, &[]).await?;
            Ok(CollectOutcome {
                transaction,
                position: accounts,
            })
        })
        .await
    }

    /// The mint a position over `[lower_price, upper_price]` (pool order, B per
    /// A) is opened with at the pool's current price: a range above the price
    /// holds only A and one at or below it only B, so a deposit of the other
    /// mint resolves to zero liquidity. Inside the range either works; A is
    /// returned.
    pub async fn deposit_mint(
        &self,
        pool: Pubkey,
        lower_price: Decimal,
        upper_price: Decimal,
    ) -> Result<Pubkey> {
        let bundle = self.load_pool(pool).await?;
        let (_, tick_upper) = self.price_range_to_ticks(&bundle, lower_price, upper_price)?;
        Ok(if bundle.whirlpool.tick_current_index >= tick_upper {
            bundle.mint_b.key
        } else {
            bundle.mint_a.key
        })
    }

    /// What the position returns if fully withdrawn and its fees collected
    /// now, in raw atoms received (net of each mint's transfer fee). Rewards
    /// are not included.
    pub async fn position_value(&self, position_mint: Pubkey) -> Result<PositionValue> {
        let position = self.load_position(position_mint).await?;
        let bundle = self.load_pool(position.pool).await?;
        // Fees earned since the position's last update live in the pool's
        // fee growth, not in `fee_owed` yet. Read the whirlpool again in the
        // same call as the position's tick arrays: one slot's global fee
        // growth against another slot's ticks can wrap the fee-growth
        // subtraction to ~2^128.
        let spacing = bundle.whirlpool.tick_spacing;
        let ticks = [position.tick_lower, position.tick_upper];
        let starts = ticks.map(|tick| core::get_tick_array_start_tick_index(tick, spacing));
        let keys = [
            position.pool,
            self.tick_array_address(position.pool, starts[0]),
            self.tick_array_address(position.pool, starts[1]),
        ];
        let accounts = self.fetch_optional(&keys).await?;
        let pool: orca::Whirlpool = self.decode_program_account(
            position.pool,
            require_account(&keys, &accounts, 0, "whirlpool")?,
            "Whirlpool",
            orca::Whirlpool::from_bytes,
        )?;
        let mut outside = [(0_u128, 0_u128); 2];
        for (index, tick) in ticks.into_iter().enumerate() {
            let key = keys[index + 1];
            let array: orca::TickArray = self.decode_program_account(
                key,
                require_account(&keys, &accounts, index + 1, "position tick array")?,
                "TickArray",
                orca::TickArray::from_bytes,
            )?;
            if current_pubkey(array.whirlpool) != position.pool
                || array.start_tick_index != starts[index]
            {
                return Err(OrcaWhirlpoolError::MalformedAccount {
                    address: key,
                    reason: "tick array pool or start index mismatch".to_owned(),
                });
            }
            let state = usize::try_from((tick - starts[index]) / i32::from(spacing))
                .ok()
                .and_then(|offset| array.ticks.get(offset))
                .ok_or_else(|| OrcaWhirlpoolError::MalformedAccount {
                    address: key,
                    reason: format!("tick {tick} is outside its tick array"),
                })?;
            outside[index] = (state.fee_growth_outside_a, state.fee_growth_outside_b);
        }
        let quote = core::decrease_liquidity_quote(
            position.liquidity,
            0,
            pool.sqrt_price,
            position.tick_lower,
            position.tick_upper,
            bundle.transfer_fee_a,
            bundle.transfer_fee_b,
        )
        .map_err(quote_error)?;
        let pool = &pool;
        let accrued = |global, lower, upper, checkpoint| {
            mtm_math::clmm::fees_accrued(
                position.liquidity,
                global,
                lower,
                upper,
                pool.tick_current_index,
                position.tick_lower,
                position.tick_upper,
                checkpoint,
            )
        };
        let fees_a = accrued(
            pool.fee_growth_global_a,
            outside[0].0,
            outside[1].0,
            position.fee_checkpoint.0,
        );
        let fees_b = accrued(
            pool.fee_growth_global_b,
            outside[0].1,
            outside[1].1,
            position.fee_checkpoint.1,
        );
        // The liquidity estimate is already net of transfer fees; collected
        // fees pay the mint's transfer fee too.
        let net = |amount: u64, fee: Option<core::TransferFee>| {
            core::try_apply_transfer_fee(amount, fee.unwrap_or_default()).map_err(quote_error)
        };
        let fees_a = net(
            position.fee_owed_a.saturating_add(fees_a),
            bundle.transfer_fee_a,
        )?;
        let fees_b = net(
            position.fee_owed_b.saturating_add(fees_b),
            bundle.transfer_fee_b,
        )?;
        Ok(PositionValue {
            pool: position.pool,
            mint_a: bundle.mint_a.key,
            mint_b: bundle.mint_b.key,
            amount_a: quote.token_est_a.saturating_add(fees_a),
            amount_b: quote.token_est_b.saturating_add(fees_b),
            liquidity: position.liquidity,
        })
    }

    pub async fn close_position(&self, position_mint: Pubkey) -> Result<ClosePositionOutcome> {
        self.run("close_position", position_mint, async move {
            let position = self.load_position(position_mint).await?;
            if position.liquidity != 0
                || position.fee_owed_a != 0
                || position.fee_owed_b != 0
                || position.rewards_owed.iter().any(|amount| *amount != 0)
            {
                return Err(OrcaWhirlpoolError::InvalidInput(
                    "position must have zero liquidity, fees, and rewards before close".to_owned(),
                ));
            }
            let ix = orca::ClosePositionWithTokenExtensions {
                position_authority: legacy_pubkey(self.payer()),
                receiver: legacy_pubkey(self.payer()),
                position: legacy_pubkey(position.address),
                position_mint: legacy_pubkey(position_mint),
                position_token_account: legacy_pubkey(position.token_account),
                token2022_program: legacy_pubkey(spl::TOKEN_2022_PROGRAM_ID),
            }
            .instruction();
            let transaction = self
                .submit(vec![instruction(ix, self.program_id)], &[])
                .await?;
            Ok(ClosePositionOutcome {
                transaction,
                closed_position: position.address,
                closed_position_mint: position_mint,
                closed_position_token_account: position.token_account,
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

    /// Fetch the pool, mints, and the five tick arrays around the current
    /// tick. Quoting and instruction building on the snapshot are pure, so
    /// callers (the router) can price many sizes and build for any owner.
    pub async fn load_swap_snapshot(&self, pool: Pubkey) -> Result<SwapSnapshot> {
        let mut bundle = self.load_pool(pool).await?;
        let (tick_arrays, facades) = self.load_swap_tick_arrays(&mut bundle).await?;
        Ok(SwapSnapshot {
            program_id: self.program_id,
            oracle: self.oracle_address(pool),
            bundle,
            tick_arrays,
            facades,
        })
    }

    async fn swap(
        &self,
        pool: Pubkey,
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
        self.run(action, pool, async move {
            ensure_positive(amount, "swap amount")?;
            let snapshot = self.load_swap_snapshot(pool).await?;
            let bundle = &snapshot.bundle;
            let specified = if exact_in {
                bundle.mint_for(input_mint)?
            } else {
                bundle.mint_for(output_mint)?
            };
            let amount_raw = ui_to_raw(amount, specified.decimals)?;
            let quote = snapshot.quote(
                input_mint,
                output_mint,
                amount_raw,
                exact_in,
                self.slippage_bps,
            )?;
            let input_balance = if input_mint == bundle.mint_a.key {
                bundle.balance_a
            } else {
                bundle.balance_b
            };
            if input_balance < quote.max_input_raw {
                return Err(OrcaWhirlpoolError::InsufficientBalance {
                    mint: input_mint,
                    required: quote.max_input_raw,
                    available: input_balance,
                });
            }
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
        let span = info_span!("orca_whirlpool_action", action, %target);
        let result = future.instrument(span).await;
        let status = if result.is_ok() { "confirmed" } else { "error" };
        counter!("orca_whirlpool_actions_total", "action" => action, "status" => status)
            .increment(1);
        histogram!("orca_whirlpool_action_latency_seconds", "action" => action)
            .record(started.elapsed().as_secs_f64());
        match &result {
            Ok(_) => {
                info!(action, %target, latency_ms = started.elapsed().as_millis(), "Orca action complete")
            }
            Err(problem) => {
                if let OrcaWhirlpoolError::SimulationFailure { logs, .. } = problem {
                    error!(action, %target, ?logs, "Orca simulation logs");
                }
                error!(action, %target, error = %problem, latency_ms = started.elapsed().as_millis(), "Orca action failed")
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
                histogram!("orca_whirlpool_simulated_compute_units")
                    .record(outcome.simulated_compute_units as f64);
                info!(
                    signature = %outcome.signature,
                    slot = outcome.confirmation_slot,
                    compute_units = outcome.simulated_compute_units,
                    "Orca transaction confirmed"
                );
                Ok(TransactionOutcome {
                    signature: outcome.signature,
                    confirmation_slot: outcome.confirmation_slot,
                    simulated_compute_units: outcome.simulated_compute_units,
                })
            }
            Err(SubmissionError::Simulation { error, logs }) => {
                Err(OrcaWhirlpoolError::SimulationFailure { error, logs })
            }
            Err(SubmissionError::SimulationRpc(error)) => {
                Err(OrcaWhirlpoolError::SimulationFailure {
                    error,
                    logs: vec![],
                })
            }
            Err(SubmissionError::AmbiguousConfirmation { signature, reason }) => {
                Err(OrcaWhirlpoolError::AmbiguousConfirmation { signature, reason })
            }
            Err(error) => Err(OrcaWhirlpoolError::SubmissionFailure(error.to_string())),
        }
    }

    async fn fetch_optional(&self, keys: &[Pubkey]) -> Result<Vec<Option<Account>>> {
        self.chain
            .rpc()
            .get_multiple_accounts(keys)
            .await
            .map_err(|error| OrcaWhirlpoolError::Rpc(error.to_string()))
    }

    fn decode_program_account<T>(
        &self,
        key: Pubkey,
        account: &Account,
        name: &str,
        decode: fn(&[u8]) -> std::io::Result<T>,
    ) -> Result<T> {
        if account.owner != self.program_id {
            return Err(OrcaWhirlpoolError::MalformedAccount {
                address: key,
                reason: format!("expected owner {}, got {}", self.program_id, account.owner),
            });
        }
        if account.data.get(..8) != Some(&account_discriminator(name)[..]) {
            return Err(OrcaWhirlpoolError::MalformedAccount {
                address: key,
                reason: format!("account is not a {name}"),
            });
        }
        decode(&account.data).map_err(|error| OrcaWhirlpoolError::MalformedAccount {
            address: key,
            reason: error.to_string(),
        })
    }

    async fn load_pool(&self, key: Pubkey) -> Result<PoolBundle> {
        let first = self.fetch_optional(&[key]).await?;
        let whirlpool: orca::Whirlpool = self.decode_program_account(
            key,
            require_account(&[key], &first, 0, "whirlpool")?,
            "Whirlpool",
            orca::Whirlpool::from_bytes,
        )?;
        let config = current_pubkey(whirlpool.whirlpools_config);
        let mint_a_key = current_pubkey(whirlpool.token_mint_a);
        let mint_b_key = current_pubkey(whirlpool.token_mint_b);
        let token_vault_a = current_pubkey(whirlpool.token_vault_a);
        let token_vault_b = current_pubkey(whirlpool.token_vault_b);
        if pool_address_for(
            self.program_id,
            config,
            mint_a_key,
            mint_b_key,
            u16::from_le_bytes(whirlpool.tick_spacing_seed),
        ) != key
        {
            return Err(OrcaWhirlpoolError::MalformedAccount {
                address: key,
                reason: "whirlpool is not its canonical PDA".to_owned(),
            });
        }
        // The payer's token accounts depend on each mint's token program,
        // known only once the mints load: fetch both candidates in this batch
        // (with the Clock sysvar for the transfer-fee epoch) instead of two
        // more round trips.
        let payer = self.payer();
        let payer_atas = [mint_a_key, mint_b_key].map(|mint| {
            [spl::TOKEN_PROGRAM_ID, spl::TOKEN_2022_PROGRAM_ID]
                .map(|program| spl::associated_token_address(&payer, &mint, &program))
        });
        let keys = [
            mint_a_key,
            mint_b_key,
            token_vault_a,
            token_vault_b,
            CLOCK_SYSVAR_ID,
            payer_atas[0][0],
            payer_atas[0][1],
            payer_atas[1][0],
            payer_atas[1][1],
        ];
        let accounts = self.fetch_optional(&keys).await?;
        let mint_a = decode_mint(mint_a_key, require_account(&keys, &accounts, 0, "mint A")?)?;
        let mint_b = decode_mint(mint_b_key, require_account(&keys, &accounts, 1, "mint B")?)?;
        validate_vault(
            key,
            token_vault_a,
            require_account(&keys, &accounts, 2, "vault A")?,
            &mint_a,
        )?;
        validate_vault(
            key,
            token_vault_b,
            require_account(&keys, &accounts, 3, "vault B")?,
            &mint_b,
        )?;
        let epoch = clock_epoch(require_account(&keys, &accounts, 4, "clock sysvar")?)?;
        let payer_ata = |index: usize, mint: &MintInfo| {
            let slot =
                5 + 2 * index + usize::from(mint.token_program == spl::TOKEN_2022_PROGRAM_ID);
            (keys[slot], accounts[slot].as_ref())
        };
        let (user_token_a, user_account_a) = payer_ata(0, &mint_a);
        let (user_token_b, user_account_b) = payer_ata(1, &mint_b);
        let balance_a = self.validate_token_account(user_token_a, user_account_a, &mint_a)?;
        let balance_b = self.validate_token_account(user_token_b, user_account_b, &mint_b)?;
        Ok(PoolBundle {
            transfer_fee_a: current_transfer_fee(&mint_a, epoch)?,
            transfer_fee_b: current_transfer_fee(&mint_b, epoch)?,
            whirlpool,
            mint_a,
            mint_b,
            token_vault_a,
            token_vault_b,
            user_token_a,
            user_token_b,
            balance_a,
            balance_b,
            key,
        })
    }

    fn validate_token_account(
        &self,
        key: Pubkey,
        account: Option<&Account>,
        mint: &MintInfo,
    ) -> Result<u64> {
        let Some(account) = account else {
            return Ok(0);
        };
        if account.owner != mint.token_program {
            return Err(OrcaWhirlpoolError::MalformedAccount {
                address: key,
                reason: "token account is owned by the wrong token program".to_owned(),
            });
        }
        let state = spl::decode_token_account(&account.data).ok_or_else(|| {
            OrcaWhirlpoolError::MalformedAccount {
                address: key,
                reason: "token account data is too short".to_owned(),
            }
        })?;
        if state.mint != mint.key || state.owner != self.payer() {
            return Err(OrcaWhirlpoolError::MalformedAccount {
                address: key,
                reason: "token account mint or authority is invalid".to_owned(),
            });
        }
        Ok(state.amount)
    }

    async fn load_position(&self, mint: Pubkey) -> Result<PositionView> {
        let address = self.position_address(mint);
        let token_account =
            spl::associated_token_address(&self.payer(), &mint, &spl::TOKEN_2022_PROGRAM_ID);
        let keys = [address, token_account];
        let accounts = self.fetch_optional(&keys).await?;
        let state: orca::Position = self.decode_program_account(
            address,
            require_account(&keys, &accounts, 0, "position")?,
            "Position",
            orca::Position::from_bytes,
        )?;
        if current_pubkey(state.position_mint) != mint {
            return Err(OrcaWhirlpoolError::MalformedAccount {
                address,
                reason: "position contains a different position mint".to_owned(),
            });
        }
        let holding = accounts[1]
            .as_ref()
            .filter(|account| account.owner == spl::TOKEN_2022_PROGRAM_ID)
            .and_then(|account| spl::decode_token_account(&account.data));
        match holding {
            Some(held) if held.amount == 1 && held.mint == mint && held.owner == self.payer() => {}
            other => {
                return Err(OrcaWhirlpoolError::InsufficientBalance {
                    mint,
                    required: 1,
                    available: other.map_or(0, |held| held.amount),
                });
            }
        }
        Ok(PositionView {
            address,
            pool: current_pubkey(state.whirlpool),
            token_account,
            tick_lower: state.tick_lower_index,
            tick_upper: state.tick_upper_index,
            liquidity: state.liquidity,
            fee_owed_a: state.fee_owed_a,
            fee_owed_b: state.fee_owed_b,
            fee_checkpoint: (state.fee_growth_checkpoint_a, state.fee_growth_checkpoint_b),
            rewards_owed: state.reward_infos.map(|reward| reward.amount_owed),
        })
    }

    fn price_range_to_ticks(
        &self,
        bundle: &PoolBundle,
        lower_price: Decimal,
        upper_price: Decimal,
    ) -> Result<(i32, i32)> {
        ensure_positive(lower_price, "lower_price")?;
        ensure_positive(upper_price, "upper_price")?;
        if lower_price >= upper_price {
            return Err(OrcaWhirlpoolError::InvalidInput(
                "lower_price must be less than upper_price".to_owned(),
            ));
        }
        let spacing = bundle.whirlpool.tick_spacing;
        let lower_sqrt =
            price_to_sqrt_x64(lower_price, bundle.mint_a.decimals, bundle.mint_b.decimals)?;
        let upper_sqrt =
            price_to_sqrt_x64(upper_price, bundle.mint_a.decimals, bundle.mint_b.decimals)?;
        let tick_lower = core::get_initializable_tick_index(
            core::sqrt_price_to_tick_index(lower_sqrt),
            spacing,
            Some(false),
        );
        let tick_upper = core::get_initializable_tick_index(
            core::sqrt_price_to_tick_index(upper_sqrt),
            spacing,
            Some(true),
        );
        if tick_lower >= tick_upper {
            return Err(OrcaWhirlpoolError::InvalidInput(
                "price range is narrower than one tick spacing".to_owned(),
            ));
        }
        if !core::is_tick_index_in_bounds(tick_lower) || !core::is_tick_index_in_bounds(tick_upper)
        {
            return Err(OrcaWhirlpoolError::InvalidInput(
                "price range is outside the supported tick bounds".to_owned(),
            ));
        }
        Ok((tick_lower, tick_upper))
    }

    fn liquidity_quote(
        &self,
        bundle: &PoolBundle,
        tick_lower: i32,
        tick_upper: i32,
        input: TokenAmount,
    ) -> Result<IncreaseLiquidityQuote> {
        let mint = bundle.mint_for(input.mint)?;
        let amount_raw = ui_to_raw(input.amount, mint.decimals)?;
        let quote = if input.mint == bundle.mint_a.key {
            core::increase_liquidity_quote_a(
                amount_raw,
                self.slippage_bps,
                bundle.whirlpool.sqrt_price,
                tick_lower,
                tick_upper,
                bundle.transfer_fee_a,
                bundle.transfer_fee_b,
            )
        } else {
            core::increase_liquidity_quote_b(
                amount_raw,
                self.slippage_bps,
                bundle.whirlpool.sqrt_price,
                tick_lower,
                tick_upper,
                bundle.transfer_fee_a,
                bundle.transfer_fee_b,
            )
        }
        .map_err(quote_error)?;
        if quote.liquidity_delta == 0 {
            return Err(OrcaWhirlpoolError::Quote(
                "token amount resolves to zero liquidity".to_owned(),
            ));
        }
        widen_increase_bounds(
            quote,
            bundle.price_and_fees(),
            tick_lower,
            tick_upper,
            self.slippage_bps,
        )
    }

    /// Liquidity to withdraw so the position returns `amount` of one token
    /// (net of its transfer fee) at the current price.
    fn withdraw_liquidity(
        &self,
        bundle: &PoolBundle,
        position: &PositionView,
        amount: TokenAmount,
    ) -> Result<u128> {
        let mint = bundle.mint_for(amount.mint)?;
        let amount_raw = ui_to_raw(amount.amount, mint.decimals)?;
        let quote = if amount.mint == bundle.mint_a.key {
            core::decrease_liquidity_quote_a(
                amount_raw,
                self.slippage_bps,
                bundle.whirlpool.sqrt_price,
                position.tick_lower,
                position.tick_upper,
                bundle.transfer_fee_a,
                bundle.transfer_fee_b,
            )
        } else {
            core::decrease_liquidity_quote_b(
                amount_raw,
                self.slippage_bps,
                bundle.whirlpool.sqrt_price,
                position.tick_lower,
                position.tick_upper,
                bundle.transfer_fee_a,
                bundle.transfer_fee_b,
            )
        }
        .map_err(quote_error)?;
        Ok(quote.liquidity_delta)
    }

    fn increase_instruction(
        &self,
        bundle: &PoolBundle,
        accounts: &PositionAccounts,
        quote: &IncreaseLiquidityQuote,
    ) -> Instruction {
        let ix = orca::IncreaseLiquidityV2 {
            whirlpool: legacy_pubkey(bundle.key),
            token_program_a: legacy_pubkey(bundle.mint_a.token_program),
            token_program_b: legacy_pubkey(bundle.mint_b.token_program),
            memo_program: legacy_pubkey(spl::MEMO_PROGRAM_ID),
            position_authority: legacy_pubkey(self.payer()),
            position: legacy_pubkey(accounts.position),
            position_token_account: legacy_pubkey(accounts.position_token_account),
            token_mint_a: legacy_pubkey(bundle.mint_a.key),
            token_mint_b: legacy_pubkey(bundle.mint_b.key),
            token_owner_account_a: legacy_pubkey(bundle.user_token_a),
            token_owner_account_b: legacy_pubkey(bundle.user_token_b),
            token_vault_a: legacy_pubkey(bundle.token_vault_a),
            token_vault_b: legacy_pubkey(bundle.token_vault_b),
            tick_array_lower: legacy_pubkey(accounts.tick_array_lower),
            tick_array_upper: legacy_pubkey(accounts.tick_array_upper),
        }
        .instruction(orca::IncreaseLiquidityV2InstructionArgs {
            liquidity_amount: quote.liquidity_delta,
            token_max_a: quote.token_max_a,
            token_max_b: quote.token_max_b,
            remaining_accounts_info: None,
        });
        instruction(ix, self.program_id)
    }

    async fn reward_instructions(
        &self,
        bundle: &PoolBundle,
        accounts: &PositionAccounts,
    ) -> Result<Vec<Instruction>> {
        let rewards: Vec<(u8, Pubkey, Pubkey)> = bundle
            .whirlpool
            .reward_infos
            .iter()
            .enumerate()
            .filter_map(|(index, reward)| {
                let mint = current_pubkey(reward.mint);
                (mint != Pubkey::default())
                    .then(|| (index as u8, mint, current_pubkey(reward.vault)))
            })
            .collect();
        if rewards.is_empty() {
            return Ok(vec![]);
        }
        let keys: Vec<Pubkey> = rewards.iter().map(|(_, mint, _)| *mint).collect();
        let mints = self.fetch_optional(&keys).await?;
        let mut instructions = Vec::with_capacity(rewards.len() * 2);
        for (position, (index, mint, vault)) in rewards.into_iter().enumerate() {
            let info = decode_mint(
                mint,
                require_account(&keys, &mints, position, "reward mint")?,
            )?;
            let owner_account =
                spl::associated_token_address(&self.payer(), &mint, &info.token_program);
            instructions.push(spl::create_associated_token_account_idempotent(
                &self.payer(),
                &self.payer(),
                &mint,
                &info.token_program,
            ));
            let ix = orca::CollectRewardV2 {
                whirlpool: legacy_pubkey(bundle.key),
                position_authority: legacy_pubkey(self.payer()),
                position: legacy_pubkey(accounts.position),
                position_token_account: legacy_pubkey(accounts.position_token_account),
                reward_owner_account: legacy_pubkey(owner_account),
                reward_mint: legacy_pubkey(mint),
                reward_vault: legacy_pubkey(vault),
                reward_token_program: legacy_pubkey(info.token_program),
                memo_program: legacy_pubkey(spl::MEMO_PROGRAM_ID),
            }
            .instruction(orca::CollectRewardV2InstructionArgs {
                reward_index: index,
                remaining_accounts_info: None,
            });
            instructions.push(instruction(ix, self.program_id));
        }
        Ok(instructions)
    }

    /// Five tick arrays centred on the current tick (current, +1, +2, -1, -2),
    /// matching Orca's own SDK; uninitialized arrays quote as empty and are
    /// accepted on-chain by sparse swap.
    ///
    /// The whirlpool is re-read in the same `getMultipleAccounts` (one slot)
    /// so liquidity and ticks are consistent: separate calls under live
    /// trading can pair one slot's liquidity with another slot's ticks, which
    /// makes the quote walk underflow. Retries while the current tick keeps
    /// moving into another array between the two reads.
    async fn load_swap_tick_arrays(
        &self,
        bundle: &mut PoolBundle,
    ) -> Result<([Pubkey; 5], [TickArrayFacade; 5])> {
        let spacing = bundle.whirlpool.tick_spacing;
        let span = tick_array_span(spacing);
        for _ in 0..3 {
            let start =
                core::get_tick_array_start_tick_index(bundle.whirlpool.tick_current_index, spacing);
            let starts = [
                start,
                start + span,
                start + span * 2,
                start - span,
                start - span * 2,
            ];
            let keys = starts.map(|start| self.tick_array_address(bundle.key, start));
            let mut request = vec![bundle.key];
            request.extend(keys);
            let accounts = self.fetch_optional(&request).await?;
            let whirlpool: orca::Whirlpool = self.decode_program_account(
                bundle.key,
                require_account(&request, &accounts, 0, "whirlpool")?,
                "Whirlpool",
                orca::Whirlpool::from_bytes,
            )?;
            let moved =
                core::get_tick_array_start_tick_index(whirlpool.tick_current_index, spacing)
                    != start;
            bundle.whirlpool = whirlpool;
            if moved {
                continue;
            }
            let mut facades = starts.map(empty_tick_array);
            for (index, account) in accounts[1..].iter().enumerate() {
                let Some(account) = account else { continue };
                let array: orca::TickArray = self.decode_program_account(
                    keys[index],
                    account,
                    "TickArray",
                    orca::TickArray::from_bytes,
                )?;
                if current_pubkey(array.whirlpool) != bundle.key
                    || array.start_tick_index != starts[index]
                {
                    return Err(OrcaWhirlpoolError::MalformedAccount {
                        address: keys[index],
                        reason: "tick array pool or start index mismatch".to_owned(),
                    });
                }
                facades[index] = array.into();
            }
            return Ok((keys, facades));
        }
        Err(OrcaWhirlpoolError::Quote(
            "pool price kept crossing tick arrays while loading".to_owned(),
        ))
    }

    async fn missing_tick_array_instructions(
        &self,
        accounts: &PositionAccounts,
    ) -> Result<Vec<Instruction>> {
        let keys = [accounts.tick_array_lower, accounts.tick_array_upper];
        let existing = self.fetch_optional(&keys).await?;
        let mut instructions = Vec::new();
        for (index, start) in [
            accounts.tick_array_lower_start,
            accounts.tick_array_upper_start,
        ]
        .into_iter()
        .enumerate()
        {
            if existing[index].is_none() && !(index == 1 && keys[0] == keys[1]) {
                instructions.push(self.initialize_tick_array_instruction(
                    accounts.pool,
                    start,
                    keys[index],
                ));
            }
        }
        Ok(instructions)
    }

    async fn require_tick_arrays(&self, accounts: &PositionAccounts) -> Result<()> {
        let keys = [accounts.tick_array_lower, accounts.tick_array_upper];
        let existing = self.fetch_optional(&keys).await?;
        for (index, account) in existing.iter().enumerate() {
            let account = account
                .as_ref()
                .ok_or(OrcaWhirlpoolError::MalformedAccount {
                    address: keys[index],
                    reason: "position tick array is missing".to_owned(),
                })?;
            let array: orca::TickArray = self.decode_program_account(
                keys[index],
                account,
                "TickArray",
                orca::TickArray::from_bytes,
            )?;
            if current_pubkey(array.whirlpool) != accounts.pool {
                return Err(OrcaWhirlpoolError::MalformedAccount {
                    address: keys[index],
                    reason: "position tick array belongs to a different pool".to_owned(),
                });
            }
        }
        Ok(())
    }

    fn initialize_tick_array_instruction(
        &self,
        pool: Pubkey,
        start_tick_index: i32,
        tick_array: Pubkey,
    ) -> Instruction {
        let ix = orca::InitializeTickArray {
            whirlpool: legacy_pubkey(pool),
            funder: legacy_pubkey(self.payer()),
            tick_array: legacy_pubkey(tick_array),
            system_program: legacy_pubkey(spl::SYSTEM_PROGRAM_ID),
        }
        .instruction(orca::InitializeTickArrayInstructionArgs { start_tick_index });
        instruction(ix, self.program_id)
    }

    fn position_ata_instructions(&self, bundle: &PoolBundle) -> Vec<Instruction> {
        [&bundle.mint_a, &bundle.mint_b]
            .into_iter()
            .map(|mint| {
                spl::create_associated_token_account_idempotent(
                    &self.payer(),
                    &self.payer(),
                    &mint.key,
                    &mint.token_program,
                )
            })
            .collect()
    }

    fn position_addresses(
        &self,
        pool: Pubkey,
        mint: Pubkey,
        tick_lower: i32,
        tick_upper: i32,
        spacing: u16,
    ) -> PositionAccounts {
        let lower_start = core::get_tick_array_start_tick_index(tick_lower, spacing);
        let upper_start = core::get_tick_array_start_tick_index(tick_upper, spacing);
        PositionAccounts {
            pool,
            position_mint: mint,
            position_token_account: spl::associated_token_address(
                &self.payer(),
                &mint,
                &spl::TOKEN_2022_PROGRAM_ID,
            ),
            position: self.position_address(mint),
            tick_array_lower: self.tick_array_address(pool, lower_start),
            tick_array_upper: self.tick_array_address(pool, upper_start),
            tick_array_lower_start: lower_start,
            tick_array_upper_start: upper_start,
        }
    }

    fn fee_tier_address(&self, config: Pubkey, tick_spacing: u16) -> Pubkey {
        pda(
            &[b"fee_tier", config.as_ref(), &tick_spacing.to_le_bytes()],
            self.program_id,
        )
    }

    fn token_badge_address(&self, config: Pubkey, mint: Pubkey) -> Pubkey {
        pda(
            &[b"token_badge", config.as_ref(), mint.as_ref()],
            self.program_id,
        )
    }

    fn position_address(&self, mint: Pubkey) -> Pubkey {
        pda(&[b"position", mint.as_ref()], self.program_id)
    }

    fn oracle_address(&self, pool: Pubkey) -> Pubkey {
        pda(&[b"oracle", pool.as_ref()], self.program_id)
    }

    fn tick_array_address(&self, pool: Pubkey, start: i32) -> Pubkey {
        tick_array_address_for(self.program_id, pool, start)
    }
}

/// Pool, mints, and tick arrays fetched together by
/// [`OrcaWhirlpoolClient::load_swap_snapshot`].
#[derive(Clone)]
pub struct SwapSnapshot {
    program_id: Pubkey,
    oracle: Pubkey,
    bundle: PoolBundle,
    tick_arrays: [Pubkey; 5],
    facades: [TickArrayFacade; 5],
}

impl SwapSnapshot {
    pub fn pool(&self) -> Pubkey {
        self.bundle.key
    }

    pub fn program_id(&self) -> Pubkey {
        self.program_id
    }

    /// Pool mints in pool order (A, B).
    pub fn mints(&self) -> (Pubkey, Pubkey) {
        (self.bundle.mint_a.key, self.bundle.mint_b.key)
    }

    pub fn token_programs(&self) -> (Pubkey, Pubkey) {
        (
            self.bundle.mint_a.token_program,
            self.bundle.mint_b.token_program,
        )
    }

    /// Trade fee in hundredths of a basis point (1,000,000 denominator).
    pub fn fee_rate(&self) -> u16 {
        self.bundle.whirlpool.fee_rate
    }

    fn a_to_b(&self, input_mint: Pubkey, output_mint: Pubkey) -> Result<bool> {
        let bundle = &self.bundle;
        if input_mint == bundle.mint_a.key && output_mint == bundle.mint_b.key {
            Ok(true)
        } else if input_mint == bundle.mint_b.key && output_mint == bundle.mint_a.key {
            Ok(false)
        } else {
            Err(OrcaWhirlpoolError::InvalidInput(
                "swap mints do not match the pool".to_owned(),
            ))
        }
    }

    /// Quote a swap in raw atoms: `amount_raw` is the input for exact-in and
    /// the output for exact-out.
    pub fn quote(
        &self,
        input_mint: Pubkey,
        output_mint: Pubkey,
        amount_raw: u64,
        exact_in: bool,
        slippage_bps: u16,
    ) -> Result<SwapQuote> {
        if amount_raw == 0 {
            return Err(OrcaWhirlpoolError::InvalidInput(
                "swap amount must be greater than zero".to_owned(),
            ));
        }
        if slippage_bps > 10_000 {
            return Err(OrcaWhirlpoolError::InvalidInput(
                "slippage_bps cannot exceed 10,000".to_owned(),
            ));
        }
        let a_to_b = self.a_to_b(input_mint, output_mint)?;
        let BoundedSwapQuote {
            expected_input_raw,
            expected_output_raw,
            other_amount_threshold,
            max_input_raw,
        } = bounded_swap_quote(
            &self.bundle,
            self.facades,
            amount_raw,
            a_to_b,
            exact_in,
            slippage_bps,
        )?;
        Ok(SwapQuote {
            exact_in,
            input_mint,
            output_mint,
            instruction_amount: amount_raw,
            expected_input_raw,
            expected_output_raw,
            other_amount_threshold,
            max_input_raw,
            tick_arrays: self.tick_arrays.to_vec(),
        })
    }

    /// `SwapV2` accounts for `owner` in instruction order, followed by the two
    /// supplemental tick arrays.
    pub fn swap_account_metas(&self, owner: Pubkey) -> Vec<AccountMeta> {
        self.swap_instruction(owner, 0, 0, true, true).accounts
    }

    pub fn swap_accounts(&self, owner: Pubkey, quote: &SwapQuote) -> Result<SwapAccounts> {
        let a_to_b = self.a_to_b(quote.input_mint, quote.output_mint)?;
        let (user_a, user_b) = self.owner_accounts(owner);
        let (input_token_account, output_token_account) = if a_to_b {
            (user_a, user_b)
        } else {
            (user_b, user_a)
        };
        Ok(SwapAccounts {
            pool: self.bundle.key,
            input_token_account,
            output_token_account,
            token_vault_a: self.bundle.token_vault_a,
            token_vault_b: self.bundle.token_vault_b,
            oracle: self.oracle,
            tick_arrays: self.tick_arrays.to_vec(),
        })
    }

    /// Idempotent ATA creation for both mints plus one `SwapV2` for `owner`.
    pub fn swap_instructions(&self, owner: Pubkey, quote: &SwapQuote) -> Result<Vec<Instruction>> {
        let a_to_b = self.a_to_b(quote.input_mint, quote.output_mint)?;
        let mut instructions: Vec<Instruction> = [&self.bundle.mint_a, &self.bundle.mint_b]
            .into_iter()
            .map(|mint| {
                spl::create_associated_token_account_idempotent(
                    &owner,
                    &owner,
                    &mint.key,
                    &mint.token_program,
                )
            })
            .collect();
        instructions.push(self.swap_instruction(
            owner,
            quote.instruction_amount,
            quote.other_amount_threshold,
            quote.exact_in,
            a_to_b,
        ));
        Ok(instructions)
    }

    fn owner_accounts(&self, owner: Pubkey) -> (Pubkey, Pubkey) {
        let bundle = &self.bundle;
        (
            spl::associated_token_address(&owner, &bundle.mint_a.key, &bundle.mint_a.token_program),
            spl::associated_token_address(&owner, &bundle.mint_b.key, &bundle.mint_b.token_program),
        )
    }

    fn swap_instruction(
        &self,
        owner: Pubkey,
        amount: u64,
        other_amount_threshold: u64,
        exact_in: bool,
        a_to_b: bool,
    ) -> Instruction {
        let bundle = &self.bundle;
        let (user_a, user_b) = self.owner_accounts(owner);
        let swap = orca::SwapV2 {
            token_program_a: legacy_pubkey(bundle.mint_a.token_program),
            token_program_b: legacy_pubkey(bundle.mint_b.token_program),
            memo_program: legacy_pubkey(spl::MEMO_PROGRAM_ID),
            token_authority: legacy_pubkey(owner),
            whirlpool: legacy_pubkey(bundle.key),
            token_mint_a: legacy_pubkey(bundle.mint_a.key),
            token_mint_b: legacy_pubkey(bundle.mint_b.key),
            token_owner_account_a: legacy_pubkey(user_a),
            token_vault_a: legacy_pubkey(bundle.token_vault_a),
            token_owner_account_b: legacy_pubkey(user_b),
            token_vault_b: legacy_pubkey(bundle.token_vault_b),
            tick_array0: legacy_pubkey(self.tick_arrays[0]),
            tick_array1: legacy_pubkey(self.tick_arrays[1]),
            tick_array2: legacy_pubkey(self.tick_arrays[2]),
            oracle: legacy_pubkey(self.oracle),
        }
        .instruction_with_remaining_accounts(
            orca::SwapV2InstructionArgs {
                amount,
                other_amount_threshold,
                sqrt_price_limit: 0,
                amount_specified_is_input: exact_in,
                a_to_b,
                remaining_accounts_info: Some(orca::RemainingAccountsInfo {
                    slices: vec![orca::RemainingAccountsSlice {
                        accounts_type: orca::AccountsType::SupplementalTickArrays,
                        length: 2,
                    }],
                }),
            },
            &[
                legacy_meta(AccountMeta::new(self.tick_arrays[3], false)),
                legacy_meta(AccountMeta::new(self.tick_arrays[4], false)),
            ],
        );
        instruction(swap, self.program_id)
    }
}

/// Tick-array PDA. Orca seeds the start index as its decimal string.
pub fn tick_array_address_for(program_id: Pubkey, pool: Pubkey, start: i32) -> Pubkey {
    pda(
        &[b"tick_array", pool.as_ref(), start.to_string().as_bytes()],
        program_id,
    )
}

fn pool_address_for(
    program_id: Pubkey,
    config: Pubkey,
    mint_a: Pubkey,
    mint_b: Pubkey,
    tick_spacing: u16,
) -> Pubkey {
    pda(
        &[
            b"whirlpool",
            config.as_ref(),
            mint_a.as_ref(),
            mint_b.as_ref(),
            &tick_spacing.to_le_bytes(),
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
pub struct EnsureConfigParams {
    /// Basis points of the trade fee taken by the protocol (max 2,500).
    pub default_protocol_fee_rate: u16,
    pub tick_spacing: u16,
    /// Hundredths of a basis point (1,000,000 denominator; max 30,000).
    pub default_fee_rate: u16,
}

#[derive(Clone, Debug)]
pub struct EnsureConfigOutcome {
    pub config_transaction: Option<TransactionOutcome>,
    pub fee_tier_transaction: Option<TransactionOutcome>,
    pub created_config: bool,
    pub created_fee_tier: bool,
    pub whirlpools_config: Pubkey,
    pub fee_tier: Pubkey,
    pub admin: Pubkey,
    pub params: EnsureConfigParams,
}

#[derive(Clone, Copy, Debug)]
pub struct TokenAmount {
    pub mint: Pubkey,
    pub amount: Decimal,
}

#[derive(Clone, Copy, Debug)]
pub struct CreatePoolParams {
    pub whirlpools_config: Pubkey,
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub tick_spacing: u16,
    /// Mint B per mint A as supplied; normalized to pool mint order.
    pub initial_price: Decimal,
}

#[derive(Clone, Copy, Debug)]
pub struct OpenPositionParams {
    pub pool: Pubkey,
    /// Pool-order price (pool mint B per pool mint A).
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
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub normalized_initial_price: Decimal,
    pub sqrt_price: u128,
    pub tick_spacing: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct PoolAccounts {
    pub pool: Pubkey,
    pub token_vault_a: Pubkey,
    pub token_vault_b: Pubkey,
    pub fee_tier: Pubkey,
    pub oracle: Pubkey,
}

#[derive(Clone, Debug)]
pub struct TickArraySeedOutcome {
    pub pool: Pubkey,
    pub tick_arrays: Vec<Pubkey>,
    pub initialized: Vec<Pubkey>,
    pub transactions: Vec<TransactionOutcome>,
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
    pub amount_a_raw: u64,
    pub amount_b_raw: u64,
    pub amount_a_max: u64,
    pub amount_b_max: u64,
    pub amount_a_min: u64,
    pub amount_b_min: u64,
}

impl From<IncreaseLiquidityQuote> for LiquidityQuote {
    fn from(quote: IncreaseLiquidityQuote) -> Self {
        Self {
            liquidity: quote.liquidity_delta,
            tick_lower: 0,
            tick_upper: 0,
            amount_a_raw: quote.token_est_a,
            amount_b_raw: quote.token_est_b,
            amount_a_max: quote.token_max_a,
            amount_b_max: quote.token_max_b,
            amount_a_min: 0,
            amount_b_min: 0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PositionAccounts {
    pub pool: Pubkey,
    pub position_mint: Pubkey,
    pub position_token_account: Pubkey,
    pub position: Pubkey,
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
    pub closed_position_mint: Pubkey,
    pub closed_position_token_account: Pubkey,
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
    /// Minimum output (exact-in) or maximum input (exact-out) enforced on-chain.
    pub other_amount_threshold: u64,
    pub max_input_raw: u64,
    pub tick_arrays: Vec<Pubkey>,
}

#[derive(Clone, Debug)]
pub struct SwapAccounts {
    pub pool: Pubkey,
    pub input_token_account: Pubkey,
    pub output_token_account: Pubkey,
    pub token_vault_a: Pubkey,
    pub token_vault_b: Pubkey,
    pub oracle: Pubkey,
    pub tick_arrays: Vec<Pubkey>,
}

#[derive(Debug, Error)]
pub enum OrcaWhirlpoolError {
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
    freeze_authority: Option<Pubkey>,
    data: Vec<u8>,
}

#[derive(Clone)]
struct PoolBundle {
    whirlpool: orca::Whirlpool,
    mint_a: MintInfo,
    mint_b: MintInfo,
    token_vault_a: Pubkey,
    token_vault_b: Pubkey,
    user_token_a: Pubkey,
    user_token_b: Pubkey,
    balance_a: u64,
    balance_b: u64,
    transfer_fee_a: Option<TransferFee>,
    transfer_fee_b: Option<TransferFee>,
    key: Pubkey,
}

impl PoolBundle {
    /// Current sqrt price and the mints' transfer fees, as the liquidity
    /// bound helpers take them.
    fn price_and_fees(&self) -> (u128, Option<core::TransferFee>, Option<core::TransferFee>) {
        (
            self.whirlpool.sqrt_price,
            self.transfer_fee_a,
            self.transfer_fee_b,
        )
    }

    fn mint_for(&self, mint: Pubkey) -> Result<&MintInfo> {
        if mint == self.mint_a.key {
            Ok(&self.mint_a)
        } else if mint == self.mint_b.key {
            Ok(&self.mint_b)
        } else {
            Err(OrcaWhirlpoolError::InvalidInput(
                "mint does not belong to the pool".to_owned(),
            ))
        }
    }
}

struct PositionView {
    address: Pubkey,
    pool: Pubkey,
    token_account: Pubkey,
    tick_lower: i32,
    tick_upper: i32,
    liquidity: u128,
    fee_owed_a: u64,
    fee_owed_b: u64,
    fee_checkpoint: (u128, u128),
    rewards_owed: [u64; 3],
}

fn ordered_mints(mint_a: Pubkey, mint_b: Pubkey) -> Result<(Pubkey, Pubkey)> {
    match mint_a.cmp(&mint_b) {
        std::cmp::Ordering::Less => Ok((mint_a, mint_b)),
        std::cmp::Ordering::Greater => Ok((mint_b, mint_a)),
        std::cmp::Ordering::Equal => Err(OrcaWhirlpoolError::InvalidInput(
            "pool mints must be different".to_owned(),
        )),
    }
}

fn require_account<'a>(
    keys: &[Pubkey],
    accounts: &'a [Option<Account>],
    index: usize,
    label: &str,
) -> Result<&'a Account> {
    accounts.get(index).and_then(Option::as_ref).ok_or_else(|| {
        OrcaWhirlpoolError::MalformedAccount {
            address: keys.get(index).copied().unwrap_or_default(),
            reason: format!("missing {label}"),
        }
    })
}

fn decode_mint(key: Pubkey, account: &Account) -> Result<MintInfo> {
    let summary = spl::decode_mint(account.owner, &account.data).ok_or_else(|| {
        OrcaWhirlpoolError::UnsupportedToken {
            mint: key,
            reason: format!("unsupported token program {}", account.owner),
        }
    })?;
    Ok(MintInfo {
        key,
        token_program: summary.token_program,
        decimals: summary.decimals,
        freeze_authority: summary.freeze_authority,
        data: account.data.clone(),
    })
}

/// Mirror of the program's `is_supported_token_mint` for mints without a
/// token badge, so unsupported pairs fail before a transaction is built.
fn validate_supported_mint(mint: &MintInfo, has_badge: bool) -> Result<()> {
    if mint.token_program == spl::TOKEN_PROGRAM_ID || has_badge {
        return Ok(());
    }
    if mint.freeze_authority.is_some() {
        return Err(OrcaWhirlpoolError::UnsupportedToken {
            mint: mint.key,
            reason: "Token-2022 mints with a freeze authority need a token badge".to_owned(),
        });
    }
    let state = StateWithExtensions::<Mint>::unpack(&mint.data).map_err(|error| {
        OrcaWhirlpoolError::MalformedAccount {
            address: mint.key,
            reason: error.to_string(),
        }
    })?;
    let allowed = [
        ExtensionType::TransferFeeConfig,
        ExtensionType::InterestBearingConfig,
        ExtensionType::TokenMetadata,
        ExtensionType::MetadataPointer,
        ExtensionType::ConfidentialTransferMint,
        ExtensionType::ConfidentialTransferFeeConfig,
    ];
    let unsupported: Vec<String> = state
        .get_extension_types()
        .map_err(|error| OrcaWhirlpoolError::MalformedAccount {
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
        Err(OrcaWhirlpoolError::UnsupportedToken {
            mint: mint.key,
            reason: format!(
                "Token-2022 extensions need a token badge: {}",
                unsupported.join(", ")
            ),
        })
    }
}

fn validate_vault(pool: Pubkey, key: Pubkey, account: &Account, mint: &MintInfo) -> Result<()> {
    if account.owner != mint.token_program {
        return Err(OrcaWhirlpoolError::MalformedAccount {
            address: key,
            reason: "vault is owned by the wrong token program".to_owned(),
        });
    }
    let state = spl::decode_token_account(&account.data).ok_or_else(|| {
        OrcaWhirlpoolError::MalformedAccount {
            address: key,
            reason: "vault data is too short".to_owned(),
        }
    })?;
    if state.mint != mint.key || state.owner != pool {
        return Err(OrcaWhirlpoolError::MalformedAccount {
            address: key,
            reason: "vault mint or authority does not match the pool".to_owned(),
        });
    }
    Ok(())
}

/// The wallet must cover what the program transfers at the quoted price
/// (`token_est_*`, transfer fee included), not the slippage maximum: the
/// program moves the computed amount and only checks it against the max.
const CLOCK_SYSVAR_ID: Pubkey =
    Pubkey::from_str_const("SysvarC1ock11111111111111111111111111111111");

/// Epoch from the Clock sysvar (`slot: u64, epoch_start_timestamp: i64,
/// epoch: u64, ...`), the epoch Token-2022 uses for transfer fees.
fn clock_epoch(account: &Account) -> Result<u64> {
    account
        .data
        .get(16..24)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| OrcaWhirlpoolError::MalformedAccount {
            address: CLOCK_SYSVAR_ID,
            reason: "clock sysvar is shorter than its layout".to_owned(),
        })
}

fn ensure_balances(bundle: &PoolBundle, quote: &IncreaseLiquidityQuote) -> Result<()> {
    for (mint, required, available) in [
        (&bundle.mint_a, quote.token_est_a, bundle.balance_a),
        (&bundle.mint_b, quote.token_est_b, bundle.balance_b),
    ] {
        if available < required {
            return Err(OrcaWhirlpoolError::InsufficientBalance {
                mint: mint.key,
                required,
                available,
            });
        }
    }
    Ok(())
}

/// Sqrt prices for a ±`bps` price move around `sqrt_price`:
/// `[√P·√(1−s), √P·√(1+s)]`, clamped to the program's sqrt-price range.
fn sqrt_price_band(sqrt_price: u128, bps: u16) -> Result<(u128, u128)> {
    let band = Decimal::new(i64::from(bps.min(10_000)), 4);
    let q64 = Decimal::from(1_u64 << 32) * Decimal::from(1_u64 << 32);
    let factor = |price_factor: Decimal, round_up: bool| {
        price_factor
            .sqrt()
            .and_then(|root| root.checked_mul(q64))
            .and_then(|scaled| {
                if round_up {
                    scaled.ceil()
                } else {
                    scaled.floor()
                }
                .to_u128()
            })
            .ok_or_else(|| OrcaWhirlpoolError::Quote("slippage band overflow".to_owned()))
    };
    let low = mtm_math::clmm::mul_shr_64(sqrt_price, factor(Decimal::ONE - band, false)?);
    let high = mtm_math::clmm::mul_shr_64(sqrt_price, factor(Decimal::ONE + band, true)?)
        .saturating_add(1);
    Ok((
        low.clamp(core::MIN_SQRT_PRICE, core::MAX_SQRT_PRICE),
        high.clamp(core::MIN_SQRT_PRICE, core::MAX_SQRT_PRICE),
    ))
}

/// Token amounts `liquidity` represents over `[tick_lower, tick_upper)` at
/// `sqrt_price`: all A below the range, all B above it.
fn position_amounts(
    liquidity: u128,
    sqrt_price: u128,
    tick_lower: i32,
    tick_upper: i32,
    round_up: bool,
) -> Result<(u64, u64)> {
    let lower: u128 = core::tick_index_to_sqrt_price(tick_lower);
    let upper: u128 = core::tick_index_to_sqrt_price(tick_upper);
    let delta_a = |from: u128| core::try_get_amount_delta_a(from, upper, liquidity, round_up);
    let delta_b = |to: u128| core::try_get_amount_delta_b(lower, to, liquidity, round_up);
    let amounts = if sqrt_price <= lower {
        (delta_a(lower), Ok(0))
    } else if sqrt_price >= upper {
        (Ok(0), delta_b(upper))
    } else {
        (delta_a(sqrt_price), delta_b(sqrt_price))
    };
    Ok((
        amounts.0.map_err(quote_error)?,
        amounts.1.map_err(quote_error)?,
    ))
}

/// Deposit maxima that hold over the whole slippage band, not just at the
/// current price. The liquidity is fixed, so the program's required A grows
/// as the price falls and the required B as it rises; `est × (1 + s)` at the
/// current price fails on far smaller moves for in-range positions, and
/// allows zero of the missing side for a position quoted out of range.
fn widen_increase_bounds(
    mut quote: IncreaseLiquidityQuote,
    (sqrt_price, fee_a, fee_b): (u128, Option<core::TransferFee>, Option<core::TransferFee>),
    tick_lower: i32,
    tick_upper: i32,
    bps: u16,
) -> Result<IncreaseLiquidityQuote> {
    let (low, high) = sqrt_price_band(sqrt_price, bps)?;
    let liquidity = quote.liquidity_delta;
    let (a_at_low, _) = position_amounts(liquidity, low, tick_lower, tick_upper, true)?;
    let (_, b_at_high) = position_amounts(liquidity, high, tick_lower, tick_upper, true)?;
    let gross = |amount, fee: Option<core::TransferFee>| {
        core::try_reverse_apply_transfer_fee(amount, fee.unwrap_or_default()).map_err(quote_error)
    };
    quote.token_max_a = quote.token_max_a.max(gross(a_at_low, fee_a)?);
    quote.token_max_b = quote.token_max_b.max(gross(b_at_high, fee_b)?);
    Ok(quote)
}

/// Withdrawal minima that hold over the whole slippage band: A is lowest at
/// the top of the band and B at the bottom.
fn widen_decrease_bounds(
    mut quote: core::DecreaseLiquidityQuote,
    (sqrt_price, fee_a, fee_b): (u128, Option<core::TransferFee>, Option<core::TransferFee>),
    tick_lower: i32,
    tick_upper: i32,
    bps: u16,
) -> Result<core::DecreaseLiquidityQuote> {
    let (low, high) = sqrt_price_band(sqrt_price, bps)?;
    let liquidity = quote.liquidity_delta;
    let (a_at_high, _) = position_amounts(liquidity, high, tick_lower, tick_upper, false)?;
    let (_, b_at_low) = position_amounts(liquidity, low, tick_lower, tick_upper, false)?;
    let net = |amount, fee: Option<core::TransferFee>| {
        core::try_apply_transfer_fee(amount, fee.unwrap_or_default()).map_err(quote_error)
    };
    quote.token_min_a = quote.token_min_a.min(net(a_at_high, fee_a)?);
    quote.token_min_b = quote.token_min_b.min(net(b_at_low, fee_b)?);
    Ok(quote)
}

fn current_transfer_fee(mint: &MintInfo, epoch: u64) -> Result<Option<TransferFee>> {
    if mint.token_program == spl::TOKEN_PROGRAM_ID {
        return Ok(None);
    }
    let state = StateWithExtensions::<Mint>::unpack(&mint.data).map_err(|error| {
        OrcaWhirlpoolError::MalformedAccount {
            address: mint.key,
            reason: error.to_string(),
        }
    })?;
    Ok(state
        .get_extension::<TransferFeeConfig>()
        .ok()
        .map(|config| {
            let fee = config.get_epoch_fee(epoch);
            TransferFee::new_with_max(
                u16::from(fee.transfer_fee_basis_points),
                u64::from(fee.maximum_fee),
            )
        }))
}

fn validate_config_params(params: EnsureConfigParams) -> Result<()> {
    if params.tick_spacing == 0 {
        return Err(OrcaWhirlpoolError::InvalidInput(
            "tick_spacing must be greater than zero".to_owned(),
        ));
    }
    if params.default_protocol_fee_rate > PROTOCOL_FEE_RATE_MAX {
        return Err(OrcaWhirlpoolError::InvalidInput(format!(
            "default_protocol_fee_rate cannot exceed {PROTOCOL_FEE_RATE_MAX}"
        )));
    }
    if params.default_fee_rate > FEE_RATE_MAX {
        return Err(OrcaWhirlpoolError::InvalidInput(format!(
            "default_fee_rate cannot exceed {FEE_RATE_MAX}"
        )));
    }
    Ok(())
}

fn ensure_positive(value: Decimal, label: &str) -> Result<()> {
    if value <= Decimal::ZERO {
        Err(OrcaWhirlpoolError::InvalidInput(format!(
            "{label} must be greater than zero"
        )))
    } else {
        Ok(())
    }
}

fn ui_to_raw(amount: Decimal, decimals: u8) -> Result<u64> {
    spl::ui_to_raw(amount, decimals).map_err(OrcaWhirlpoolError::InvalidInput)
}

/// Q64.64 sqrt price for a UI price (token B per token A), without floats.
fn price_to_sqrt_x64(price: Decimal, decimals_a: u8, decimals_b: u8) -> Result<u128> {
    ensure_positive(price, "price")?;
    let scale_a = Decimal::from_u128(10_u128.pow(u32::from(decimals_a)))
        .ok_or_else(|| OrcaWhirlpoolError::Quote("mint A decimal scale overflow".to_owned()))?;
    let scale_b = Decimal::from_u128(10_u128.pow(u32::from(decimals_b)))
        .ok_or_else(|| OrcaWhirlpoolError::Quote("mint B decimal scale overflow".to_owned()))?;
    let raw_price = price
        .checked_mul(scale_b)
        .and_then(|value| value.checked_div(scale_a))
        .ok_or_else(|| {
            OrcaWhirlpoolError::Quote("price decimal normalization failed".to_owned())
        })?;
    let sqrt = raw_price
        .sqrt()
        .ok_or_else(|| OrcaWhirlpoolError::Quote("price square root failed".to_owned()))?;
    let sqrt_price = sqrt
        .checked_mul(
            Decimal::from_u128(1_u128 << 64)
                .ok_or_else(|| OrcaWhirlpoolError::Quote("Q64 conversion failed".to_owned()))?,
        )
        .and_then(|value| value.floor().to_u128())
        .ok_or_else(|| OrcaWhirlpoolError::Quote("sqrt price does not fit u128".to_owned()))?;
    if !(core::MIN_SQRT_PRICE..=core::MAX_SQRT_PRICE).contains(&sqrt_price) {
        return Err(OrcaWhirlpoolError::Quote(
            "price is outside the supported sqrt-price bounds".to_owned(),
        ));
    }
    Ok(sqrt_price)
}

struct BoundedSwapQuote {
    expected_input_raw: u64,
    expected_output_raw: u64,
    other_amount_threshold: u64,
    max_input_raw: u64,
}

/// Mirror of `swap_quote_by_{input,output}_token` that pins the sqrt-price
/// limit to the edge of the loaded tick-array sequence.
///
/// orca_whirlpools_core 1.0.4's `compute_swap` never terminates once the walk
/// runs past the last loaded array (the sequence keeps returning its boundary
/// tick), e.g. for an empty pool. On-chain the same swap fails, so a walk that
/// cannot fill completely inside the loaded arrays is reported as a quote error.
fn bounded_swap_quote(
    bundle: &PoolBundle,
    facades: [TickArrayFacade; 5],
    amount: u64,
    a_to_b: bool,
    exact_in: bool,
    slippage_bps: u16,
) -> Result<BoundedSwapQuote> {
    let spacing = bundle.whirlpool.tick_spacing;
    let sequence = TickArraySequence::new(facades.map(Some), spacing).map_err(quote_error)?;
    let sqrt_price_limit = if a_to_b {
        core::tick_index_to_sqrt_price(sequence.start_index()).max(core::MIN_SQRT_PRICE)
    } else {
        core::tick_index_to_sqrt_price(sequence.end_index()).min(core::MAX_SQRT_PRICE)
    };
    let (fee_in, fee_out) = if a_to_b {
        (bundle.transfer_fee_a, bundle.transfer_fee_b)
    } else {
        (bundle.transfer_fee_b, bundle.transfer_fee_a)
    };
    let whirlpool: core::WhirlpoolFacade = bundle.whirlpool.clone().into();
    let crosses_limit = if a_to_b {
        whirlpool.sqrt_price <= sqrt_price_limit
    } else {
        whirlpool.sqrt_price >= sqrt_price_limit
    };
    if crosses_limit {
        return Err(OrcaWhirlpoolError::Quote(
            "current price sits outside the loaded tick arrays".to_owned(),
        ));
    }
    let specified = if exact_in {
        core::try_apply_transfer_fee(amount, fee_in.unwrap_or_default())
    } else {
        core::try_reverse_apply_transfer_fee(amount, fee_out.unwrap_or_default())
    }
    .map_err(quote_error)?;
    let result = core::compute_swap(
        specified,
        sqrt_price_limit,
        whirlpool,
        sequence,
        a_to_b,
        exact_in,
        0,
    )
    .map_err(quote_error)?;
    let (amount_in, amount_out) = if a_to_b {
        (result.token_a, result.token_b)
    } else {
        (result.token_b, result.token_a)
    };
    let filled = if exact_in { amount_in } else { amount_out };
    if filled != specified {
        return Err(OrcaWhirlpoolError::Quote(format!(
            "loaded tick arrays absorb only {filled} of {specified} atoms"
        )));
    }
    if exact_in {
        let expected_output_raw =
            core::try_apply_transfer_fee(amount_out, fee_out.unwrap_or_default())
                .map_err(quote_error)?;
        let minimum =
            core::try_get_min_amount_with_slippage_tolerance(expected_output_raw, slippage_bps)
                .map_err(quote_error)?;
        if expected_output_raw == 0 {
            return Err(OrcaWhirlpoolError::Quote(
                "swap amount quotes to zero output".to_owned(),
            ));
        }
        Ok(BoundedSwapQuote {
            expected_input_raw: amount,
            expected_output_raw,
            other_amount_threshold: minimum,
            max_input_raw: amount,
        })
    } else {
        let expected_input_raw =
            core::try_reverse_apply_transfer_fee(amount_in, fee_in.unwrap_or_default())
                .map_err(quote_error)?;
        let maximum =
            core::try_get_max_amount_with_slippage_tolerance(expected_input_raw, slippage_bps)
                .map_err(quote_error)?;
        Ok(BoundedSwapQuote {
            expected_input_raw,
            expected_output_raw: amount,
            other_amount_threshold: maximum,
            max_input_raw: maximum,
        })
    }
}

fn tick_array_span(tick_spacing: u16) -> i32 {
    i32::from(tick_spacing) * TICK_ARRAY_SIZE as i32
}

fn empty_tick_array(start_tick_index: i32) -> TickArrayFacade {
    TickArrayFacade {
        start_tick_index,
        ticks: [TickFacade::default(); TICK_ARRAY_SIZE],
    }
}

fn quote_error(error: core::CoreError) -> OrcaWhirlpoolError {
    OrcaWhirlpoolError::Quote(error.to_owned())
}

/// Anchor account discriminator: `sha256("account:<Name>")[..8]`.
/// Anchor account discriminator: the first 8 bytes of
/// `sha256("account:<Name>")`. Cached for the accounts this client decodes,
/// which are read on every quote (the generated client's constants are
/// zeroed).
fn account_discriminator(name: &str) -> [u8; 8] {
    static KNOWN: std::sync::LazyLock<Vec<(&'static str, [u8; 8])>> =
        std::sync::LazyLock::new(|| {
            [
                "Whirlpool",
                "TickArray",
                "Position",
                "FeeTier",
                "WhirlpoolsConfig",
            ]
            .map(|name| (name, compute_discriminator(name)))
            .to_vec()
        });
    KNOWN.iter().find(|(known, _)| *known == name).map_or_else(
        || compute_discriminator(name),
        |(_, discriminator)| *discriminator,
    )
}

fn compute_discriminator(name: &str) -> [u8; 8] {
    let digest = Sha256::digest(format!("account:{name}").as_bytes());
    let mut discriminator = [0_u8; 8];
    discriminator.copy_from_slice(&digest[..8]);
    discriminator
}

fn pda(seeds: &[&[u8]], program_id: Pubkey) -> Pubkey {
    Pubkey::find_program_address(seeds, &program_id).0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdas_match_the_generated_client_for_the_canonical_program() {
        let pool = Pubkey::from_str_const("2kJmUjxWBwL2NGPBV2PiA5hWtmLCqcKY6reQgkrPtaeS");
        let expected = Pubkey::from_str_const("8PhPzk7n4wU98Z6XCbVtPai2LtXSxYnfjkmgWuoAU8Zy");
        assert_eq!(
            tick_array_address_for(CANONICAL_PROGRAM_ID, pool, 0),
            expected
        );
        let (generated, _) =
            orca::get_tick_array_address(&legacy_pubkey(pool), -5632).expect("derives");
        assert_eq!(
            tick_array_address_for(CANONICAL_PROGRAM_ID, pool, -5632),
            current_pubkey(generated)
        );
        let config = Pubkey::new_from_array([3; 32]);
        let (mint_a, mint_b) = (
            Pubkey::new_from_array([1; 32]),
            Pubkey::new_from_array([2; 32]),
        );
        let (generated, _) = orca::get_whirlpool_address(
            &legacy_pubkey(config),
            &legacy_pubkey(mint_a),
            &legacy_pubkey(mint_b),
            64,
        )
        .expect("derives");
        assert_eq!(
            derive_pool_address(CANONICAL_PROGRAM_ID, config, mint_b, mint_a, 64).expect("pool"),
            current_pubkey(generated)
        );
    }

    #[test]
    fn discriminators_match_anchor() {
        // sha256("account:Whirlpool")[..8], as hard-coded in Orca's TS SDK.
        assert_eq!(
            account_discriminator("Whirlpool"),
            [63, 149, 209, 12, 225, 128, 99, 9]
        );
    }

    #[test]
    fn sqrt_price_round_trips_through_ticks() {
        let sqrt = price_to_sqrt_x64(Decimal::ONE, 6, 6).expect("price 1");
        assert_eq!(sqrt, 1_u128 << 64);
        assert_eq!(core::sqrt_price_to_tick_index(sqrt), 0);
        let doubled = price_to_sqrt_x64(Decimal::TWO, 9, 6).expect("price 2");
        let tick = core::sqrt_price_to_tick_index(doubled);
        // 2 × 10^-3 raw price ≈ tick ln(0.002)/ln(1.0001)
        assert!((-62_150..=-62_140).contains(&tick), "tick {tick}");
    }

    #[test]
    fn empty_pool_quotes_terminate_with_an_error() {
        let span = tick_array_span(64);
        assert_eq!(span, 5_632);
        let arrays = [0, span, 2 * span, -span, -2 * span].map(empty_tick_array);
        let whirlpool = orca::Whirlpool {
            discriminator: account_discriminator("Whirlpool"),
            whirlpools_config: legacy_pubkey(Pubkey::default()),
            whirlpool_bump: [255],
            tick_spacing: 64,
            tick_spacing_seed: 64_u16.to_le_bytes(),
            fee_rate: 3_000,
            protocol_fee_rate: 300,
            liquidity: 0,
            sqrt_price: 1_u128 << 64,
            tick_current_index: 0,
            protocol_fee_owed_a: 0,
            protocol_fee_owed_b: 0,
            token_mint_a: legacy_pubkey(Pubkey::default()),
            token_vault_a: legacy_pubkey(Pubkey::default()),
            fee_growth_global_a: 0,
            token_mint_b: legacy_pubkey(Pubkey::default()),
            token_vault_b: legacy_pubkey(Pubkey::default()),
            fee_growth_global_b: 0,
            reward_last_updated_timestamp: 0,
            reward_infos: std::array::from_fn(|_| orca::WhirlpoolRewardInfo {
                mint: legacy_pubkey(Pubkey::default()),
                vault: legacy_pubkey(Pubkey::default()),
                authority: legacy_pubkey(Pubkey::default()),
                emissions_per_second_x64: 0,
                growth_global_x64: 0,
            }),
        };
        let mint = |key: u8| MintInfo {
            key: Pubkey::new_from_array([key; 32]),
            token_program: spl::TOKEN_PROGRAM_ID,
            decimals: 6,
            freeze_authority: None,
            data: vec![],
        };
        let bundle = PoolBundle {
            whirlpool,
            mint_a: mint(1),
            mint_b: mint(2),
            token_vault_a: Pubkey::default(),
            token_vault_b: Pubkey::default(),
            user_token_a: Pubkey::default(),
            user_token_b: Pubkey::default(),
            balance_a: 0,
            balance_b: 0,
            transfer_fee_a: None,
            transfer_fee_b: None,
            key: Pubkey::default(),
        };
        // A liquid pool fills inside the loaded arrays and must agree with
        // core's own quote functions exactly.
        let mut liquid = PoolBundle {
            whirlpool: orca::Whirlpool {
                liquidity: 1_000_000_000_000,
                ..bundle.whirlpool.clone()
            },
            ..bundle.clone()
        };
        liquid.transfer_fee_b = Some(TransferFee::new(25));
        for a_to_b in [true, false] {
            let specified_token_a = a_to_b;
            let exact = bounded_swap_quote(&liquid, arrays, 1_000_000, a_to_b, true, 50)
                .expect("exact-in fills");
            let reference = core::swap_quote_by_input_token(
                1_000_000,
                specified_token_a,
                50,
                liquid.whirlpool.clone().into(),
                arrays.into(),
                liquid.transfer_fee_a,
                liquid.transfer_fee_b,
            )
            .expect("core quotes");
            assert_eq!(exact.expected_output_raw, reference.token_est_out);
            assert_eq!(exact.other_amount_threshold, reference.token_min_out);
            let exact = bounded_swap_quote(&liquid, arrays, 1_000_000, a_to_b, false, 50)
                .expect("exact-out fills");
            let reference = core::swap_quote_by_output_token(
                1_000_000,
                !specified_token_a,
                50,
                liquid.whirlpool.clone().into(),
                arrays.into(),
                liquid.transfer_fee_a,
                liquid.transfer_fee_b,
            )
            .expect("core quotes");
            assert_eq!(exact.expected_input_raw, reference.token_est_in);
            assert_eq!(exact.max_input_raw, reference.token_max_in);
        }

        // An empty pool used to spin forever inside core's compute_swap.
        for (a_to_b, exact_in) in [(true, true), (false, true), (true, false), (false, false)] {
            let quote = bounded_swap_quote(&bundle, arrays, 1_000, a_to_b, exact_in, 50);
            assert!(
                matches!(quote, Err(OrcaWhirlpoolError::Quote(_))),
                "a_to_b={a_to_b} exact_in={exact_in}"
            );
        }
    }
}

#[cfg(test)]
mod liquidity_bound_tests {
    use super::*;

    const Q64: u128 = 1 << 64;
    const RANGE: (i32, i32) = (-1000, 1000);
    const LIQUIDITY: u128 = 1_000_000_000_000;

    fn moved(sqrt_price: u128, price_factor: f64) -> u128 {
        (sqrt_price as f64 * price_factor.sqrt()) as u128
    }

    #[test]
    fn band_brackets_the_price_by_the_slippage() {
        let (low, high) = sqrt_price_band(Q64, 100).expect("band");
        let ratio = |sqrt: u128| (sqrt as f64 / Q64 as f64).powi(2);
        assert!((ratio(low) - 0.99).abs() < 1e-12);
        assert!((ratio(high) - 1.01).abs() < 1e-12);
        let (min, max) = sqrt_price_band(core::MAX_SQRT_PRICE, 10_000).expect("band");
        assert_eq!((min, max), (core::MIN_SQRT_PRICE, core::MAX_SQRT_PRICE));
    }

    #[test]
    fn position_amounts_follow_the_range() {
        let (lower, upper) = RANGE;
        let below = core::tick_index_to_sqrt_price(lower - 10);
        let above = core::tick_index_to_sqrt_price(upper + 10);
        let (a, b) = position_amounts(LIQUIDITY, below, lower, upper, true).expect("below");
        assert!(a > 0 && b == 0);
        let (a, b) = position_amounts(LIQUIDITY, above, lower, upper, true).expect("above");
        assert!(a == 0 && b > 0);
        let (a, b) = position_amounts(LIQUIDITY, Q64, lower, upper, true).expect("inside");
        assert!(a > 0 && b > 0);
    }

    #[test]
    fn deposit_maxima_cover_a_move_within_the_slippage() {
        let (lower, upper) = RANGE;
        let bps = 50;
        let quote = core::increase_liquidity_quote(LIQUIDITY, bps, Q64, lower, upper, None, None)
            .expect("quote");
        // A 0.4% rise (inside 0.5% slippage) needs more B than est × (1 + s).
        let (_, needed_b) =
            position_amounts(LIQUIDITY, moved(Q64, 1.004), lower, upper, true).expect("moved");
        assert!(
            needed_b > quote.token_max_b,
            "core's bound should fail this move"
        );
        let widened =
            widen_increase_bounds(quote, (Q64, None, None), lower, upper, bps).expect("widen");
        assert!(needed_b <= widened.token_max_b);
        let (needed_a, _) =
            position_amounts(LIQUIDITY, moved(Q64, 0.996), lower, upper, true).expect("moved");
        assert!(needed_a <= widened.token_max_a);
        assert!(widened.token_max_a >= quote.token_max_a);
    }

    #[test]
    fn deposit_maxima_allow_the_missing_side_when_quoted_out_of_range() {
        let (lower, upper) = RANGE;
        let just_below: u128 = core::tick_index_to_sqrt_price(lower - 1);
        let quote =
            core::increase_liquidity_quote(LIQUIDITY, 50, just_below, lower, upper, None, None)
                .expect("quote");
        assert_eq!(quote.token_max_b, 0);
        let widened = widen_increase_bounds(quote, (just_below, None, None), lower, upper, 50)
            .expect("widen");
        assert!(widened.token_max_b > 0);
    }

    #[test]
    fn withdrawal_minima_hold_over_the_band() {
        let (lower, upper) = RANGE;
        let bps = 50;
        let quote = core::decrease_liquidity_quote(LIQUIDITY, bps, Q64, lower, upper, None, None)
            .expect("quote");
        let widened =
            widen_decrease_bounds(quote, (Q64, None, None), lower, upper, bps).expect("widen");
        let (got_a, _) =
            position_amounts(LIQUIDITY, moved(Q64, 1.004), lower, upper, false).expect("up");
        let (_, got_b) =
            position_amounts(LIQUIDITY, moved(Q64, 0.996), lower, upper, false).expect("down");
        assert!(got_a >= widened.token_min_a && got_b >= widened.token_min_b);
        assert!(got_a < quote.token_min_a || got_b < quote.token_min_b);
    }

    #[test]
    fn cached_discriminators_match_their_hash() {
        for name in [
            "Whirlpool",
            "TickArray",
            "Position",
            "FeeTier",
            "WhirlpoolsConfig",
            "Other",
        ] {
            assert_eq!(account_discriminator(name), compute_discriminator(name));
        }
    }

    #[test]
    fn clock_epoch_reads_the_epoch_field() {
        let mut data = vec![0_u8; 40];
        data[16..24].copy_from_slice(&9_u64.to_le_bytes());
        let account = Account {
            data,
            ..Account::default()
        };
        assert_eq!(clock_epoch(&account).expect("epoch"), 9);
        let short = Account {
            data: vec![0; 10],
            ..Account::default()
        };
        assert!(clock_epoch(&short).is_err());
    }
}
