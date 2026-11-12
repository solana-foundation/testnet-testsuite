//! High-level async actions for Manifest markets on a self-deployed Manifest
//! program.
//!
//! Instructions are built with the codama-generated `manifest-sdk` and market
//! accounts are read by [`book`]; no GPL Manifest code is linked. Every
//! transaction is signed once, simulated, submitted, and confirmed through
//! `mtm-chain`.

pub mod book;

use std::sync::Arc;
use std::time::Instant;

pub use book::{Book, BookError, BookOrder, Seat};
use manifest_sdk::instructions::{
    BatchUpdate, BatchUpdateInstructionArgs, ClaimSeat, CreateMarket, Deposit,
    DepositInstructionArgs, SwapV2, SwapV2InstructionArgs, Withdraw, WithdrawInstructionArgs,
};
use manifest_sdk::types::{CancelOrderParams, OrderType, PlaceOrderParams, SwapParams};
use metrics::{counter, histogram};
use mtm_chain::{ChainClient, SubmissionConfig, SubmissionError};
use mtm_dex_harness::spl;
use rust_decimal::Decimal;
use solana_account::Account;
use solana_account_decoder_client_types::UiAccountEncoding;
use solana_commitment_config::CommitmentConfig;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_rpc_client_api::config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
use solana_rpc_client_api::filter::{Memcmp, RpcFilterType};
use solana_signature::Signature;
use solana_signer::Signer;
use thiserror::Error;
use tracing::{Instrument, error, info, info_span, warn};

/// Canonical mainnet Manifest program, for reference only; testnet uses a fork.
pub const CANONICAL_PROGRAM_ID: Pubkey = manifest_sdk::programs::MANIFEST_ID;
const BASE_MINT_OFFSET: usize = 16;
const QUOTE_MINT_OFFSET: usize = 48;

pub type Result<T> = std::result::Result<T, ManifestError>;

/// Vault PDA for a market and one of its mints.
pub fn vault_address(program_id: Pubkey, market: Pubkey, mint: Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"vault", market.as_ref(), mint.as_ref()], &program_id).0
}

/// Every market of `program_id` trading `base_mint` against `quote_mint`,
/// ordered by address.
pub async fn find_markets(
    chain: &ChainClient,
    program_id: Pubkey,
    base_mint: Pubkey,
    quote_mint: Pubkey,
) -> Result<Vec<Pubkey>> {
    let config = RpcProgramAccountsConfig {
        filters: Some(vec![
            RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
                0,
                book::MARKET_FIXED_DISCRIMINANT.to_le_bytes().to_vec(),
            )),
            RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
                BASE_MINT_OFFSET,
                base_mint.to_bytes().to_vec(),
            )),
            RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
                QUOTE_MINT_OFFSET,
                quote_mint.to_bytes().to_vec(),
            )),
        ]),
        account_config: RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::Base64),
            data_slice: Some(solana_account_decoder_client_types::UiDataSliceConfig {
                offset: 0,
                length: 0,
            }),
            ..RpcAccountInfoConfig::default()
        },
        ..RpcProgramAccountsConfig::default()
    };
    let mut markets: Vec<Pubkey> = chain
        .rpc()
        .get_program_ui_accounts_with_config(&program_id, config)
        .await
        .map_err(|error| ManifestError::Rpc(error.to_string()))?
        .into_iter()
        .map(|(address, _)| address)
        .collect();
    markets.sort();
    Ok(markets)
}

/// A market and the token programs of its mints, fetched at one slot. All
/// quoting on a snapshot is pure.
#[derive(Clone, Debug)]
pub struct MarketSnapshot {
    pub program_id: Pubkey,
    pub market: Pubkey,
    pub book: Book,
    pub base_token_program: Pubkey,
    pub quote_token_program: Pubkey,
    pub slot: u64,
}

impl MarketSnapshot {
    pub async fn load(chain: &ChainClient, program_id: Pubkey, market: Pubkey) -> Result<Self> {
        Self::load_at(chain, program_id, market, None).await
    }

    /// Load the market as of a context slot of at least `min_context_slot`
    /// (reads after a confirmed transaction must not come from a node that
    /// has not seen it). `slot` is the slot the market account was read at.
    pub async fn load_at(
        chain: &ChainClient,
        program_id: Pubkey,
        market: Pubkey,
        min_context_slot: Option<u64>,
    ) -> Result<Self> {
        let (slot, accounts) = fetch_accounts(chain, &[market], min_context_slot).await?;
        let account = require_account(&[market], &accounts, 0, "market")?;
        if account.owner != program_id {
            return Err(ManifestError::MalformedAccount {
                address: market,
                reason: format!("expected owner {program_id}, got {}", account.owner),
            });
        }
        let book = Book::parse(&account.data).map_err(|error| ManifestError::MalformedAccount {
            address: market,
            reason: error.to_string(),
        })?;
        if vault_address(program_id, market, book.base_mint) != book.base_vault
            || vault_address(program_id, market, book.quote_mint) != book.quote_vault
        {
            return Err(ManifestError::MalformedAccount {
                address: market,
                reason: "vaults are not the canonical PDAs".to_owned(),
            });
        }
        let mint_keys = [book.base_mint, book.quote_mint];
        let (_, mints) = fetch_accounts(chain, &mint_keys, Some(slot)).await?;
        let base = require_account(&mint_keys, &mints, 0, "base mint")?;
        let quote = require_account(&mint_keys, &mints, 1, "quote mint")?;
        for (key, account) in mint_keys.iter().zip([base, quote]) {
            if spl::decode_mint(account.owner, &account.data).is_none() {
                return Err(ManifestError::UnsupportedToken {
                    mint: *key,
                    reason: format!("unsupported token program {}", account.owner),
                });
            }
        }
        Ok(Self {
            program_id,
            market,
            base_token_program: base.owner,
            quote_token_program: quote.owner,
            book,
            slot,
        })
    }

    /// Whether `input_mint` is the base mint; errors if the pair does not match.
    pub fn base_in(&self, input_mint: Pubkey, output_mint: Pubkey) -> Result<bool> {
        let book = &self.book;
        if input_mint == book.base_mint && output_mint == book.quote_mint {
            Ok(true)
        } else if input_mint == book.quote_mint && output_mint == book.base_mint {
            Ok(false)
        } else {
            Err(ManifestError::InvalidInput(
                "swap mints do not match the market".to_owned(),
            ))
        }
    }

    /// Quote a swap in raw atoms against this snapshot.
    pub fn quote(
        &self,
        input_mint: Pubkey,
        output_mint: Pubkey,
        amount_raw: u64,
        exact_in: bool,
        slippage_bps: u16,
    ) -> Result<SwapQuote> {
        if amount_raw == 0 {
            return Err(ManifestError::InvalidInput(
                "swap amount must be greater than zero".to_owned(),
            ));
        }
        if slippage_bps > 10_000 {
            return Err(ManifestError::InvalidInput(
                "slippage_bps cannot exceed 10,000".to_owned(),
            ));
        }
        let base_in = self.base_in(input_mint, output_mint)?;
        let now_slot = u32::try_from(self.slot).unwrap_or(u32::MAX);
        let (input, output) = if exact_in {
            self.book.quote_exact_in(base_in, amount_raw, now_slot)
        } else {
            self.book.quote_exact_out(base_in, amount_raw, now_slot)
        }
        .map_err(|error| ManifestError::Quote(error.to_string()))?;
        // Buying base with quote fills whole base atoms only, so the program
        // leaves up to one atom's cost with the trader. That shortfall is
        // rounding, not missing liquidity, when the book holds at least the
        // requested quote amount.
        let rounding_only = !base_in
            && input < amount_raw
            && self
                .book
                .quote_depth(true, now_slot)
                .map_err(|error| ManifestError::Quote(error.to_string()))?
                >= amount_raw;
        if exact_in && input != amount_raw && !rounding_only {
            return Err(ManifestError::Quote(format!(
                "book absorbs only {input} of the requested {amount_raw} input atoms"
            )));
        }
        if !exact_in && output < amount_raw {
            return Err(ManifestError::Quote(format!(
                "book provides only {output} of the requested {amount_raw} output atoms"
            )));
        }
        if output == 0 {
            return Err(ManifestError::Quote(
                "swap amount quotes to zero output".to_owned(),
            ));
        }
        let (other_amount_threshold, max_input_raw) = if exact_in {
            (apply_slippage(output, slippage_bps, false)?, amount_raw)
        } else {
            let maximum = apply_slippage(input, slippage_bps, true)?;
            (maximum, maximum)
        };
        Ok(SwapQuote {
            exact_in,
            input_mint,
            output_mint,
            expected_input_raw: input,
            expected_output_raw: output,
            other_amount_threshold,
            max_input_raw,
        })
    }

