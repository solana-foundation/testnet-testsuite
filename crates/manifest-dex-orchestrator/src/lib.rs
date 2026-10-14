//! Full-flow orchestration built on top of [`manifest_dex_client`].
//!
//! The orchestrator owns sequencing and test-token funding. It receives signers
//! from its caller and never reads keypair files or environment variables.

use std::sync::Arc;

use manifest_dex_client::{
    BalanceOutcome, CreateMarketOutcome, CreateMarketParams, ManifestClient, ManifestError,
    OrderSpec, OrdersOutcome, SeatOutcome, Side, SwapExactInParams, SwapExactOutParams,
    SwapOutcome, TokenAmount,
};
pub use manifest_dex_client::{TransactionOutcome, find_markets};
use mtm_chain::{ChainClient, SubmissionConfig};
use mtm_dex_harness::funding::{FundingError, Grant, TestTokenFunder};
use rust_decimal::Decimal;
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use thiserror::Error;
use tracing::{Instrument, info, info_span, warn};

pub type Result<T> = std::result::Result<T, OrchestrationError>;

/// Transaction settings shared by funding and Manifest actions.
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

#[derive(Clone, Copy, Debug)]
pub struct MarketCreationParams {
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    /// Create another market even if one already trades this pair.
    pub allow_duplicate: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct MarketCreationOutcome {
    pub market: Pubkey,
    /// `None` when an existing market was reused.
    pub created: Option<CreateMarketOutcome>,
}

/// Human-readable parameters for one maker-and-taker lifecycle.
#[derive(Clone, Copy, Debug)]
pub struct UserFlowParams {
    pub market: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    /// UI units minted to the user before the flow begins.
    pub funding_base: Decimal,
    pub funding_quote: Decimal,
    /// UI units moved from the wallet into the seat.
    pub deposit_base: Decimal,
    pub deposit_quote: Decimal,
    /// Quote per base; the bid and ask straddle it by `spread_bps` each way.
    pub mid_price: Decimal,
    pub spread_bps: u16,
    /// Base UI units resting on each side.
    pub order_size: Decimal,
    /// Base UI units sold (exact-in) into the resting bid.
    pub exact_in_amount: Decimal,
    /// Base UI units bought (exact-out) from the resting ask.
    pub exact_out_amount: Decimal,
}

#[derive(Clone, Copy, Debug)]
pub struct FundingOutcome {
    pub transaction: TransactionOutcome,
    pub token_account_base: Pubkey,
    pub token_account_quote: Pubkey,
    pub amount_base_raw: u64,
    pub amount_quote_raw: u64,
}

#[derive(Clone, Debug)]
pub struct UserFlowOutcome {
    pub funding: FundingOutcome,
    pub claim_seat: SeatOutcome,
    pub deposit_base: BalanceOutcome,
    pub deposit_quote: BalanceOutcome,
    pub place_orders: OrdersOutcome,
    pub swap_exact_in: SwapOutcome,
    pub swap_exact_out: SwapOutcome,
    pub cancel_orders: OrdersOutcome,
    pub withdraw: BalanceOutcome,
}

#[derive(Clone)]
pub struct ManifestOrchestrator {
    chain: ChainClient,
    client: ManifestClient,
    funder: TestTokenFunder,
    program_id: Pubkey,
    payer: Pubkey,
}

impl ManifestOrchestrator {
    pub fn new(
        chain: ChainClient,
        program_id: Pubkey,
        payer: Arc<dyn Signer + Send + Sync>,
        mint_authority: Arc<dyn Signer + Send + Sync>,
        config: OrchestrationConfig,
    ) -> Result<Self> {
        let client = ManifestClient::new(
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

    /// Reuse the pair's first market as `find_markets` lists it (lowest
    /// address, not oldest) or create one.
    pub async fn create_market_flow(
        &self,
        params: MarketCreationParams,
    ) -> Result<MarketCreationOutcome> {
        if params.base_mint == params.quote_mint {
            return Err(OrchestrationError::InvalidInput(
                "base and quote mints must differ".to_owned(),
            ));
        }
        let span = info_span!(
            "manifest_market_creation",
            base_mint = %params.base_mint,
            quote_mint = %params.quote_mint,
            payer = %self.payer(),
        );
        // `.instrument` rather than `span.enter()`: an entered guard held across
        // `.await` attributes other tasks' events to this span.
        async {
            self.validate_program().await?;
            if !params.allow_duplicate
                && let Some(market) = self
                    .client
                    .find_markets(params.base_mint, params.quote_mint)
                    .await?
                    .first()
            {
                info!(%market, "reusing existing Manifest market");
                return Ok(MarketCreationOutcome {
                    market: *market,
                    created: None,
                });
            }
            let created = self
                .client
                .create_market(CreateMarketParams {
                    base_mint: params.base_mint,
                    quote_mint: params.quote_mint,
                })
                .await?;
            info!(market = %created.market, "Manifest market created");
            Ok(MarketCreationOutcome {
                market: created.market,
                created: Some(created),
            })
        }
        .instrument(span)
        .await
    }

    /// Fund the user, make and take on an existing market, then cancel and
    /// withdraw everything. This flow never creates a market.
    pub async fn run_user_flow(&self, params: UserFlowParams) -> Result<UserFlowOutcome> {
        validate_user_flow_params(params)?;
        let span = info_span!(
            "manifest_user_flow",
            market = %params.market,
            base_mint = %params.base_mint,
            quote_mint = %params.quote_mint,
            payer = %self.payer(),
        );
        async {
            self.validate_program().await?;

            let funding = self.fund_payer(params).await?;
            let claim_seat = self.client.claim_seat(params.market).await?;
            // From here the seat holds deposits and may rest orders: on failure,
            // cancel and withdraw (best effort) before returning the error, so
            // neither stays locked in the market nor pollutes later quotes.
            let trading = async {
                let deposit_base = self
                    .client
                    .deposit(
                        params.market,
                        TokenAmount {
                            mint: params.base_mint,
                            amount: params.deposit_base,
                        },
                    )
                    .await?;
                let deposit_quote = self
                    .client
                    .deposit(
                        params.market,
                        TokenAmount {
                            mint: params.quote_mint,
                            amount: params.deposit_quote,
                        },
                    )
                    .await?;
                let (bid, ask) = straddle(params.mid_price, params.spread_bps)?;
                let place_orders = self
                    .client
                    .place_orders(
                        params.market,
                        &[
                            OrderSpec {
                                side: Side::Bid,
                                price: bid,
                                base_amount: params.order_size,
                            },
                            OrderSpec {
                                side: Side::Ask,
                                price: ask,
                                base_amount: params.order_size,
                            },
                        ],
                    )
                    .await?;
                let swap_exact_in = self
                    .client
                    .swap_exact_in(SwapExactInParams {
                        market: params.market,
                        input_mint: params.base_mint,
                        output_mint: params.quote_mint,
                        amount_in: params.exact_in_amount,
                    })
                    .await?;
                let swap_exact_out = self
                    .client
                    .swap_exact_out(SwapExactOutParams {
                        market: params.market,
                        input_mint: params.quote_mint,
                        output_mint: params.base_mint,
                        amount_out: params.exact_out_amount,
                    })
                    .await?;
                Ok::<_, OrchestrationError>((
                    deposit_base,
                    deposit_quote,
                    place_orders,
                    swap_exact_in,
                    swap_exact_out,
                ))
            }
            .await;
            let (deposit_base, deposit_quote, place_orders, swap_exact_in, swap_exact_out) =
                match trading {
                    Ok(steps) => steps,
                    Err(error) => {
                        self.unwind(params.market).await;
                        return Err(error);
                    }
                };
            let cancel_orders = self.client.cancel_all_orders(params.market).await?;
            let withdraw = self.client.withdraw_all(params.market).await?;
            info!(market = %params.market, "Manifest user flow complete");
            Ok(UserFlowOutcome {
                funding,
                claim_seat,
                deposit_base,
                deposit_quote,
                place_orders,
                swap_exact_in,
                swap_exact_out,
                cancel_orders,
                withdraw,
            })
        }
        .instrument(span)
        .await
    }

    /// Best-effort cleanup after a failed flow: cancel the seat's resting
    /// orders, then withdraw its balances.
    async fn unwind(&self, market: Pubkey) {
        if let Err(error) = self.client.cancel_all_orders(market).await {
            warn!(%market, %error, "failed to cancel resting orders after a failed flow");
        }
        if let Err(error) = self.client.withdraw_all(market).await {
            warn!(%market, %error, "failed to withdraw the seat after a failed flow");
        }
    }

    async fn fund_payer(&self, params: UserFlowParams) -> Result<FundingOutcome> {
        let funded = self
            .funder
            .fund(&[
                Grant {
                    mint: params.base_mint,
                    amount: params.funding_base,
                },
                Grant {
                    mint: params.quote_mint,
                    amount: params.funding_quote,
                },
            ])
            .await?;
        Ok(FundingOutcome {
            transaction: TransactionOutcome {
                signature: funded.transaction.signature,
                confirmation_slot: funded.transaction.confirmation_slot,
                simulated_compute_units: funded.transaction.simulated_compute_units,
            },
            token_account_base: funded.grants[0].token_account,
            token_account_quote: funded.grants[1].token_account,
            amount_base_raw: funded.grants[0].amount_raw,
            amount_quote_raw: funded.grants[1].amount_raw,
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

/// `(bid, ask)` around `mid`, each `spread_bps` away.
fn straddle(mid: Decimal, spread_bps: u16) -> Result<(Decimal, Decimal)> {
    let offset = mid
        .checked_mul(Decimal::from(spread_bps))
        .and_then(|value| value.checked_div(Decimal::from(10_000)))
        .ok_or_else(|| OrchestrationError::InvalidInput("spread overflow".to_owned()))?;
    let bid = mid - offset;
    if bid <= Decimal::ZERO {
        return Err(OrchestrationError::InvalidInput(
            "spread_bps leaves a non-positive bid".to_owned(),
        ));
    }
    Ok((bid, mid + offset))
}

fn validate_user_flow_params(params: UserFlowParams) -> Result<()> {
    if params.base_mint == params.quote_mint {
        return Err(OrchestrationError::InvalidInput(
            "base and quote mints must differ".to_owned(),
        ));
    }
    for (name, value) in [
        ("funding_base", params.funding_base),
        ("funding_quote", params.funding_quote),
        ("deposit_base", params.deposit_base),
        ("deposit_quote", params.deposit_quote),
        ("mid_price", params.mid_price),
        ("order_size", params.order_size),
        ("exact_in_amount", params.exact_in_amount),
        ("exact_out_amount", params.exact_out_amount),
    ] {
        if value <= Decimal::ZERO {
            return Err(OrchestrationError::InvalidInput(format!(
                "{name} must be greater than zero"
            )));
        }
    }
    if params.deposit_base > params.funding_base || params.deposit_quote > params.funding_quote {
        return Err(OrchestrationError::InvalidInput(
            "deposits cannot exceed funding".to_owned(),
        ));
    }
    let (bid, ask) = straddle(params.mid_price, params.spread_bps)?;
    let notional = |base: Decimal, price: Decimal| {
        base.checked_mul(price)
            .ok_or_else(|| OrchestrationError::InvalidInput("order notional overflow".to_owned()))
    };
    if params.order_size > params.deposit_base
        || notional(params.order_size, bid)? > params.deposit_quote
    {
        return Err(OrchestrationError::InvalidInput(
            "deposits must cover one resting order per side".to_owned(),
        ));
    }
    if params.exact_in_amount > params.order_size || params.exact_out_amount > params.order_size {
        return Err(OrchestrationError::InvalidInput(
            "swaps must fit inside the resting orders".to_owned(),
        ));
    }
    // The taker swaps from its wallet, after the deposits.
    if params.exact_in_amount > params.funding_base - params.deposit_base {
        return Err(OrchestrationError::InvalidInput(
            "funding_base must cover the exact-in swap after deposits".to_owned(),
        ));
    }
    if notional(params.exact_out_amount, ask)? > params.funding_quote - params.deposit_quote {
        return Err(OrchestrationError::InvalidInput(
            "funding_quote must cover the exact-out swap after deposits".to_owned(),
        ));
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum OrchestrationError {
    #[error("invalid orchestration input: {0}")]
    InvalidInput(String),
    #[error("Manifest program account {0} does not exist")]
    MissingProgram(Pubkey),
    #[error("Manifest program account {0} is not executable")]
    InvalidProgram(Pubkey),
    #[error("RPC request failed: {0}")]
    Rpc(String),
    #[error(transparent)]
    Funding(#[from] FundingError),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> UserFlowParams {
        UserFlowParams {
            market: Pubkey::new_from_array([1; 32]),
            base_mint: Pubkey::new_from_array([2; 32]),
            quote_mint: Pubkey::new_from_array([3; 32]),
            funding_base: Decimal::from(10_000),
            funding_quote: Decimal::from(10_000),
            deposit_base: Decimal::from(1_000),
            deposit_quote: Decimal::from(1_000),
            mid_price: Decimal::ONE,
            spread_bps: 100,
            order_size: Decimal::from(100),
            exact_in_amount: Decimal::from(10),
            exact_out_amount: Decimal::from(5),
        }
    }

    #[test]
    fn straddles_the_mid() {
        assert_eq!(
            straddle(Decimal::ONE, 100).expect("fits"),
            (Decimal::new(99, 2), Decimal::new(101, 2))
        );
        assert!(straddle(Decimal::ONE, 10_000).is_err());
    }

    #[test]
    fn rejects_swaps_larger_than_resting_liquidity() {
        assert!(validate_user_flow_params(params()).is_ok());
        let too_big = UserFlowParams {
            exact_in_amount: Decimal::from(101),
            ..params()
        };
        assert!(validate_user_flow_params(too_big).is_err());
    }

    #[test]
    fn rejects_an_exact_in_swap_the_wallet_cannot_fund() {
        // Everything but 5 base goes into the seat; the swap needs 10 from the wallet.
        let short = UserFlowParams {
            funding_base: Decimal::from(1_005),
            ..params()
        };
        assert!(validate_user_flow_params(short).is_err());
    }
}
