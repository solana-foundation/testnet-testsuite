//! Venue adapters for the router (docs/router-architecture.md §3).
//!
//! Each venue client exposes a swap snapshot: one RPC fetch, then pure,
//! simulation-exact quoting and instruction building for any owner. This
//! crate wraps the three snapshots in one enum so the router can price, split,
//! and chain legs without venue-specific code, and can emit each leg either as
//! top-level venue instructions or as a Jupiter-fork route step.

pub mod jupiter;

use std::sync::Arc;

use manifest_dex_client::MarketSnapshot;
use mtm_chain::ChainClient;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use solana_account_decoder_client_types::{UiAccountEncoding, UiDataSliceConfig};
use solana_commitment_config::CommitmentConfig;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;
use solana_rpc_client_api::config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
use solana_rpc_client_api::filter::{Memcmp, RpcFilterType};
use solana_signer::null_signer::NullSigner;

/// Measured on Surfpool (2026-10-10) including two idempotent ATA creations;
/// rounded up for headroom. Raydium adds a per-tick-array increment.
pub const ORCA_SWAP_CU: u32 = 100_000;
pub const MANIFEST_SWAP_CU: u32 = 40_000;
pub const RAYDIUM_SWAP_BASE_CU: u32 = 100_000;
pub const RAYDIUM_SWAP_CU_PER_TICK_ARRAY: u32 = 15_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VenueKind {
    RaydiumClmm,
    OrcaWhirlpool,
    Manifest,
}

impl VenueKind {
    pub const ALL: [VenueKind; 3] = [
        VenueKind::RaydiumClmm,
        VenueKind::OrcaWhirlpool,
        VenueKind::Manifest,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            VenueKind::RaydiumClmm => "raydium-clmm",
            VenueKind::OrcaWhirlpool => "orca-whirlpool",
            VenueKind::Manifest => "manifest",
        }
    }
}

impl std::str::FromStr for VenueKind {
    type Err = VenueError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        VenueKind::ALL
            .into_iter()
            .find(|kind| kind.as_str() == value)
            .ok_or_else(|| VenueError::UnknownVenue(value.to_owned()))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VenueError {
    #[error("unknown venue kind: {0}")]
    UnknownVenue(String),
    #[error("RPC request failed: {0}")]
    Rpc(String),
    #[error("{venue} pool {pool}: {reason}")]
    Venue {
        venue: &'static str,
        pool: Pubkey,
        reason: String,
    },
}

/// One venue's answer for a prospective leg, in raw atoms.
#[derive(Debug, Clone)]
pub struct LegQuote {
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    pub exact_in: bool,
    pub in_amount: u64,
    pub out_amount: u64,
    /// Minimum output the leg's instruction enforces (exact-in), or the
    /// requested output (exact-out).
    pub min_out: u64,
    /// Maximum input the leg's instruction may debit.
    pub max_in: u64,
    /// Estimated trade fee, in input atoms.
    pub fee_amount: u64,
    pub cu_estimate: u32,
    detail: QuoteDetail,
}

#[derive(Debug, Clone)]
enum QuoteDetail {
    RaydiumClmm(raydium_clmm_client::SwapQuote),
    OrcaWhirlpool(orca_whirlpool_client::SwapQuote),
    Manifest(manifest_dex_client::SwapQuote),
}

/// Swap state of one pool or market at one point in time.
#[derive(Clone)]
pub enum PoolSnapshot {
    RaydiumClmm(Box<raydium_clmm_client::SwapSnapshot>),
    OrcaWhirlpool(Box<orca_whirlpool_client::SwapSnapshot>),
    Manifest(Box<MarketSnapshot>),
}

impl PoolSnapshot {
    pub async fn load(
        chain: &ChainClient,
        kind: VenueKind,
        program_id: Pubkey,
        pool: Pubkey,
    ) -> Result<Self, VenueError> {
        // Snapshots never sign; the venue clients only need an identity.
        let reader = Arc::new(NullSigner::new(&Pubkey::default()));
        let venue = |reason: String| VenueError::Venue {
            venue: kind.as_str(),
            pool,
            reason,
        };
        match kind {
            VenueKind::RaydiumClmm => {
                let client = raydium_clmm_client::RaydiumClmmClient::new(
                    chain.clone(),
                    program_id,
                    reader,
                    0,
                    1_400_000,
                    0,
                    CommitmentConfig::confirmed(),
                )
                .map_err(|error| venue(error.to_string()))?;
                Ok(Self::RaydiumClmm(Box::new(
                    client
                        .load_swap_snapshot(pool)
                        .await
                        .map_err(|error| venue(error.to_string()))?,
                )))
            }
            VenueKind::OrcaWhirlpool => {
                let client = orca_whirlpool_client::OrcaWhirlpoolClient::new(
                    chain.clone(),
                    program_id,
                    reader,
                    0,
                    1_400_000,
                    0,
                    CommitmentConfig::confirmed(),
                )
                .map_err(|error| venue(error.to_string()))?;
                Ok(Self::OrcaWhirlpool(Box::new(
                    client
                        .load_swap_snapshot(pool)
                        .await
                        .map_err(|error| venue(error.to_string()))?,
                )))
            }
            VenueKind::Manifest => Ok(Self::Manifest(Box::new(
                MarketSnapshot::load(chain, program_id, pool)
                    .await
                    .map_err(|error| venue(error.to_string()))?,
            ))),
        }
    }