    /// Token accounts a swap moves funds between: `(base, quote)` ATAs of `owner`.
    pub fn trader_accounts(&self, owner: Pubkey) -> (Pubkey, Pubkey) {
        (
            spl::associated_token_address(&owner, &self.book.base_mint, &self.base_token_program),
            spl::associated_token_address(&owner, &self.book.quote_mint, &self.quote_token_program),
        )
    }

    /// Accounts of a seatless `SwapV2` for `owner` (who also pays any market
    /// expansion), without global accounts. Shared with route builders that
    /// pass the same list to an aggregator.
    pub fn swap_account_metas(&self, owner: Pubkey) -> Vec<AccountMeta> {
        let (trader_base, trader_quote) = self.trader_accounts(owner);
        let ix = SwapV2 {
            payer: address(owner),
            owner: address(owner),
            market: address(self.market),
            system_program: address(spl::SYSTEM_PROGRAM_ID),
            trader_base: address(trader_base),
            trader_quote: address(trader_quote),
            base_vault: address(self.book.base_vault),
            quote_vault: address(self.book.quote_vault),
            token_program_base: address(self.base_token_program),
            base_mint: Some(address(self.book.base_mint)),
            token_program_quote: Some(address(self.quote_token_program)),
            quote_mint: Some(address(self.book.quote_mint)),
            global: None,
            global_vault: None,
        }
        .instruction(SwapV2InstructionArgs {
            params: SwapParams {
                in_atoms: 0,
                out_atoms: 0,
                is_base_in: true,
                is_exact_in: true,
            },
        });
        without_placeholders(ix, self.program_id).accounts
    }

    /// ATA creation plus one `SwapV2` executing `quote` for `owner`.
    pub fn swap_instructions(&self, owner: Pubkey, quote: &SwapQuote) -> Result<Vec<Instruction>> {
        let base_in = self.base_in(quote.input_mint, quote.output_mint)?;
        let (in_atoms, out_atoms) = if quote.exact_in {
            (quote.expected_input_raw, quote.other_amount_threshold)
        } else {
            (quote.max_input_raw, quote.expected_output_raw)
        };
        let accounts = self.swap_account_metas(owner);
        let mut data = vec![manifest_sdk::instructions::SWAP_V2_DISCRIMINATOR];
        data.extend_from_slice(
            &borsh::to_vec(&SwapParams {
                in_atoms,
                out_atoms,
                is_base_in: base_in,
                is_exact_in: quote.exact_in,
            })
            .map_err(|error| ManifestError::InvalidInput(error.to_string()))?,
        );
        Ok(vec![
            spl::create_associated_token_account_idempotent(
                &owner,
                &owner,
                &self.book.base_mint,
                &self.base_token_program,
            ),
            spl::create_associated_token_account_idempotent(
                &owner,
                &owner,
                &self.book.quote_mint,
                &self.quote_token_program,
            ),
            Instruction {
                program_id: self.program_id,
                accounts,
                data,
            },
        ])
    }
}

#[derive(Clone)]
pub struct ManifestClient {
    chain: ChainClient,
    program_id: Pubkey,
    payer: Arc<dyn Signer + Send + Sync>,
    slippage_bps: u16,
    submission: SubmissionConfig,
}

