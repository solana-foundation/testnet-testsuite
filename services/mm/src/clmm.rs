//! `clmm-range`: one concentrated position on a Raydium CLMM or Orca pool,
//! centred on the pool price and re-centred when the price leaves the inner
//! band. The open position is persisted so restarts pick it back up, and a
//! re-centre interrupted between its transactions resumes where it stopped.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use metrics::counter;
use mtm_dex_harness::spl;
use mtm_scenario::venue::{spot_price, to_atoms, to_ui};
use mtm_scenario::{Strategy, Venue};
use mtm_venues::PoolSnapshot;
use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use tracing::{info, warn};

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

/// Why an open did not happen.
enum OpenError {
    /// The venue computed (for its tick-snapped range) more of `mint` than
    /// the wallet holds.
    Short {
        required: u64,
        available: u64,
    },
    Other(anyhow::Error),
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
    ) -> std::result::Result<Pubkey, OpenError> {
        let opened = self
            .retry_on_slippage("open", || self.open_inner(pool, lower, upper, mint, amount))
            .await;
        match opened {
            Ok(position) => Ok(position),
            Err(error) => {
                let short = match error.downcast_ref::<orca_whirlpool_client::OrcaWhirlpoolError>()
                {
                    Some(orca_whirlpool_client::OrcaWhirlpoolError::InsufficientBalance {
                        required,
                        available,
                        ..
                    }) => Some((*required, *available)),
                    _ => match error.downcast_ref::<raydium_clmm_client::RaydiumClmmError>() {
                        Some(raydium_clmm_client::RaydiumClmmError::InsufficientBalance {
                            required,
                            available,
                            ..
                        }) => Some((*required, *available)),
                        _ => None,
                    },
                };
                Err(match short {
                    Some((required, available)) => OpenError::Short {
                        required,
                        available,
                    },
                    None => OpenError::Other(error),
                })
            }
        }
    }

    async fn open_inner(
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

    /// The venue's codes for a liquidity change whose amounts moved past
    /// their bounds between the read and landing: Raydium
    /// `PriceSlippageCheck` (6017, both directions); Orca `TokenMaxExceeded`
    /// (6017, adding) and `TokenMinSubceeded` (6018, removing).
    fn slippage_codes(&self) -> &'static [u32] {
        match self {
            Lp::Orca(_) => &[6017, 6018],
            Lp::Raydium(_) => &[6017],
        }
    }

    /// Run `call`, and once more if it failed on liquidity slippage: the
    /// clients re-read the pool on every call, so the retry prices the move
    /// that broke the first attempt. At x1000 (~1,260 swaps/s) 23 of 31
    /// Raydium re-centre transactions failed this way with the default
    /// 100 bps, each pushing the re-centre back a full tick.
    async fn retry_on_slippage<T, F, Fut>(&self, action: &'static str, mut call: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        match call().await {
            Err(error) if is_slippage(&error, self.slippage_codes()) => {
                counter!("mm_lp_slippage_retries_total", "action" => action).increment(1);
                warn!(action, error = %format!("{error:#}"), "liquidity moved past its bounds; retrying on a fresh read");
                call().await
            }
            result => result,
        }
    }

    /// Withdraw everything, collect fees, and close the position NFT. Each
    /// step runs only if still needed (`decrease` refuses a position with no
    /// liquidity), so a close that failed or timed out after any of its
    /// transactions finishes on the next attempt instead of failing forever.
    async fn close(&self, position: Pubkey, liquidity: u128) -> Result<()> {
        match self {
            Lp::Orca(client) => {
                if liquidity > 0 {
                    self.retry_on_slippage("decrease", || async {
                        Ok(client
                            .decrease_liquidity(orca_whirlpool_client::DecreaseLiquidityParams {
                                position_mint: position,
                                amount: None,
                            })
                            .await?)
                    })
                    .await?;
                }
                client.collect_position(position).await?;
                client.close_position(position).await?;
            }
            Lp::Raydium(client) => {
                if liquidity > 0 {
                    self.retry_on_slippage("decrease", || async {
                        Ok(client
                            .decrease_liquidity(raydium_clmm_client::DecreaseLiquidityParams {
                                position_mint: position,
                                amount: None,
                            })
                            .await?)
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

/// Whether a failed transaction's error is one of `codes` (`Custom(n)`).
fn is_slippage(error: &anyhow::Error, codes: &[u32]) -> bool {
    let text = format!("{error:#}");
    codes
        .iter()
        .any(|code| text.contains(&format!("Custom({code}))")))
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

        // Between positions everything is in the wallet: hedge it back to
        // the target mix, so a one-sided close does not leave a position
        // that can only be opened at a fraction of its size.
        crate::hedge::wallet(ctx, &prices).await?;
        // Balances after any close (and hedge): the withdrawn tokens are
        // back in the wallet and size the new position.
        let base_atoms = ctx.wallet_balance(&ctx.base).await?;
        let quote_atoms = ctx.wallet_balance(&ctx.quote).await?;
        ctx.record_inventory(&prices, base_atoms, quote_atoms);

        let half = Decimal::from(width_bps) / Decimal::from(20_000);
        let (lower, upper) = (
            center * (Decimal::ONE - half),
            center * (Decimal::ONE + half),
        );
        // Size from what the range actually takes: a CLMM range centred on
        // the price needs more quote than base by value (≈1.04x at ±3%,
        // ≈1.13x at ±12.5%), and the client reserves its slippage on top.
        // Keep 10% of each side in reserve.
        let (slippage_bps, _, _) = ctx.execution(100, 1_400_000);
        let quote_per_base =
            deposit_ratio(center, lower, upper).context("range has no deposit ratio")?;
        let mut amount = open_size(OpenSizing {
            position_usd,
            base_usd: prices.usd(&ctx.base)?,
            quote_usd: prices.usd(&ctx.quote)?,
            quote_per_base,
            slippage: Decimal::from(slippage_bps) / Decimal::from(10_000),
            base_free: to_ui(&ctx.base, base_atoms) * Decimal::new(9, 1),
            quote_free: to_ui(&ctx.quote, quote_atoms) * Decimal::new(9, 1),
        })
        .round_dp_with_strategy(u32::from(ctx.base.decimals), RoundingStrategy::ToZero);
        if amount.is_zero() || to_atoms(&ctx.base, amount).unwrap_or(0) == 0 {
            anyhow::bail!("not enough inventory to open a position");
        }
        // Venue clients take pool-order prices: mint B per mint A, A < B.
        let (lower, upper) = if ctx.base.mint < ctx.quote.mint {
            (lower, upper)
        } else {
            (Decimal::ONE / upper, Decimal::ONE / lower)
        };
        let (lower, upper) = (lower.round_dp(10), upper.round_dp(10));
        let mut retried = false;
        let mint = loop {
            match self
                .lp
                .open(ctx.pool, lower, upper, ctx.base.mint, amount)
                .await
            {
                Ok(mint) => break mint,
                // The venue snaps the range to its tick spacing, which can
                // shift the split further: shrink to what it asked for, once.
                Err(OpenError::Short {
                    required,
                    available,
                }) if !retried && required > 0 => {
                    retried = true;
                    let scale =
                        Decimal::from(available) / Decimal::from(required) * Decimal::new(98, 2);
                    amount = (amount * scale).round_dp_with_strategy(
                        u32::from(ctx.base.decimals),
                        RoundingStrategy::ToZero,
                    );
                    info!(maker = %ctx.maker.name, required, available, %amount, "position resized to the venue's requirement");
                }
                Err(OpenError::Short {
                    required,
                    available,
                }) => {
                    anyhow::bail!(
                        "opening position: needs {required} atoms, wallet holds {available}"
                    )
                }
                Err(OpenError::Other(error)) => return Err(error.context("opening position")),
            }
        };
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

/// Quote needed per unit of base (UI amounts) to fund a range `[lower,
/// upper]` (quote per base) at price `price` inside it:
/// `(√p − √pa) / (1/√p − 1/√pb)`.
fn deposit_ratio(price: Decimal, lower: Decimal, upper: Decimal) -> Option<Decimal> {
    use rust_decimal::MathematicalOps;
    if !(lower < price && price < upper) || lower <= Decimal::ZERO {
        return None;
    }
    let (sp, sa, sb) = (price.sqrt()?, lower.sqrt()?, upper.sqrt()?);
    let base = Decimal::ONE / sp - Decimal::ONE / sb;
    (base > Decimal::ZERO).then(|| (sp - sa) / base)
}

struct OpenSizing {
    position_usd: Decimal,
    base_usd: Decimal,
    quote_usd: Decimal,
    /// quote per base the range takes ([`deposit_ratio`])
    quote_per_base: Decimal,
    /// share the client reserves on top of each side
    slippage: Decimal,
    base_free: Decimal,
    quote_free: Decimal,
}

/// Base amount to open with: the position's USD target split as the range
/// takes it, capped by what each side can fund including slippage.
fn open_size(s: OpenSizing) -> Decimal {
    let wanted = s.position_usd / (s.base_usd + s.quote_per_base * s.quote_usd);
    let margin = Decimal::ONE + s.slippage;
    wanted
        .min(s.base_free / margin)
        .min(s.quote_free / (s.quote_per_base * margin))
}

#[cfg(test)]
mod tests {
    #[test]
    fn recognises_liquidity_slippage_by_code() {
        let failed = |code: u32| {
            anyhow::anyhow!("transaction x failed in slot 1: InstructionError(4, Custom({code}))")
        };
        assert!(is_slippage(&failed(6017), &[6017]));
        assert!(is_slippage(&failed(6018), &[6017, 6018]));
        assert!(!is_slippage(&failed(6018), &[6017]));
        assert!(!is_slippage(&failed(60170), &[6017]));
        assert!(!is_slippage(&anyhow::anyhow!("rpc timeout"), &[6017]));
    }

    use super::*;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).expect("decimal")
    }

    #[test]
    fn symmetric_ranges_take_more_quote_than_base() {
        // ±3% around 1: ≈1.03x quote per base; ±12.5%: ≈1.13x.
        let narrow = deposit_ratio(d("1"), d("0.97"), d("1.03")).expect("in range");
        assert!(narrow > d("1.02") && narrow < d("1.04"), "{narrow}");
        let wide = deposit_ratio(d("1"), d("0.875"), d("1.125")).expect("in range");
        assert!(wide > d("1.12") && wide < d("1.14"), "{wide}");
        // scales with the price
        let at_two = deposit_ratio(d("2"), d("1.94"), d("2.06")).expect("in range");
        assert!((at_two / narrow - d("2")).abs() < d("0.000001"), "{at_two}");
        assert!(deposit_ratio(d("1"), d("1.1"), d("1.2")).is_none());
    }

    #[test]
    fn sizing_funds_the_quote_side_with_slippage() {
        // A wide range in JUP/RAY terms (price 0.1734 RAY per JUP).
        let price = d("0.1734");
        let half = d("0.075");
        let ratio = deposit_ratio(
            price,
            price * (Decimal::ONE - half),
            price * (Decimal::ONE + half),
        )
        .expect("in range");
        let quote_free = d("4500") * d("0.9");
        let amount = open_size(OpenSizing {
            position_usd: d("1000000"),
            base_usd: d("0.32"),
            quote_usd: d("1.84"),
            quote_per_base: ratio,
            slippage: d("0.01"),
            base_free: d("1000000"),
            quote_free,
        })
        // as `tick` does: truncated to the base token's decimals
        .round_dp_with_strategy(6, RoundingStrategy::ToZero);
        // What the client will ask for never exceeds what the wallet holds.
        let required = amount * ratio * d("1.01");
        assert!(required <= quote_free, "{required} > {quote_free}");
        assert!(
            required > quote_free * d("0.999"),
            "quote-bound: uses the side fully"
        );
    }
}