    pub fn kind(&self) -> VenueKind {
        match self {
            Self::RaydiumClmm(_) => VenueKind::RaydiumClmm,
            Self::OrcaWhirlpool(_) => VenueKind::OrcaWhirlpool,
            Self::Manifest(_) => VenueKind::Manifest,
        }
    }

    pub fn pool(&self) -> Pubkey {
        match self {
            Self::RaydiumClmm(snapshot) => snapshot.pool(),
            Self::OrcaWhirlpool(snapshot) => snapshot.pool(),
            Self::Manifest(snapshot) => snapshot.market,
        }
    }

    pub fn program_id(&self) -> Pubkey {
        match self {
            Self::RaydiumClmm(snapshot) => snapshot.program_id(),
            Self::OrcaWhirlpool(snapshot) => snapshot.program_id(),
            Self::Manifest(snapshot) => snapshot.program_id,
        }
    }

    /// The two mints, in venue order (pool order; base then quote).
    pub fn mints(&self) -> (Pubkey, Pubkey) {
        match self {
            Self::RaydiumClmm(snapshot) => snapshot.mints(),
            Self::OrcaWhirlpool(snapshot) => snapshot.mints(),
            Self::Manifest(snapshot) => (snapshot.book.base_mint, snapshot.book.quote_mint),
        }
    }

    /// Token program of one of the pool's mints.
    pub fn token_program(&self, mint: &Pubkey) -> Option<Pubkey> {
        let (mint_a, mint_b) = self.mints();
        let (program_a, program_b) = match self {
            Self::RaydiumClmm(snapshot) => snapshot.token_programs(),
            Self::OrcaWhirlpool(snapshot) => snapshot.token_programs(),
            Self::Manifest(snapshot) => (snapshot.base_token_program, snapshot.quote_token_program),
        };
        if *mint == mint_a {
            Some(program_a)
        } else if *mint == mint_b {
            Some(program_b)
        } else {
            None
        }
    }

    pub fn trades(&self, a: &Pubkey, b: &Pubkey) -> bool {
        let (mint_a, mint_b) = self.mints();
        (mint_a == *a && mint_b == *b) || (mint_a == *b && mint_b == *a)
    }

    /// Fee rate in hundredths of a basis point (Manifest charges none).
    fn fee_rate(&self) -> u64 {
        match self {
            Self::RaydiumClmm(snapshot) => u64::from(snapshot.trade_fee_rate()),
            Self::OrcaWhirlpool(snapshot) => u64::from(snapshot.fee_rate()),
            Self::Manifest(_) => 0,
        }
    }

