//! `clmm-range`: one concentrated position on a Raydium CLMM or Orca pool,
//! centred on the pool price and re-centred when the price leaves the inner
//! band. The open position is persisted so restarts pick it back up, and a
//! re-centre interrupted between its transactions resumes where it stopped.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use mtm_dex_harness::spl;
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

    /// Liquidity left in the position, and its pool-order token amounts.
    async fn value(&self, position: Pubkey) -> Result<(u128, Pubkey, u64, Pubkey, u64)> {
        Ok(match self {
            Lp::Orca(client) => {
                let value = client.position_value(position).await?;
                (
                    value.liquidity,
                    value.mint_a,
                    value.amount_a,
                    value.mint_b,
                    value.amount_b,
                )
            }
            Lp::Raydium(client) => {
                let value = client.position_value(position).await?;
                (
                    value.liquidity,
                    value.mint_a,
                    value.amount_a,
                    value.mint_b,
                    value.amount_b,
                )
            }
        })
    }

    /// Withdraw everything, collect fees, and close the position NFT. Each
    /// step runs only if still needed (`decrease` refuses a position with no
    /// liquidity), so a close that failed or timed out after any of its
    /// transactions finishes on the next attempt instead of failing forever.
    async fn close(&self, position: Pubkey, liquidity: u128) -> Result<()> {
        match self {
            Lp::Orca(client) => {
                if liquidity > 0 {
                    client
                        .decrease_liquidity(orca_whirlpool_client::DecreaseLiquidityParams {
                            position_mint: position,
                            amount: None,
                        })
                        .await?;
                }
                client.collect_position(position).await?;
                client.close_position(position).await?;
            }
            Lp::Raydium(client) => {
                if liquidity > 0 {
                    client
                        .decrease_liquidity(raydium_clmm_client::DecreaseLiquidityParams {
                            position_mint: position,
                            amount: None,
                        })
                        .await?;
                }
                client.collect_position(position).await?;
                client.close_position(position).await?;
            }
        }
        Ok(())
    }
}

/// Whether `owner` still holds the position NFT `mint` (in its classic or
/// Token-2022 ATA). Closing a position burns it, so a missing NFT means the
/// close already landed.
async fn holds_position(ctx: &Ctx, mint: Pubkey) -> Result<bool> {
    let owner = ctx.wallet_pubkey();
    let keys = [spl::TOKEN_PROGRAM_ID, spl::TOKEN_2022_PROGRAM_ID]
        .map(|program| spl::associated_token_address(&owner, &mint, &program));
    let accounts = ctx
        .chain
        .rpc()
        .get_multiple_accounts(&keys)
        .await
        .context("reading position NFT accounts")?;
    Ok(accounts.iter().flatten().any(|account| {
        spl::decode_token_account(&account.data).is_some_and(|state| state.amount > 0)
    }))
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
        let (slippage_bps, compute_units, priority_fee) = ctx.execution(100, 1_400_000);
        let lp = match ctx.maker.venue {
            Venue::OrcaWhirlpool => Lp::Orca(orca_whirlpool_client::OrcaWhirlpoolClient::new(
                ctx.chain.clone(),
                program,
                payer,
                slippage_bps,
                compute_units,
                priority_fee,
                confirmed,
            )?),
            Venue::RaydiumClmm => Lp::Raydium(raydium_clmm_client::RaydiumClmmClient::new(
                ctx.chain.clone(),
                program,
                payer,
                slippage_bps,
                compute_units,
                priority_fee,
                confirmed,
            )?),
            Venue::Manifest => anyhow::bail!("clmm-range does not run on Manifest"),
        };
        let state_file = ctx.state_dir.join(format!("{}.json", ctx.maker.name));
        // No file means no position; an unreadable one must not be taken
        // for "none", which would open a second position and orphan the first.
        let position = match std::fs::read(&state_file) {
            Ok(bytes) => {
                Some(serde_json::from_slice(&bytes).with_context(|| {
                    format!("corrupt maker state file {}", state_file.display())
                })?)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading maker state file {}", state_file.display()));
            }
        };
        Ok(Self {
            lp,
            state_file,
            position,
        })
    }

    /// Persist the position atomically (temp file, fsync, rename), so a
    /// crash never leaves a truncated state file behind.
    fn save(&self) -> Result<()> {
        match &self.position {
            Some(position) => {
                use std::io::Write;
                let tmp = self.state_file.with_extension("json.tmp");
                let mut file = std::fs::File::create(&tmp)
                    .with_context(|| format!("writing {}", tmp.display()))?;
                file.write_all(&serde_json::to_vec(position)?)?;
                file.sync_all()?;
                std::fs::rename(&tmp, &self.state_file)
                    .with_context(|| format!("replacing {}", self.state_file.display()))?;
            }
            None => match std::fs::remove_file(&self.state_file) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(error).context("removing maker state file");
                }
                _ => {}
            },
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
        let spot = spot_price(&snapshot, &ctx.base, &ctx.quote, oracle);
        ctx.record_prices(oracle, spot);
        // An empty pool has no spot price yet: seed it at the oracle price.
        let center = spot.unwrap_or(oracle);

        if let Some(position) = &self.position {
            let mint = Pubkey::from_str(&position.mint)?;
            let drift = deviation_bps(center, position.center).abs();
            if !holds_position(ctx, mint).await? {
                // The close landed but was not recorded (crash, timeout).
                info!(maker = %ctx.maker.name, %mint, "position already closed");
            } else {
                let (liquidity, mint_a, amount_a, _, amount_b) = self.lp.value(mint).await?;
                if drift < Decimal::from(recenter_bps) {
                    // Inventory includes what the position would return.
                    let (base_in, quote_in) = if mint_a == ctx.base.mint {
                        (amount_a, amount_b)
                    } else {
                        (amount_b, amount_a)
                    };
                    let base_atoms = ctx.wallet_balance(&ctx.base).await?.saturating_add(base_in);
                    let quote_atoms = ctx
                        .wallet_balance(&ctx.quote)
                        .await?
                        .saturating_add(quote_in);
                    ctx.record_inventory(&prices, base_atoms, quote_atoms);
                    return Ok(());
                }
                info!(maker = %ctx.maker.name, %mint, %drift, "re-centring position");
                self.lp
                    .close(mint, liquidity)
                    .await
                    .context("closing position")?;
                ctx.action("close");
            }
            self.position = None;
            self.save()?;
        }

        // Balances after any close: the withdrawn tokens are back in the
        // wallet and size the new position.
        let base_atoms = ctx.wallet_balance(&ctx.base).await?;
        let quote_atoms = ctx.wallet_balance(&ctx.quote).await?;
        ctx.record_inventory(&prices, base_atoms, quote_atoms);

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
