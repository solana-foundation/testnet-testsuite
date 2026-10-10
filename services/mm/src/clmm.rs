//! `clmm-range`: one concentrated position on a Raydium CLMM or Orca pool,
//! centred on the pool price and re-centred when the price leaves the inner
//! band. The open position is persisted so restarts pick it back up.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use mtm_scenario::venue::{spot_price, to_atoms, to_ui};
use mtm_scenario::{Strategy, Venue};
use mtm_venues::PoolSnapshot;
use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use tracing::info;

use crate::maker::{Ctx, deviation_bps};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Position {
    mint: String,
    /// Centre price (quote per base) the range was opened around.
    center: Decimal,
}

enum Lp {
    Orca(orca_whirlpool_client::OrcaWhirlpoolClient),
    Raydium(raydium_clmm_client::RaydiumClmmClient),
}

impl Lp {
    /// Open a position in pool-order prices with `amount` of `mint`.
    async fn open(
        &self,
        pool: Pubkey,
        lower: Decimal,
        upper: Decimal,
        mint: Pubkey,
        amount: Decimal,
    ) -> Result<Pubkey> {
        Ok(match self {
            Lp::Orca(client) => {
                client
                    .open_position(orca_whirlpool_client::OpenPositionParams {
                        pool,
                        lower_price: lower,
                        upper_price: upper,
                        input: orca_whirlpool_client::TokenAmount { mint, amount },
                        with_metadata: false,
                    })
                    .await?
                    .accounts
                    .position_mint
            }
            Lp::Raydium(client) => {
                client
                    .open_position(raydium_clmm_client::OpenPositionParams {
                        pool,
                        lower_price: lower,
                        upper_price: upper,
                        input: raydium_clmm_client::TokenAmount { mint, amount },
                        with_metadata: false,
                    })
                    .await?
                    .accounts
                    .position_nft_mint
            }
        })
    }

    /// Withdraw everything, collect fees, and close the position NFT.
    async fn close(&self, position: Pubkey) -> Result<()> {
        match self {
            Lp::Orca(client) => {
                client
                    .decrease_liquidity(orca_whirlpool_client::DecreaseLiquidityParams {
                        position_mint: position,
                        amount: None,
                    })
                    .await?;
                client.collect_position(position).await?;
                client.close_position(position).await?;
            }
            Lp::Raydium(client) => {
                client
                    .decrease_liquidity(raydium_clmm_client::DecreaseLiquidityParams {
                        position_mint: position,
                        amount: None,
                    })
                    .await?;
                client.collect_position(position).await?;
                client.close_position(position).await?;
            }
        }
        Ok(())
    }
}

pub struct Clmm {
    lp: Lp,
    state_file: PathBuf,
    position: Option<Position>,
}

impl Clmm {
    pub fn start(ctx: &Ctx) -> Result<Self> {
        let payer = Arc::clone(&ctx.wallet) as _;
        let confirmed = CommitmentConfig::confirmed();
        let program = ctx.scenario.programs.of(ctx.maker.venue);
        let lp = match ctx.maker.venue {
            Venue::OrcaWhirlpool => Lp::Orca(orca_whirlpool_client::OrcaWhirlpoolClient::new(
                ctx.chain.clone(),
                program,
                payer,
                100,
                1_400_000,
                0,
                confirmed,
            )?),
            Venue::RaydiumClmm => Lp::Raydium(raydium_clmm_client::RaydiumClmmClient::new(
                ctx.chain.clone(),
                program,
                payer,
                100,
                1_400_000,
                0,
                confirmed,
            )?),
            Venue::Manifest => anyhow::bail!("clmm-range does not run on Manifest"),
        };
        let state_file = ctx.state_dir.join(format!("{}.json", ctx.maker.name));
        let position = std::fs::read(&state_file)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        Ok(Self {
            lp,
            state_file,
            position,
        })
    }

    fn save(&self) -> Result<()> {
        match &self.position {
            Some(position) => std::fs::write(&self.state_file, serde_json::to_vec(position)?)?,
            None => {
                let _ = std::fs::remove_file(&self.state_file);
            }
        }
        Ok(())
    }

    pub async fn tick(&mut self, ctx: &Ctx) -> Result<()> {
        let Strategy::ClmmRange {
            width_bps,
            recenter_bps,
            position_usd,
        } = ctx.maker.strategy.clone()
        else {
            unreachable!("clmm maker with a non-clmm strategy");
        };
        let prices = ctx.prices().await?;
        let oracle = prices.pair(&ctx.base, &ctx.quote)?;
        let program = ctx.scenario.programs.of(ctx.maker.venue);
        let snapshot = PoolSnapshot::load(
            &ctx.chain,
            mtm_scenario::Scenario::venue_kind(ctx.maker.venue),
            program,
            ctx.pool,
        )
        .await?;
        let spot = spot_price(&snapshot, &ctx.base, &ctx.quote);
        ctx.record_prices(oracle, spot);
        let base_atoms = ctx.wallet_balance(&ctx.base).await?;
        let quote_atoms = ctx.wallet_balance(&ctx.quote).await?;
        ctx.record_inventory(&prices, base_atoms, quote_atoms);
        // An empty pool has no spot price yet: seed it at the oracle price.
        let center = spot.unwrap_or(oracle);

        if let Some(position) = &self.position {
            let drift = deviation_bps(center, position.center).abs();
            if drift < Decimal::from(recenter_bps) {
                return Ok(());
            }
            let mint = Pubkey::from_str(&position.mint)?;
            info!(maker = %ctx.maker.name, %mint, %drift, "re-centring position");
            self.lp.close(mint).await.context("closing position")?;
            self.position = None;
            self.save()?;
            ctx.action("close");
        }

        // Half the target value in base; keep 10% of each side in reserve.
        let base_usd = prices.usd(&ctx.base)?;
        let wanted = position_usd / Decimal::TWO / base_usd;
        let base_free = to_ui(&ctx.base, base_atoms) * Decimal::new(9, 1);
        let quote_free = to_ui(&ctx.quote, quote_atoms) * Decimal::new(9, 1) / center;
        let amount = wanted
            .min(base_free)
            .min(quote_free)
            .round_dp_with_strategy(u32::from(ctx.base.decimals), RoundingStrategy::ToZero);
        if amount.is_zero() || to_atoms(&ctx.base, amount).unwrap_or(0) == 0 {
            anyhow::bail!("not enough inventory to open a position");
        }
        let half = Decimal::from(width_bps) / Decimal::from(20_000);
        let (lower, upper) = (
            center * (Decimal::ONE - half),
            center * (Decimal::ONE + half),
        );
        // Venue clients take pool-order prices: mint B per mint A, A < B.
        let (lower, upper) = if ctx.base.mint < ctx.quote.mint {
            (lower, upper)
        } else {
            (Decimal::ONE / upper, Decimal::ONE / lower)
        };
        let mint = self
            .lp
            .open(
                ctx.pool,
                lower.round_dp(10),
                upper.round_dp(10),
                ctx.base.mint,
                amount,
            )
            .await
            .context("opening position")?;
        info!(maker = %ctx.maker.name, %mint, %center, %amount, "position opened");
        self.position = Some(Position {
            mint: mint.to_string(),
            center,
        });
        self.save()?;
        ctx.action("open");
        Ok(())
    }
}