    /// Quote one leg. `amount` is the input (exact-in) or output (exact-out).
    ///
    /// Venue math runs on live, untrusted pool state; a panic inside it
    /// (e.g. an arithmetic overflow in a venue's quote library) is reported
    /// as a quote error instead of unwinding into the caller.
    pub fn quote(
        &self,
        input_mint: Pubkey,
        output_mint: Pubkey,
        amount: u64,
        exact_in: bool,
        slippage_bps: u16,
    ) -> Result<LegQuote, VenueError> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.quote_unguarded(input_mint, output_mint, amount, exact_in, slippage_bps)
        }))
        .unwrap_or_else(|_| {
            metrics::counter!("venue_quote_panics_total", "venue" => self.kind().as_str())
                .increment(1);
            Err(VenueError::Venue {
                venue: self.kind().as_str(),
                pool: self.pool(),
                reason: "venue quote math panicked".to_owned(),
            })
        })
    }

    fn quote_unguarded(
        &self,
        input_mint: Pubkey,
        output_mint: Pubkey,
        amount: u64,
        exact_in: bool,
        slippage_bps: u16,
    ) -> Result<LegQuote, VenueError> {
        let venue = |reason: String| VenueError::Venue {
            venue: self.kind().as_str(),
            pool: self.pool(),
            reason,
        };
        let (in_amount, out_amount, threshold, max_in, cu_estimate, detail) = match self {
            Self::RaydiumClmm(snapshot) => {
                let quote = snapshot
                    .quote(input_mint, output_mint, amount, exact_in, slippage_bps)
                    .map_err(|error| venue(error.to_string()))?;
                let cu = RAYDIUM_SWAP_BASE_CU
                    + RAYDIUM_SWAP_CU_PER_TICK_ARRAY * quote.tick_arrays.len() as u32;
                (
                    quote.expected_input_raw,
                    quote.expected_output_raw,
                    quote.other_amount_threshold,
                    quote.max_input_raw(),
                    cu,
                    QuoteDetail::RaydiumClmm(quote),
                )
            }
            Self::OrcaWhirlpool(snapshot) => {
                let quote = snapshot
                    .quote(input_mint, output_mint, amount, exact_in, slippage_bps)
                    .map_err(|error| venue(error.to_string()))?;
                (
                    quote.expected_input_raw,
                    quote.expected_output_raw,
                    quote.other_amount_threshold,
                    quote.max_input_raw,
                    ORCA_SWAP_CU,
                    QuoteDetail::OrcaWhirlpool(quote),
                )
            }
            Self::Manifest(snapshot) => {
                let quote = snapshot
                    .quote(input_mint, output_mint, amount, exact_in, slippage_bps)
                    .map_err(|error| venue(error.to_string()))?;
                (
                    quote.expected_input_raw,
                    quote.expected_output_raw,
                    quote.other_amount_threshold,
                    quote.max_input_raw,
                    MANIFEST_SWAP_CU,
                    QuoteDetail::Manifest(quote),
                )
            }
        };
        Ok(LegQuote {
            input_mint,
            output_mint,
            exact_in,
            in_amount,
            out_amount,
            min_out: if exact_in { threshold } else { out_amount },
            max_in,
            fee_amount: (u128::from(in_amount) * u128::from(self.fee_rate()) / 1_000_000) as u64,
            cu_estimate,
            detail,
        })
    }

    /// Top-level instructions executing `quote` for `owner`: idempotent ATA
    /// creation for both mints, then the venue's own swap instruction.
    pub fn swap_instructions(
        &self,
        owner: Pubkey,
        quote: &LegQuote,
    ) -> Result<Vec<Instruction>, VenueError> {
        let venue = |reason: String| VenueError::Venue {
            venue: self.kind().as_str(),
            pool: self.pool(),
            reason,
        };
        match (self, &quote.detail) {
            (Self::RaydiumClmm(snapshot), QuoteDetail::RaydiumClmm(detail)) => snapshot
                .swap_instructions(owner, detail)
                .map_err(|error| venue(error.to_string())),
            (Self::OrcaWhirlpool(snapshot), QuoteDetail::OrcaWhirlpool(detail)) => snapshot
                .swap_instructions(owner, detail)
                .map_err(|error| venue(error.to_string())),
            (Self::Manifest(snapshot), QuoteDetail::Manifest(detail)) => snapshot
                .swap_instructions(owner, detail)
                .map_err(|error| venue(error.to_string())),
            _ => Err(venue("quote belongs to a different venue".to_owned())),
        }
    }

    /// The same leg as a Jupiter-fork route step (see [`jupiter`] for the
    /// placeholders this relies on).
    pub fn jupiter_leg(&self, owner: Pubkey, quote: &LegQuote) -> Result<jupiter::Leg, VenueError> {
        let venue = |reason: String| VenueError::Venue {
            venue: self.kind().as_str(),
            pool: self.pool(),
            reason,
        };
        let program = AccountMeta::new_readonly(self.program_id(), false);
        match (self, &quote.detail) {
            (Self::RaydiumClmm(snapshot), QuoteDetail::RaydiumClmm(detail)) => {
                let mut accounts = vec![program];
                accounts.extend(
                    snapshot
                        .swap_account_metas(owner, detail)
                        .map_err(|error| venue(error.to_string()))?,
                );
                Ok(jupiter::Leg {
                    swap: jupiter::SwapVariant::RaydiumClmmV2,
                    accounts,
                })
            }
            (Self::OrcaWhirlpool(snapshot), QuoteDetail::OrcaWhirlpool(_)) => {
                let (mint_a, _) = snapshot.mints();
                let mut accounts = vec![program];
                accounts.extend(snapshot.swap_account_metas(owner));
                Ok(jupiter::Leg {
                    swap: jupiter::SwapVariant::WhirlpoolSwapV2 {
                        a_to_b: quote.input_mint == mint_a,
                        supplemental_tick_arrays: 2,
                    },
                    accounts,
                })
            }
            (Self::Manifest(snapshot), QuoteDetail::Manifest(_)) => {
                let base_in = quote.input_mint == snapshot.book.base_mint;
                let pda = |seed: &[u8]| {
                    Pubkey::find_program_address(
                        &[seed, quote.output_mint.as_ref()],
                        &snapshot.program_id,
                    )
                    .0
                };
                let mut accounts = vec![program];
                accounts.extend(snapshot.swap_account_metas(owner));
                // Manifest's own Jupiter adapter appends the output mint's
                // global account and global vault.
                accounts.push(AccountMeta::new(pda(b"global"), false));
                accounts.push(AccountMeta::new(pda(b"global-vault"), false));
                Ok(jupiter::Leg {
                    swap: jupiter::SwapVariant::Manifest {
                        side: if base_in {
                            jupiter::ManifestSide::Ask
                        } else {
                            jupiter::ManifestSide::Bid
                        },
                    },
                    accounts,
                })
            }
            _ => Err(venue("quote belongs to a different venue".to_owned())),
        }
    }
}