impl ManifestClient {
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
            return Err(ManifestError::InvalidInput(
                "slippage_bps cannot exceed 10,000".to_owned(),
            ));
        }
        if compute_unit_limit == 0 {
            return Err(ManifestError::InvalidInput(
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

    pub async fn find_markets(&self, base_mint: Pubkey, quote_mint: Pubkey) -> Result<Vec<Pubkey>> {
        find_markets(&self.chain, self.program_id, base_mint, quote_mint).await
    }

    pub async fn load_market(&self, market: Pubkey) -> Result<MarketSnapshot> {
        MarketSnapshot::load(&self.chain, self.program_id, market).await
    }

    /// The market as of (at least) the slot `transaction` confirmed in.
    async fn load_market_after(
        &self,
        market: Pubkey,
        transaction: &TransactionOutcome,
    ) -> Result<MarketSnapshot> {
        MarketSnapshot::load_at(
            &self.chain,
            self.program_id,
            market,
            Some(transaction.confirmation_slot),
        )
        .await
    }

    /// Create a market in a fresh keypair account. Manifest markets are not
    /// PDAs, so this is intentionally non-idempotent; use [`find_markets`] to
    /// locate existing ones.
    pub async fn create_market(&self, params: CreateMarketParams) -> Result<CreateMarketOutcome> {
        self.run("create_market", params.base_mint, async move {
            if params.base_mint == params.quote_mint {
                return Err(ManifestError::InvalidInput(
                    "base and quote mints must differ".to_owned(),
                ));
            }
            let keys = [self.program_id, params.base_mint, params.quote_mint];
            let accounts = self.fetch_optional(&keys).await?;
            let program = require_account(&keys, &accounts, 0, "Manifest program")?;
            if !program.executable {
                return Err(ManifestError::MalformedAccount {
                    address: self.program_id,
                    reason: "Manifest program account is not executable".to_owned(),
                });
            }
            for (index, mint) in [(1, params.base_mint), (2, params.quote_mint)] {
                let account = require_account(&keys, &accounts, index, "mint")?;
                if spl::decode_mint(account.owner, &account.data).is_none() {
                    return Err(ManifestError::UnsupportedToken {
                        mint,
                        reason: format!("unsupported token program {}", account.owner),
                    });
                }
            }
            let market = Keypair::new();
            let base_vault = vault_address(self.program_id, market.pubkey(), params.base_mint);
            let quote_vault = vault_address(self.program_id, market.pubkey(), params.quote_mint);
            let rent = self
                .chain
                .rpc()
                .get_minimum_balance_for_rent_exemption(book::MARKET_FIXED_SIZE)
                .await
                .map_err(|error| ManifestError::Rpc(error.to_string()))?;
            let create_account = create_account_instruction(
                self.payer(),
                market.pubkey(),
                rent,
                book::MARKET_FIXED_SIZE as u64,
                self.program_id,
            );
            let create_market = CreateMarket {
                payer: address(self.payer()),
                market: address(market.pubkey()),
                system_program: address(spl::SYSTEM_PROGRAM_ID),
                base_mint: address(params.base_mint),
                quote_mint: address(params.quote_mint),
                base_vault: address(base_vault),
                quote_vault: address(quote_vault),
                token_program: address(spl::TOKEN_PROGRAM_ID),
                token_program22: address(spl::TOKEN_2022_PROGRAM_ID),
            }
            .instruction();
            let transaction = self
                .submit(
                    vec![create_account, retarget(create_market, self.program_id)],
                    &[&market],
                )
                .await?;
            Ok(CreateMarketOutcome {
                transaction,
                market: market.pubkey(),
                base_mint: params.base_mint,
                quote_mint: params.quote_mint,
                base_vault,
                quote_vault,
            })
        })
        .await
    }

    /// Claim the payer's seat, or return the existing one.
    pub async fn claim_seat(&self, market: Pubkey) -> Result<SeatOutcome> {
        self.run("claim_seat", market, async move {
            if let Some(seat) = self.load_market(market).await?.book.seat(&self.payer()) {
                return Ok(SeatOutcome {
                    transaction: None,
                    seat,
                });
            }
            let ix = ClaimSeat {
                payer: address(self.payer()),
                market: address(market),
                system_program: address(spl::SYSTEM_PROGRAM_ID),
            }
            .instruction();
            let transaction = self
                .submit(vec![retarget(ix, self.program_id)], &[])
                .await?;
            let seat = self.require_seat(market, &transaction).await?;
            Ok(SeatOutcome {
                transaction: Some(transaction),
                seat,
            })
        })
        .await
    }

    /// Move tokens from the payer's ATA into its seat balance.
    pub async fn deposit(&self, market: Pubkey, input: TokenAmount) -> Result<BalanceOutcome> {
        self.run("deposit", market, async move {
            let snapshot = self.load_market(market).await?;
            let seat = snapshot
                .book
                .seat(&self.payer())
                .ok_or(ManifestError::MissingSeat {
                    market,
                    trader: self.payer(),
                })?;
            let leg = self.seat_leg(&snapshot, input.mint)?;
            let amount_raw = ui_to_raw(input.amount, leg.decimals)?;
            let accounts = self.fetch_optional(&[leg.trader_token]).await?;
            let balance = self.token_balance(leg.trader_token, accounts[0].as_ref(), &leg)?;
            if balance < amount_raw {
                return Err(ManifestError::InsufficientBalance {
                    mint: input.mint,
                    required: amount_raw,
                    available: balance,
                });
            }
            let ix = Deposit {
                payer: address(self.payer()),
                market: address(market),
                trader_token: address(leg.trader_token),
                vault: address(leg.vault),
                token_program: address(leg.token_program),
                mint: address(input.mint),
            }
            .instruction(DepositInstructionArgs {
                amount_atoms: amount_raw,
                trader_index_hint: Some(seat.index),
            });
            let transaction = self
                .submit(vec![retarget(ix, self.program_id)], &[])
                .await?;
            let seat = self.require_seat(market, &transaction).await?;
            Ok(BalanceOutcome {
                transaction: Some(transaction),
                seat,
            })
        })
        .await
    }

    /// Withdraw the payer's entire withdrawable seat balance of both mints.
    pub async fn withdraw_all(&self, market: Pubkey) -> Result<BalanceOutcome> {
        self.run("withdraw_all", market, async move {
            let snapshot = self.load_market(market).await?;
            let seat = snapshot
                .book
                .seat(&self.payer())
                .ok_or(ManifestError::MissingSeat {
                    market,
                    trader: self.payer(),
                })?;
            let mut instructions = Vec::new();
            for (mint, amount) in [
                (snapshot.book.base_mint, seat.base_withdrawable),
                (snapshot.book.quote_mint, seat.quote_withdrawable),
            ] {
                if amount == 0 {
                    continue;
                }
                let leg = self.seat_leg(&snapshot, mint)?;
                instructions.push(spl::create_associated_token_account_idempotent(
                    &self.payer(),
                    &self.payer(),
                    &mint,
                    &leg.token_program,
                ));
                let ix = Withdraw {
                    payer: address(self.payer()),
                    market: address(market),
                    trader_token: address(leg.trader_token),
                    vault: address(leg.vault),
                    token_program: address(leg.token_program),
                    mint: address(mint),
                }
                .instruction(WithdrawInstructionArgs {
                    amount_atoms: amount,
                    trader_index_hint: Some(seat.index),
                });
                instructions.push(retarget(ix, self.program_id));
            }
            if instructions.is_empty() {
                return Ok(BalanceOutcome {
                    transaction: None,
                    seat,
                });
            }
            let transaction = self.submit(instructions, &[]).await?;
            let seat = self.require_seat(market, &transaction).await?;
            Ok(BalanceOutcome {
                transaction: Some(transaction),
                seat,
            })
        })
        .await
    }

    /// Place resting limit orders funded from the payer's seat balance.
    /// Orders the seat cannot fund are trimmed (see [`replace_orders_with`]).
    ///
    /// [`replace_orders_with`]: Self::replace_orders_with
    pub async fn place_orders(
        &self,
        market: Pubkey,
        orders: &[OrderSpec],
    ) -> Result<OrdersOutcome> {
        let build = |_: &MarketSnapshot, _: &Seat| orders.to_vec();
        self.run("place_orders", market, self.batch(market, &build, false))
            .await
    }

    /// Cancel every resting order of the payer's seat and place `orders`, in
    /// one atomic `BatchUpdate` so the book never shows a gap. Orders the
    /// seat cannot fund are trimmed (see [`replace_orders_with`]).
    ///
    /// [`replace_orders_with`]: Self::replace_orders_with
    pub async fn replace_orders(
        &self,
        market: Pubkey,
        orders: &[OrderSpec],
    ) -> Result<OrdersOutcome> {
        let build = |_: &MarketSnapshot, _: &Seat| orders.to_vec();
        self.run("replace_orders", market, self.batch(market, &build, true))
            .await
    }

    /// [`replace_orders`](Self::replace_orders) with orders sized by `build`
    /// from the market snapshot and seat the batch is built on. `build` runs
    /// once per attempt, so the stale-cancel retry (a fill moved the seat's
    /// inventory) re-sizes from the post-fill seat.
    ///
    /// The client never sends a batch the seat cannot fund: each side's
    /// budget is the seat's withdrawable balance plus what the batch's
    /// cancels refund; bids lock [`book::locked_quote_atoms`] plus one atom
    /// of rounding margin. Each side is funded best price first; the first
    /// order that does not fit is shrunk to what is left and worse ones are
    /// dropped. [`OrdersOutcome::placed`] holds what was actually sent. When
    /// nothing fits the batch only cancels; with nothing to cancel either it
    /// fails with [`ManifestError::InsufficientBalance`] before sending.
    pub async fn replace_orders_with<F>(&self, market: Pubkey, build: F) -> Result<OrdersOutcome>
    where
        F: Fn(&MarketSnapshot, &Seat) -> Vec<OrderSpec>,
    {
        self.run("replace_orders", market, self.batch(market, &build, true))
            .await
    }

    /// A batch that cancels is rebuilt from a fresh book and retried once
    /// when one of its cancels went stale (see [`ManifestError::is_stale_cancel`]).
    async fn batch<F>(
        &self,
        market: Pubkey,
        build: &F,
        cancel_existing: bool,
    ) -> Result<OrdersOutcome>
    where
        F: Fn(&MarketSnapshot, &Seat) -> Vec<OrderSpec>,
    {
        let attempts = if cancel_existing {
            STALE_CANCEL_ATTEMPTS
        } else {
            1
        };
        retry_stale("replace_orders", market, attempts, |min_context_slot| {
            self.batch_once(market, build, cancel_existing, min_context_slot)
        })
        .await
    }

    /// One batch attempt; also returns the slot the market was read at.
    async fn batch_once<F>(
        &self,
        market: Pubkey,
        build: &F,
        cancel_existing: bool,
        min_context_slot: Option<u64>,
    ) -> (Result<OrdersOutcome>, Option<u64>)
    where
        F: Fn(&MarketSnapshot, &Seat) -> Vec<OrderSpec>,
    {
        let snapshot =
            match MarketSnapshot::load_at(&self.chain, self.program_id, market, min_context_slot)
                .await
            {
                Ok(snapshot) => snapshot,
                Err(error) => return (Err(error), None),
            };
        let loaded = Some(snapshot.slot);
        let result = async {
            let plan = plan_batch(&snapshot, self.payer(), build, cancel_existing)?;
            let ix = self.batch_update(market, plan.seat.index, plan.cancels, plan.orders);
            let transaction = self.submit(vec![ix], &[]).await?;
            let after = self.load_market_after(market, &transaction).await?;
            let resting = after.book.orders_of(plan.seat.index).copied().collect();
            Ok(OrdersOutcome {
                transaction: Some(transaction),
                resting,
                placed: plan.placed,
            })
        }
        .await;
        (result, loaded)
    }

    /// Cancel every resting order owned by the payer's seat.
    pub async fn cancel_all_orders(&self, market: Pubkey) -> Result<OrdersOutcome> {
        self.run(
            "cancel_all_orders",
            market,
            retry_stale(
                "cancel_all_orders",
                market,
                STALE_CANCEL_ATTEMPTS,
                |min_context_slot| self.cancel_all_once(market, min_context_slot),
            ),
        )
        .await
    }

    async fn cancel_all_once(
        &self,
        market: Pubkey,
        min_context_slot: Option<u64>,
    ) -> (Result<OrdersOutcome>, Option<u64>) {
        let snapshot =
            match MarketSnapshot::load_at(&self.chain, self.program_id, market, min_context_slot)
                .await
            {
                Ok(snapshot) => snapshot,
                Err(error) => return (Err(error), None),
            };
        let loaded = Some(snapshot.slot);
        let result = async {
            let seat = snapshot
                .book
                .seat(&self.payer())
                .ok_or(ManifestError::MissingSeat {
                    market,
                    trader: self.payer(),
                })?;
            let cancels = cancels_of(&snapshot.book, seat.index);
            if cancels.is_empty() {
                return Ok(OrdersOutcome {
                    transaction: None,
                    resting: vec![],
                    placed: vec![],
                });
            }
            let ix = self.batch_update(market, seat.index, cancels, vec![]);
            let transaction = self.submit(vec![ix], &[]).await?;
            let after = self.load_market_after(market, &transaction).await?;
            Ok(OrdersOutcome {
                transaction: Some(transaction),
                resting: after.book.orders_of(seat.index).copied().collect(),
                placed: vec![],
            })
        }
        .await;
        (result, loaded)
    }

    pub async fn swap_exact_in(&self, params: SwapExactInParams) -> Result<SwapOutcome> {
        self.swap(
            params.market,
            params.input_mint,
            params.output_mint,
            params.amount_in,
            true,
        )
        .await
    }

    /// Manifest documents direct exact-out swaps as unsupported unless the
    /// caller validates the returned amounts: when the book is exhausted the
    /// program fills what it can. The quote here refuses partial fills, and
    /// the post-trade balances in the outcome let callers verify.
    pub async fn swap_exact_out(&self, params: SwapExactOutParams) -> Result<SwapOutcome> {
        self.swap(
            params.market,
            params.input_mint,
            params.output_mint,
            params.amount_out,
            false,
        )
        .await
    }

    async fn swap(
        &self,
        market: Pubkey,
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
        self.run(action, market, async move {
            ensure_positive(amount, "swap amount")?;
            let snapshot = self.load_market(market).await?;
            let base_in = snapshot.base_in(input_mint, output_mint)?;
            let specified_decimals = match (exact_in, base_in) {
                (true, true) | (false, false) => snapshot.book.base_decimals,
                _ => snapshot.book.quote_decimals,
            };
            let amount_raw = ui_to_raw(amount, specified_decimals)?;
            let quote = snapshot.quote(
                input_mint,
                output_mint,
                amount_raw,
                exact_in,
                self.slippage_bps,
            )?;
            let (trader_base, trader_quote) = snapshot.trader_accounts(self.payer());
            let (input_account, output_account) = if base_in {
                (trader_base, trader_quote)
            } else {
                (trader_quote, trader_base)
            };
            let input_leg = self.seat_leg(&snapshot, input_mint)?;
            let output_leg = self.seat_leg(&snapshot, output_mint)?;
            let legs = [(input_account, &input_leg), (output_account, &output_leg)];
            let [input_before, output_before] = self.token_balances(legs, None).await?;
            if input_before < quote.max_input_raw {
                return Err(ManifestError::InsufficientBalance {
                    mint: input_mint,
                    required: quote.max_input_raw,
                    available: input_before,
                });
            }
            let instructions = snapshot.swap_instructions(self.payer(), &quote)?;
            let transaction = self.submit(instructions, &[]).await?;
            let [input_after, output_after] = self
                .token_balances(legs, Some(transaction.confirmation_slot))
                .await?;
            let executed = ExecutedSwap {
                input_raw: input_before.saturating_sub(input_after),
                output_raw: output_after.saturating_sub(output_before),
            };
            if !exact_in && executed.output_raw < quote.expected_output_raw {
                return Err(ManifestError::Quote(format!(
                    "exact-out swap {} delivered {} of {} requested atoms",
                    transaction.signature, executed.output_raw, quote.expected_output_raw
                )));
            }
            Ok(SwapOutcome {
                transaction,
                quote,
                executed,
                accounts: SwapAccounts {
                    market,
                    input_token_account: input_account,
                    output_token_account: output_account,
                    base_vault: snapshot.book.base_vault,
                    quote_vault: snapshot.book.quote_vault,
                },
            })
        })
        .await
    }

    fn batch_update(
        &self,
        market: Pubkey,
        seat_index: u32,
        cancels: Vec<CancelOrderParams>,
        orders: Vec<PlaceOrderParams>,
    ) -> Instruction {
        let ix = BatchUpdate {
            payer: address(self.payer()),
            market: address(market),
            system_program: address(spl::SYSTEM_PROGRAM_ID),
            base_mint: None,
            base_global: None,
            base_global_vault: None,
            base_market_vault: None,
            base_token_program: None,
            quote_mint: None,
            quote_global: None,
            quote_global_vault: None,
            quote_market_vault: None,
            quote_token_program: None,
        }
        .instruction(BatchUpdateInstructionArgs {
            trader_index_hint: Some(seat_index),
            cancels,
            orders,
        });
        without_placeholders(ix, self.program_id)
    }

    async fn require_seat(&self, market: Pubkey, after: &TransactionOutcome) -> Result<Seat> {
        self.load_market_after(market, after)
            .await?
            .book
            .seat(&self.payer())
            .ok_or(ManifestError::MissingSeat {
                market,
                trader: self.payer(),
            })
    }

    fn seat_leg(&self, snapshot: &MarketSnapshot, mint: Pubkey) -> Result<SeatLeg> {
        let book = &snapshot.book;
        let (vault, token_program, decimals) = if mint == book.base_mint {
            (
                book.base_vault,
                snapshot.base_token_program,
                book.base_decimals,
            )
        } else if mint == book.quote_mint {
            (
                book.quote_vault,
                snapshot.quote_token_program,
                book.quote_decimals,
            )
        } else {
            return Err(ManifestError::InvalidInput(
                "mint does not belong to the market".to_owned(),
            ));
        };
        Ok(SeatLeg {
            mint,
            vault,
            token_program,
            decimals,
            trader_token: spl::associated_token_address(&self.payer(), &mint, &token_program),
        })
    }

    /// Balances of the payer's two swap token accounts in one read, at a
    /// context slot of at least `min_context_slot`.
    async fn token_balances(
        &self,
        legs: [(Pubkey, &SeatLeg); 2],
        min_context_slot: Option<u64>,
    ) -> Result<[u64; 2]> {
        let keys = legs.map(|(key, _)| key);
        let (_, accounts) = fetch_accounts(&self.chain, &keys, min_context_slot).await?;
        Ok([
            self.token_balance(keys[0], accounts[0].as_ref(), legs[0].1)?,
            self.token_balance(keys[1], accounts[1].as_ref(), legs[1].1)?,
        ])
    }

    fn token_balance(&self, key: Pubkey, account: Option<&Account>, leg: &SeatLeg) -> Result<u64> {
        let Some(account) = account else {
            return Ok(0);
        };
        if account.owner != leg.token_program {
            return Err(ManifestError::MalformedAccount {
                address: key,
                reason: "token account is owned by the wrong token program".to_owned(),
            });
        }
        let state = spl::decode_token_account(&account.data).ok_or_else(|| {
            ManifestError::MalformedAccount {
                address: key,
                reason: "token account data is too short".to_owned(),
            }
        })?;
        if state.mint != leg.mint || state.owner != self.payer() {
            return Err(ManifestError::MalformedAccount {
                address: key,
                reason: "token account mint or authority is invalid".to_owned(),
            });
        }
        Ok(state.amount)
    }

    async fn run<T, F>(&self, action: &'static str, target: Pubkey, future: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        let started = Instant::now();
        let span = info_span!("manifest_action", action, %target);
        let result = future.instrument(span).await;
        let status = if result.is_ok() { "confirmed" } else { "error" };
        counter!("manifest_actions_total", "action" => action, "status" => status).increment(1);
        histogram!("manifest_action_latency_seconds", "action" => action)
            .record(started.elapsed().as_secs_f64());
        match &result {
            Ok(_) => {
                info!(action, %target, latency_ms = started.elapsed().as_millis(), "Manifest action complete")
            }
            Err(problem) => {
                if let ManifestError::SimulationFailure { logs, .. } = problem {
                    error!(action, %target, ?logs, "Manifest simulation logs");
                }
                error!(action, %target, error = %problem, latency_ms = started.elapsed().as_millis(), "Manifest action failed")
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
                histogram!("manifest_simulated_compute_units")
                    .record(outcome.simulated_compute_units as f64);
                Ok(TransactionOutcome {
                    signature: outcome.signature,
                    confirmation_slot: outcome.confirmation_slot,
                    simulated_compute_units: outcome.simulated_compute_units,
                })
            }
            Err(SubmissionError::Simulation { error, logs }) => {
                Err(ManifestError::SimulationFailure { error, logs })
            }
            // A failed simulation RPC says nothing about the transaction.
            Err(SubmissionError::SimulationRpc(error)) => Err(ManifestError::Rpc(error)),
            Err(SubmissionError::Execution {
                signature,
                slot,
                error,
            }) => Err(ManifestError::ExecutionFailure {
                signature,
                slot,
                error,
            }),
            Err(SubmissionError::AmbiguousConfirmation { signature, reason }) => {
                Err(ManifestError::AmbiguousConfirmation { signature, reason })
            }
            Err(error) => Err(ManifestError::SubmissionFailure(error.to_string())),
        }
    }

    async fn fetch_optional(&self, keys: &[Pubkey]) -> Result<Vec<Option<Account>>> {
        self.chain
            .rpc()
            .get_multiple_accounts(keys)
            .await
            .map_err(|error| ManifestError::Rpc(error.to_string()))
    }
}

/// UI amount of one market mint.
#[derive(Clone, Copy, Debug)]
pub struct TokenAmount {
    pub mint: Pubkey,
    pub amount: Decimal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Bid,
    Ask,
}

/// One resting limit order in UI units.
#[derive(Clone, Copy, Debug)]
pub struct OrderSpec {
    pub side: Side,
    /// Quote per base, in UI units.
    pub price: Decimal,
    /// Base UI units.
    pub base_amount: Decimal,
}

#[derive(Clone, Copy, Debug)]
pub struct CreateMarketParams {
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
}

#[derive(Clone, Copy, Debug)]
pub struct SwapExactInParams {
    pub market: Pubkey,
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    pub amount_in: Decimal,
}

#[derive(Clone, Copy, Debug)]
pub struct SwapExactOutParams {
    pub market: Pubkey,
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    pub amount_out: Decimal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransactionOutcome {
    pub signature: Signature,
    pub confirmation_slot: u64,
    pub simulated_compute_units: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct CreateMarketOutcome {
    pub transaction: TransactionOutcome,
    pub market: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
}

#[derive(Clone, Copy, Debug)]
pub struct SeatOutcome {
    pub transaction: Option<TransactionOutcome>,
    pub seat: Seat,
}

#[derive(Clone, Copy, Debug)]
pub struct BalanceOutcome {
    pub transaction: Option<TransactionOutcome>,
    /// Seat balances after the transaction.
    pub seat: Seat,
}

#[derive(Clone, Debug)]
pub struct OrdersOutcome {
    pub transaction: Option<TransactionOutcome>,
    /// The seat's resting orders after the transaction.
    pub resting: Vec<BookOrder>,
    /// The orders the batch placed, after fitting them to the seat's
    /// balance: sizes can be smaller than requested and orders dropped.
    pub placed: Vec<OrderSpec>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwapQuote {
    pub exact_in: bool,
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    pub expected_input_raw: u64,
    pub expected_output_raw: u64,
    /// Minimum output (exact-in) or maximum input (exact-out) sent on-chain.
    pub other_amount_threshold: u64,
    pub max_input_raw: u64,
}

/// Token-account deltas observed around a confirmed swap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecutedSwap {
    pub input_raw: u64,
    pub output_raw: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct SwapAccounts {
    pub market: Pubkey,
    pub input_token_account: Pubkey,
    pub output_token_account: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
}

#[derive(Clone, Copy, Debug)]
pub struct SwapOutcome {
    pub transaction: TransactionOutcome,
    pub quote: SwapQuote,
    pub executed: ExecutedSwap,
    pub accounts: SwapAccounts,
}

/// Quote atoms each placed bid keeps in reserve beyond its lock: a bid that
/// matches resting asks before it rests pays per-fill rounding.
pub const BID_MARGIN_ATOMS: u64 = 1;

/// Run `attempt` up to `attempts` times while it fails with a stale cancel.
/// Each attempt gets the minimum context slot its market read must reach,
/// and returns the slot it read the market at (if it got that far): a
/// retry must not read the pre-fill book from a lagging node again.
async fn retry_stale<T, F, Fut>(
    action: &'static str,
    market: Pubkey,
    attempts: usize,
    mut attempt: F,
) -> Result<T>
where
    F: FnMut(Option<u64>) -> Fut,
    Fut: std::future::Future<Output = (Result<T>, Option<u64>)>,
{
    let mut min_context_slot = None;
    let mut tries = 1;
    loop {
        match attempt(min_context_slot).await {
            (Err(error), loaded_slot) if error.is_stale_cancel() && tries < attempts => {
                counter!("manifest_stale_cancel_retries_total", "action" => action).increment(1);
                warn!(%market, error = %error, "a cancelled order was filled first; retrying on a fresh book");
                min_context_slot = retry_min_context_slot(&error, loaded_slot);
                tries += 1;
            }
            (result, _) => return result,
        }
    }
}

/// Where a retry after `error` must read from: a landed failure's slot
/// (the fill is in an earlier slot), else past the slot the failed attempt
/// read (preflight gives no slot; the fill was not in that read).
fn retry_min_context_slot(error: &ManifestError, loaded_slot: Option<u64>) -> Option<u64> {
    match error {
        ManifestError::ExecutionFailure { slot, .. } => Some(*slot),
        _ => loaded_slot.map(|slot| slot.saturating_add(1)),
    }
}

fn cancels_of(book: &Book, trader_index: u32) -> Vec<CancelOrderParams> {
    book.orders_of(trader_index)
        .map(|order| CancelOrderParams {
            order_sequence_number: order.sequence_number,
            order_index_hint: Some(order.index),
        })
        .collect()
}

/// A batch ready to send: the seat, its cancels, and the fitted orders.
struct BatchPlan {
    seat: Seat,
    cancels: Vec<CancelOrderParams>,
    orders: Vec<PlaceOrderParams>,
    placed: Vec<OrderSpec>,
}

/// Size, convert and fit the orders `build` asks for on `snapshot`.
fn plan_batch<F>(
    snapshot: &MarketSnapshot,
    payer: Pubkey,
    build: &F,
    cancel_existing: bool,
) -> Result<BatchPlan>
where
    F: Fn(&MarketSnapshot, &Seat) -> Vec<OrderSpec>,
{
    let book = &snapshot.book;
    let seat = book.seat(&payer).ok_or(ManifestError::MissingSeat {
        market: snapshot.market,
        trader: payer,
    })?;
    let requested = build(snapshot, &seat);
    if requested.is_empty() {
        return Err(ManifestError::InvalidInput(
            "at least one order is required".to_owned(),
        ));
    }
    let params = requested
        .iter()
        .map(|order| {
            ensure_positive(order.price, "order price")?;
            let (mantissa, exponent) = price_to_mantissa(
                order.price,
                book.base_decimals,
                book.quote_decimals,
                order.side == Side::Bid,
            )?;
            Ok(PlaceOrderParams {
                base_atoms: ui_to_raw(order.base_amount, book.base_decimals)?,
                price_mantissa: mantissa,
                price_exponent: exponent,
                is_bid: order.side == Side::Bid,
                last_valid_slot: 0,
                order_type: OrderType::Limit,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let (cancels, (base_budget, quote_budget)) = if cancel_existing {
        let (base, quote) = book.locked_by(seat.index).map_err(quote_error)?;
        (
            cancels_of(book, seat.index),
            (
                seat.base_withdrawable.saturating_add(base),
                seat.quote_withdrawable.saturating_add(quote),
            ),
        )
    } else {
        (vec![], (seat.base_withdrawable, seat.quote_withdrawable))
    };
    let fitted = fit_orders(&params, base_budget, quote_budget)?;
    let mut orders = Vec::with_capacity(params.len());
    let mut placed = Vec::with_capacity(params.len());
    for ((mut param, spec), base_atoms) in params.into_iter().zip(&requested).zip(fitted) {
        if base_atoms < param.base_atoms {
            let side = if param.is_bid { "bid" } else { "ask" };
            counter!("manifest_orders_trimmed_total", "side" => side).increment(1);
        }
        if base_atoms == 0 {
            continue;
        }
        param.base_atoms = base_atoms;
        orders.push(param);
        placed.push(OrderSpec {
            base_amount: Decimal::from_i128_with_scale(
                i128::from(base_atoms),
                u32::from(book.base_decimals),
            ),
            ..*spec
        });
    }
    if orders.is_empty() && cancels.is_empty() {
        let (mint, available) = match requested[0].side {
            Side::Bid => (book.quote_mint, quote_budget),
            Side::Ask => (book.base_mint, base_budget),
        };
        return Err(ManifestError::InsufficientBalance {
            mint,
            required: available.saturating_add(1),
            available,
        });
    }
    Ok(BatchPlan {
        seat,
        cancels,
        orders,
        placed,
    })
}

/// Base atoms each of `orders` can keep so the batch is fundable from
/// `base_budget` (asks lock their base) and `quote_budget` (bids lock
/// [`book::locked_quote_atoms`] plus [`BID_MARGIN_ATOMS`]). Each side is
/// funded best price first; the first order that does not fit is shrunk
/// to what is left and every worse one gets 0.
fn fit_orders(
    orders: &[PlaceOrderParams],
    base_budget: u64,
    quote_budget: u64,
) -> Result<Vec<u64>> {
    let prices = orders
        .iter()
        .map(|order| book::price_raw(order.price_mantissa, order.price_exponent))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(quote_error)?;
    let mut fitted = vec![0_u64; orders.len()];
    for is_bid in [true, false] {
        let mut side: Vec<usize> = (0..orders.len())
            .filter(|&index| orders[index].is_bid == is_bid)
            .collect();
        // Best first: highest bid, lowest ask (stable on ties).
        side.sort_by(|&a, &b| {
            if is_bid {
                prices[b].cmp(&prices[a])
            } else {
                prices[a].cmp(&prices[b])
            }
        });
        let mut remaining = if is_bid { quote_budget } else { base_budget };
        for index in side {
            let wanted = orders[index].base_atoms;
            let base = if is_bid {
                let room = remaining.saturating_sub(BID_MARGIN_ATOMS);
                let base = wanted.min(book::max_bid_base_atoms(prices[index], room));
                if base > 0 {
                    let lock =
                        book::locked_quote_atoms(prices[index], base).map_err(quote_error)?;
                    remaining = remaining.saturating_sub(lock.saturating_add(BID_MARGIN_ATOMS));
                }
                base
            } else {
                let base = wanted.min(remaining);
                remaining -= base;
                base
            };
            fitted[index] = base;
            if base < wanted {
                // Budget exhausted: nothing worse is placed.
                remaining = 0;
            }
        }
    }
    Ok(fitted)
}

fn quote_error(error: BookError) -> ManifestError {
    ManifestError::Quote(error.to_string())
}

/// Attempts for a batch that cancels resting orders. A taker can fill one
/// between the snapshot and landing; the program then rejects the whole
/// batch (`WrongIndexHintParams` with an index hint, `InvalidCancel`
/// without), so one rebuild from a fresh book is worth it: at x1000 this
/// was 13% of all maker transactions.
const STALE_CANCEL_ATTEMPTS: usize = 2;

/// `Custom(n)` codes of the program's stale-cancel errors.
fn stale_cancel_codes() -> [String; 2] {
    use manifest_sdk::errors::ManifestError as Program;
    [
        format!("Custom({}))", Program::WrongIndexHintParams as u32),
        format!("Custom({}))", Program::InvalidCancel as u32),
    ]
}

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("unsupported token {mint}: {reason}")]
    UnsupportedToken { mint: Pubkey, reason: String },
    #[error("malformed account {address}: {reason}")]
    MalformedAccount { address: Pubkey, reason: String },
    #[error("{trader} has no seat on market {market}")]
    MissingSeat { market: Pubkey, trader: Pubkey },
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
    /// The transaction landed and failed: its fee was paid.
    #[error("transaction {signature} failed in slot {slot}: {error}")]
    ExecutionFailure {
        signature: Signature,
        slot: u64,
        error: String,
    },
    #[error("confirmation of {signature} is ambiguous: {reason}")]
    AmbiguousConfirmation {
        signature: Signature,
        reason: String,
    },
}

impl ManifestError {
    /// The batch failed because a cancel named an order that was no longer
    /// resting (filled since the snapshot), on chain or in preflight.
    pub fn is_stale_cancel(&self) -> bool {
        let error = match self {
            Self::ExecutionFailure { error, .. } | Self::SimulationFailure { error, .. } => error,
            _ => return false,
        };
        stale_cancel_codes()
            .iter()
            .any(|code| error.contains(code.as_str()))
    }
}

struct SeatLeg {
    mint: Pubkey,
    vault: Pubkey,
    token_program: Pubkey,
    decimals: u8,
    trader_token: Pubkey,
}

fn address(key: Pubkey) -> solana_address::Address {
    solana_address::Address::new_from_array(key.to_bytes())
}

/// `getMultipleAccounts` at the client's commitment and, when given, a
/// minimum context slot; returns the slot the accounts were read at.
async fn fetch_accounts(
    chain: &ChainClient,
    keys: &[Pubkey],
    min_context_slot: Option<u64>,
) -> Result<(u64, Vec<Option<Account>>)> {
    let config = RpcAccountInfoConfig {
        encoding: Some(UiAccountEncoding::Base64),
        commitment: Some(chain.rpc().commitment()),
        min_context_slot,
        ..RpcAccountInfoConfig::default()
    };
    let response = chain
        .rpc()
        .get_multiple_ui_accounts_with_config(keys, config)
        .await
        .map_err(|error| ManifestError::Rpc(error.to_string()))?;
    let accounts = response
        .value
        .into_iter()
        .zip(keys)
        .map(|(account, key)| {
            account
                .map(|account| {
                    let malformed = |reason: &str| ManifestError::MalformedAccount {
                        address: *key,
                        reason: reason.to_owned(),
                    };
                    Ok(Account {
                        lamports: account.lamports,
                        data: account
                            .data
                            .decode()
                            .ok_or_else(|| malformed("account data is not base64"))?,
                        owner: account
                            .owner
                            .parse()
                            .map_err(|_| malformed("account owner is not a public key"))?,
                        executable: account.executable,
                        rent_epoch: account.rent_epoch,
                    })
                })
                .transpose()
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((response.context.slot, accounts))
}

fn retarget(mut ix: Instruction, program_id: Pubkey) -> Instruction {
    ix.program_id = program_id;
    ix
}

/// codama fills absent optional accounts with the canonical program ID; the
/// program expects trailing optional accounts to be omitted instead.
fn without_placeholders(mut ix: Instruction, program_id: Pubkey) -> Instruction {
    ix.accounts
        .retain(|meta| meta.pubkey != CANONICAL_PROGRAM_ID);
    retarget(ix, program_id)
}

fn create_account_instruction(
    payer: Pubkey,
    account: Pubkey,
    lamports: u64,
    space: u64,
    owner: Pubkey,
) -> Instruction {
    let mut data = 0_u32.to_le_bytes().to_vec();
    data.extend_from_slice(&lamports.to_le_bytes());
    data.extend_from_slice(&space.to_le_bytes());
    data.extend_from_slice(owner.as_ref());
    Instruction {
        program_id: spl::SYSTEM_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(payer, true),
            AccountMeta::new(account, true),
        ],
        data,
    }
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
        .ok_or_else(|| ManifestError::MalformedAccount {
            address: keys.get(index).copied().unwrap_or_default(),
            reason: format!("missing {label}"),
        })
}

fn ensure_positive(value: Decimal, label: &str) -> Result<()> {
    if value <= Decimal::ZERO {
        Err(ManifestError::InvalidInput(format!(
            "{label} must be greater than zero"
        )))
    } else {
        Ok(())
    }
}

fn ui_to_raw(amount: Decimal, decimals: u8) -> Result<u64> {
    spl::ui_to_raw(amount, decimals).map_err(ManifestError::InvalidInput)
}

/// UI price (quote per base) → Manifest `(mantissa, exponent)` in atoms.
/// UI price (quote per base) as the program's price mantissa and exponent.
/// Bids round down and asks up, so neither rests at a better price for the
/// taker than requested.
fn price_to_mantissa(
    price: Decimal,
    base_decimals: u8,
    quote_decimals: u8,
    is_bid: bool,
) -> Result<(u32, i8)> {
    let mantissa = u128::try_from(price.mantissa())
        .map_err(|_| ManifestError::InvalidInput("price must be positive".to_owned()))?;
    let numerator = 10_u128
        .checked_pow(u32::from(quote_decimals))
        .and_then(|scale| mantissa.checked_mul(scale))
        .ok_or_else(|| ManifestError::InvalidInput("price overflow".to_owned()))?;
    let denominator = 10_u128
        .checked_pow(price.scale() + u32::from(base_decimals))
        .ok_or_else(|| ManifestError::InvalidInput("price overflow".to_owned()))?;
    book::price_mantissa_exponent(numerator, denominator, !is_bid)
        .map_err(|error| ManifestError::InvalidInput(error.to_string()))
}

fn apply_slippage(amount: u64, bps: u16, increase: bool) -> Result<u64> {
    let denominator = 10_000_u128;
    let adjusted = if increase {
        (u128::from(amount) * (denominator + u128::from(bps))).div_ceil(denominator)
    } else {
        u128::from(amount) * (denominator - u128::from(bps)) / denominator
    };
    u64::try_from(adjusted).map_err(|_| ManifestError::Quote("slippage overflow".to_owned()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn stale_cancels_are_recognised() {
        let failed = |error: &str| ManifestError::ExecutionFailure {
            signature: Signature::default(),
            slot: 1,
            error: error.to_owned(),
        };
        assert!(failed("InstructionError(2, Custom(10))").is_stale_cancel());
        assert!(failed("InstructionError(0, Custom(3))").is_stale_cancel());
        assert!(!failed("InstructionError(2, Custom(30))").is_stale_cancel());
        assert!(!failed("InstructionError(2, Custom(1))").is_stale_cancel());
        let simulated = ManifestError::SimulationFailure {
            error: "InstructionError(2, Custom(10))".to_owned(),
            logs: vec![],
        };
        assert!(simulated.is_stale_cancel());
        assert!(!ManifestError::Rpc("Custom(10))".to_owned()).is_stale_cancel());
    }

    use super::*;

    #[test]
    fn ui_price_converts_through_decimals() {
        // 1.5 quote per base, base 9 decimals, quote 6 → 0.0015 quote atoms per base atom
        assert_eq!(
            price_to_mantissa(Decimal::new(15, 1), 9, 6, true).expect("fits"),
            (1_500_000_000, -12)
        );
        assert_eq!(
            price_to_mantissa(Decimal::ONE, 6, 6, true).expect("fits"),
            (1_000_000_000, -9)
        );
    }

    #[test]
    fn placeholders_are_stripped_and_program_retargeted() {
        let fork = Pubkey::new_from_array([42; 32]);
        let snapshot = MarketSnapshot {
            program_id: fork,
            market: Pubkey::new_from_array([5; 32]),
            book: Book {
                base_mint: Pubkey::new_from_array([1; 32]),
                quote_mint: Pubkey::new_from_array([2; 32]),
                base_vault: Pubkey::new_from_array([3; 32]),
                quote_vault: Pubkey::new_from_array([4; 32]),
                base_decimals: 6,
                quote_decimals: 6,
                bids: vec![],
                asks: vec![],
                seats: vec![],
                has_free_block: false,
            },
            base_token_program: spl::TOKEN_PROGRAM_ID,
            quote_token_program: spl::TOKEN_PROGRAM_ID,
            slot: 1,
        };
        let metas = snapshot.swap_account_metas(Pubkey::new_from_array([9; 32]));
        assert_eq!(metas.len(), 12);
        assert!(metas.iter().all(|meta| meta.pubkey != CANONICAL_PROGRAM_ID));
        assert!(metas[0].is_signer && metas[1].is_signer);
    }

    #[test]
    fn slippage_rounds_against_the_trader() {
        assert_eq!(apply_slippage(10_000, 50, false).expect("fits"), 9_950);
        assert_eq!(apply_slippage(10_001, 50, true).expect("fits"), 10_052);
    }
}

#[cfg(test)]
mod quote_tests {
    use super::*;
    use crate::book::BookOrder;

    const D18: u128 = 1_000_000_000_000_000_000;

    fn snapshot(bids: Vec<BookOrder>, asks: Vec<BookOrder>) -> MarketSnapshot {
        MarketSnapshot {
            program_id: Pubkey::default(),
            market: Pubkey::default(),
            book: Book {
                base_mint: Pubkey::new_from_array([1; 32]),
                quote_mint: Pubkey::new_from_array([2; 32]),
                base_vault: Pubkey::default(),
                quote_vault: Pubkey::default(),
                base_decimals: 6,
                quote_decimals: 6,
                bids,
                asks,
                seats: vec![],
                has_free_block: false,
            },
            base_token_program: Pubkey::default(),
            quote_token_program: Pubkey::default(),
            slot: 1,
        }
    }

    fn order_at(price_raw: u128, base_atoms: u64) -> BookOrder {
        BookOrder {
            index: 0,
            price_raw,
            base_atoms,
            sequence_number: 0,
            trader_index: 0,
            last_valid_slot: 0,
            is_global: false,
        }
    }

    fn asks() -> MarketSnapshot {
        snapshot(
            vec![],
            vec![order_at(3 * D18 / 2, 100), order_at(2 * D18, 100)],
        )
    }

    #[test]
    fn quote_in_accepts_rounding_dust() {
        let (base, quote) = (
            Pubkey::new_from_array([1; 32]),
            Pubkey::new_from_array([2; 32]),
        );
        let swap = asks().quote(quote, base, 251, true, 50).expect("dust only");
        assert_eq!(
            (swap.expected_input_raw, swap.expected_output_raw),
            (250, 150)
        );
    }

    #[test]
    fn quote_in_beyond_the_book_is_rejected() {
        let (base, quote) = (
            Pubkey::new_from_array([1; 32]),
            Pubkey::new_from_array([2; 32]),
        );
        assert!(asks().quote(quote, base, 400, true, 50).is_err());
    }

    #[test]
    fn base_in_shortfall_is_rejected() {
        let (base, quote) = (
            Pubkey::new_from_array([1; 32]),
            Pubkey::new_from_array([2; 32]),
        );
        let bids = snapshot(vec![order_at(2 * D18, 100)], vec![]);
        assert!(bids.quote(base, quote, 150, true, 50).is_err());
        assert!(bids.quote(base, quote, 100, true, 50).is_ok());
    }
}

#[cfg(test)]
mod batch_tests {
    use std::cell::RefCell;
    use std::future::Future;
    use std::str::FromStr;
    use std::task::{Context, Poll, Waker};

    use super::*;

    /// Drive a future that never waits (the fakes here are all ready).
    fn ready<T>(future: impl Future<Output = T>) -> T {
        let mut future = std::pin::pin!(future);
        match future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("fake future is not ready"),
        }
    }

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).expect("decimal")
    }

    fn param(
        is_bid: bool,
        price: Decimal,
        base_atoms: u64,
        decimals: (u8, u8),
    ) -> PlaceOrderParams {
        let (price_mantissa, price_exponent) =
            price_to_mantissa(price, decimals.0, decimals.1, is_bid).expect("price");
        PlaceOrderParams {
            base_atoms,
            price_mantissa,
            price_exponent,
            is_bid,
            last_valid_slot: 0,
            order_type: OrderType::Limit,
        }
    }

    /// Quote and base the fitted orders need, bid margin included.
    fn needs(orders: &[PlaceOrderParams], fitted: &[u64]) -> (u64, u64) {
        let (mut base, mut quote) = (0_u64, 0_u64);
        for (order, &atoms) in orders.iter().zip(fitted) {
            if atoms == 0 {
                continue;
            }
            if order.is_bid {
                let price =
                    book::price_raw(order.price_mantissa, order.price_exponent).expect("in range");
                quote +=
                    book::locked_quote_atoms(price, atoms).expect("in range") + BID_MARGIN_ATOMS;
            } else {
                base += atoms;
            }
        }
        (base, quote)
    }

    #[test]
    fn a_drained_bid_ladder_is_fitted_to_the_quote() {
        // 6/6 decimals, Q = 10 USDC over 3 levels, each sized as
        // floor(Q / L / p): unfitted, the locks round up past Q.
        let quote = 10_000_000_u64;
        let orders: Vec<_> = ["0.4990", "0.4983", "0.4975"]
            .iter()
            .map(|price| {
                let base = (Decimal::from(quote / 3) / d(price)).floor();
                param(
                    true,
                    d(price),
                    u64::try_from(base.mantissa()).expect("in range"),
                    (6, 6),
                )
            })
            .collect();
        let unfitted: Vec<u64> = orders.iter().map(|o| o.base_atoms).collect();
        assert!(
            needs(&orders, &unfitted).1 > quote,
            "the bug: over-committed"
        );
        let fitted = fit_orders(&orders, 0, quote).expect("fits");
        assert!(needs(&orders, &fitted).1 <= quote);
        // Best two untouched, the worst shrunk rather than dropped.
        assert_eq!(fitted[..2], unfitted[..2]);
        assert!(fitted[2] > 0 && fitted[2] < unfitted[2]);
    }

    #[test]
    fn fitting_never_exceeds_the_budget() {
        // Deterministic pseudo-random cases (no property-test dependency).
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = move |bound: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % bound.max(1)
        };
        for _ in 0..5_000 {
            let decimals = [(6, 6), (9, 6), (5, 6), (6, 9)][next(4) as usize];
            let base_budget = next(1_000_000_000_000);
            let quote_budget = next(1_000_000_000_000);
            let levels = 1 + next(8);
            let mut orders = Vec::new();
            for _ in 0..levels {
                // p in (0, 2), four to eight significant figures.
                let price = Decimal::new(1 + next(199_999_999) as i64, 8).normalize();
                let wanted_quote = Decimal::from(next(quote_budget.max(1) * 2));
                let bid_base = (wanted_quote / price)
                    .floor()
                    .min(Decimal::from(u64::MAX / 4));
                let bid_base =
                    u64::try_from(bid_base.mantissa()).unwrap_or(0) / 10_u64.pow(bid_base.scale());
                orders.push(param(true, price, bid_base, decimals));
                orders.push(param(
                    false,
                    price * d("1.01"),
                    next(base_budget.max(1) * 2),
                    decimals,
                ));
            }
            let fitted = fit_orders(&orders, base_budget, quote_budget).expect("fits");
            let (base, quote) = needs(&orders, &fitted);
            assert!(base <= base_budget, "asks {base} > {base_budget}");
            assert!(quote <= quote_budget, "bids {quote} > {quote_budget}");
            for (order, &atoms) in orders.iter().zip(&fitted) {
                assert!(atoms <= order.base_atoms);
            }
            // Worst-price-first: a trimmed order has nothing worse behind it.
            for (i, order) in orders.iter().enumerate() {
                if fitted[i] < order.base_atoms {
                    let price = book::price_raw(order.price_mantissa, order.price_exponent)
                        .expect("in range");
                    for (j, other) in orders.iter().enumerate() {
                        let other_price =
                            book::price_raw(other.price_mantissa, other.price_exponent)
                                .expect("in range");
                        let worse = if order.is_bid {
                            other_price < price
                        } else {
                            other_price > price
                        };
                        if other.is_bid == order.is_bid && worse {
                            assert_eq!(fitted[j], 0, "worse order kept past a trimmed one");
                        }
                    }
                }
            }
            // A batch that already fits is left alone.
            let full: Vec<u64> = orders.iter().map(|o| o.base_atoms).collect();
            let (base, quote) = needs(&orders, &full);
            if base <= base_budget && quote <= quote_budget {
                assert_eq!(fitted, full);
            }
        }
    }

    fn snapshot(seat: Seat, bids: Vec<BookOrder>, slot: u64) -> MarketSnapshot {
        MarketSnapshot {
            program_id: Pubkey::default(),
            market: Pubkey::new_from_array([5; 32]),
            book: Book {
                base_mint: Pubkey::new_from_array([1; 32]),
                quote_mint: Pubkey::new_from_array([2; 32]),
                base_vault: Pubkey::default(),
                quote_vault: Pubkey::default(),
                base_decimals: 6,
                quote_decimals: 6,
                bids,
                asks: vec![],
                seats: vec![seat],
                has_free_block: false,
            },
            base_token_program: Pubkey::default(),
            quote_token_program: Pubkey::default(),
            slot,
        }
    }

    fn stale(slot: u64) -> ManifestError {
        ManifestError::ExecutionFailure {
            signature: Signature::default(),
            slot,
            error: "InstructionError(0, Custom(10))".to_owned(),
        }
    }

    #[test]
    fn cancels_refund_into_the_budget() {
        let trader = Pubkey::new_from_array([9; 32]);
        let seat = Seat {
            index: 80,
            trader,
            base_withdrawable: 0,
            quote_withdrawable: 1_000,
        };
        // A resting bid of 4_000 base at 0.5 locks 2_000 quote.
        let resting = BookOrder {
            index: 160,
            price_raw: 500_000_000_000_000_000,
            base_atoms: 4_000,
            sequence_number: 7,
            trader_index: 80,
            last_valid_slot: 0,
            is_global: false,
        };
        let market = snapshot(seat, vec![resting], 10);
        let build = |_: &MarketSnapshot, _: &Seat| {
            vec![OrderSpec {
                side: Side::Bid,
                price: d("0.5"),
                base_amount: d("0.01"),
            }]
        };
        // Replacing: 1_000 free + 2_000 refunded, less the margin.
        let plan = plan_batch(&market, trader, &build, true).expect("plan");
        assert_eq!(plan.cancels.len(), 1);
        assert_eq!(plan.orders[0].base_atoms, 5_998);
        assert_eq!(plan.placed[0].base_amount, d("0.005998"));
        // Placing alongside: only the free 1_000.
        let plan = plan_batch(&market, trader, &build, false).expect("plan");
        assert_eq!(plan.orders[0].base_atoms, 1_998);
        // Nothing fundable and nothing to cancel is an error, not a send.
        let empty = snapshot(
            Seat {
                quote_withdrawable: 0,
                ..seat
            },
            vec![],
            10,
        );
        assert!(matches!(
            plan_batch(&empty, trader, &build, true),
            Err(ManifestError::InsufficientBalance { .. })
        ));
    }

    #[test]
    fn a_stale_retry_resizes_from_a_pinned_post_fill_read() {
        let trader = Pubkey::new_from_array([9; 32]);
        let seat = |quote| Seat {
            index: 80,
            trader,
            base_withdrawable: 0,
            quote_withdrawable: quote,
        };
        // Attempt 1 sees 10 USDC; a fill then leaves 4.
        let books = [
            snapshot(seat(10_000_000), vec![], 70),
            snapshot(seat(4_000_000), vec![], 78),
        ];
        let seen = RefCell::new(Vec::new());
        let reads = RefCell::new(Vec::new());
        let build = |_: &MarketSnapshot, seat: &Seat| {
            seen.borrow_mut().push(seat.quote_withdrawable);
            vec![OrderSpec {
                side: Side::Bid,
                price: d("1"),
                base_amount: d("10"),
            }]
        };
        let outcome = ready(retry_stale(
            "replace_orders",
            Pubkey::default(),
            2,
            |min_slot| {
                let attempt = reads.borrow().len();
                reads.borrow_mut().push(min_slot);
                let market = &books[attempt];
                let result = plan_batch(market, trader, &build, true).and_then(|plan| {
                    if attempt == 0 {
                        Err(stale(77))
                    } else {
                        Ok(plan)
                    }
                });
                async move { (result, Some(market.slot)) }
            },
        ))
        .expect("second attempt");
        assert_eq!(*seen.borrow(), vec![10_000_000, 4_000_000]);
        assert_eq!(*reads.borrow(), vec![None, Some(77)]);
        assert_eq!(outcome.orders[0].base_atoms, 3_999_999);
    }

    #[test]
    fn a_preflight_stale_cancel_reads_past_the_failed_snapshot() {
        let simulated = ManifestError::SimulationFailure {
            error: "InstructionError(0, Custom(10))".to_owned(),
            logs: vec![],
        };
        assert_eq!(retry_min_context_slot(&simulated, Some(70)), Some(71));
        assert_eq!(retry_min_context_slot(&stale(77), Some(70)), Some(77));
        // The retry budget still caps attempts.
        let calls = RefCell::new(0);
        let result: Result<()> = ready(retry_stale(
            "cancel_all_orders",
            Pubkey::default(),
            2,
            |_| {
                *calls.borrow_mut() += 1;
                async { (Err(stale(5)), Some(4)) }
            },
        ));
        assert!(result.expect_err("fails").is_stale_cancel());
        assert_eq!(*calls.borrow(), 2);
    }
}