/// Every pool or market account owned by `program_id`, found by account
/// discriminator. Structural and functional validation happens on load.
pub async fn discover(
    chain: &ChainClient,
    kind: VenueKind,
    program_id: Pubkey,
) -> Result<Vec<Pubkey>, VenueError> {
    let discriminator = match kind {
        VenueKind::RaydiumClmm => anchor_discriminator("PoolState").to_vec(),
        VenueKind::OrcaWhirlpool => anchor_discriminator("Whirlpool").to_vec(),
        VenueKind::Manifest => manifest_dex_client::book::MARKET_FIXED_DISCRIMINANT
            .to_le_bytes()
            .to_vec(),
    };
    let config = RpcProgramAccountsConfig {
        filters: Some(vec![RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
            0,
            discriminator,
        ))]),
        account_config: RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::Base64),
            data_slice: Some(UiDataSliceConfig {
                offset: 0,
                length: 0,
            }),
            ..RpcAccountInfoConfig::default()
        },
        ..RpcProgramAccountsConfig::default()
    };
    let mut pools: Vec<Pubkey> = chain
        .rpc()
        .get_program_ui_accounts_with_config(&program_id, config)
        .await
        .map_err(|error| VenueError::Rpc(error.to_string()))?
        .into_iter()
        .map(|(address, _)| address)
        .collect();
    pools.sort();
    Ok(pools)
}

fn anchor_discriminator(account: &str) -> [u8; 8] {
    let digest = Sha256::digest(format!("account:{account}").as_bytes());
    let mut out = [0_u8; 8];
    out.copy_from_slice(&digest[..8]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn venue_names_round_trip() {
        for kind in VenueKind::ALL {
            assert_eq!(kind.as_str().parse::<VenueKind>().expect("parses"), kind);
        }
        assert!("raydium-cpmm".parse::<VenueKind>().is_err());
    }

    #[test]
    fn discriminators_match_anchor() {
        assert_eq!(
            anchor_discriminator("Whirlpool"),
            [63, 149, 209, 12, 225, 128, 99, 9]
        );
        // raydium-clmm PoolState, as emitted in its IDL.
        assert_eq!(
            anchor_discriminator("PoolState"),
            [247, 237, 227, 245, 215, 195, 222, 70]
        );
    }
}
