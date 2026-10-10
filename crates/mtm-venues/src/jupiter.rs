//! `route_v2` instruction builder for a self-deployed Jupiter aggregator fork.
//!
//! Wire layout (instruction discriminator, account order, `RoutePlanStepV2`,
//! `Swap` variant indices, `AccountsType`) is taken from the Jupiter v6 IDL
//! published on mainnet (`JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4`, IDL
//! account fetched 2026-10-10). Per-leg accounts follow the convention of
//! `jupiter-amm-interface` adapters (e.g. Manifest's `client/rust/jup`): the
//! AMM program ID first, then that AMM's own swap-instruction accounts.
//!
//! PLACEHOLDERS — confirm each against the fork before relying on this path:
//! - `SWAP_VARIANT_*`: enum indices of the deployed fork's `Swap` type. A fork
//!   that adds or removes variants shifts them; compare with its IDL.
//! - The fork must accept our testnet AMM program IDs. Mainnet Jupiter pins
//!   the canonical program ID per `Swap` variant, so a fork that was not
//!   rebuilt (or re-pointed) with the IDs of our Raydium, Orca, and Manifest
//!   forks will reject every leg.
//! - Exact per-variant account counts: Raydium CLMM legs pass the
//!   `SwapSingleV2` accounts plus the bitmap extension and the quote's tick
//!   arrays; Jupiter's closed-source adapter may expect a fixed tick-array
//!   count. Orca and Manifest legs mirror their public adapters.

use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;

/// Canonical mainnet Jupiter v6 program, for reference only.
pub const JUPITER_V6_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4");
/// `route_v2` Anchor discriminator.
pub const ROUTE_V2_DISCRIMINATOR: [u8; 8] = [187, 100, 250, 204, 49, 196, 175, 20];
/// `Swap::RaydiumClmmV2` (no fields).
pub const SWAP_VARIANT_RAYDIUM_CLMM_V2: u8 = 40;
/// `Swap::WhirlpoolSwapV2 { a_to_b, remaining_accounts_info }`.
pub const SWAP_VARIANT_WHIRLPOOL_SWAP_V2: u8 = 47;
/// `Swap::Manifest { side }`.
pub const SWAP_VARIANT_MANIFEST: u8 = 116;
/// `AccountsType::SupplementalTickArrays` in Jupiter's (and Orca's) enum.
pub const ACCOUNTS_TYPE_SUPPLEMENTAL_TICK_ARRAYS: u8 = 6;
/// Basis-point denominator of `RoutePlanStepV2::bps`.
pub const BPS_DENOMINATOR: u16 = 10_000;

/// Taker side of a Manifest leg in Jupiter's encoding: selling base is `Ask`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManifestSide {
    Bid,
    Ask,
}

/// The fork's `Swap` enum, restricted to the venues this suite deploys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SwapVariant {
    RaydiumClmmV2,
    WhirlpoolSwapV2 {
        a_to_b: bool,
        /// Number of supplemental tick arrays appended after the fixed accounts.
        supplemental_tick_arrays: u8,
    },
    Manifest {
        side: ManifestSide,
    },
}

impl SwapVariant {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::RaydiumClmmV2 => out.push(SWAP_VARIANT_RAYDIUM_CLMM_V2),
            Self::WhirlpoolSwapV2 {
                a_to_b,
                supplemental_tick_arrays,
            } => {
                out.push(SWAP_VARIANT_WHIRLPOOL_SWAP_V2);
                out.push(u8::from(*a_to_b));
                if *supplemental_tick_arrays == 0 {
                    out.push(0); // Option::None
                } else {
                    out.push(1); // Option::Some(RemainingAccountsInfo { slices })
                    out.extend_from_slice(&1_u32.to_le_bytes());
                    out.push(ACCOUNTS_TYPE_SUPPLEMENTAL_TICK_ARRAYS);
                    out.push(*supplemental_tick_arrays);
                }
            }
            Self::Manifest { side } => {
                out.push(SWAP_VARIANT_MANIFEST);
                out.push(match side {
                    ManifestSide::Bid => 0,
                    ManifestSide::Ask => 1,
                });
            }
        }
    }
}

/// One AMM hop as Jupiter consumes it.
#[derive(Clone, Debug)]
pub struct Leg {
    pub swap: SwapVariant,
    /// AMM program ID followed by the AMM's swap accounts.
    pub accounts: Vec<AccountMeta>,
}

/// One `RoutePlanStepV2`: route `bps` of the balance at `input_index` through
/// `leg` into `output_index`. Index 0 is the source mint; each later index is
/// the next mint along the route.
#[derive(Clone, Debug)]
pub struct Step {
    pub leg: Leg,
    pub bps: u16,
    pub input_index: u8,
    pub output_index: u8,
}

/// Route-level accounts and amounts.
#[derive(Clone, Copy, Debug)]
pub struct RouteParams {
    pub program_id: Pubkey,
    pub user: Pubkey,
    pub source_token_account: Pubkey,
    pub destination_token_account: Pubkey,
    pub source_mint: Pubkey,
    pub destination_mint: Pubkey,
    pub source_token_program: Pubkey,
    pub destination_token_program: Pubkey,
    pub in_amount: u64,
    pub quoted_out_amount: u64,
    pub slippage_bps: u16,
}

pub fn event_authority(program_id: Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], &program_id).0
}

/// Build `route_v2`. Steps sharing an `input_index` must sum to 10,000 bps.
pub fn route_v2_instruction(params: RouteParams, steps: &[Step]) -> Result<Instruction, String> {
    if steps.is_empty() {
        return Err("a route needs at least one step".to_owned());
    }
    let mut totals = std::collections::BTreeMap::<u8, u32>::new();
    for step in steps {
        if step.output_index <= step.input_index {
            return Err("route steps must move to a later token index".to_owned());
        }
        *totals.entry(step.input_index).or_default() += u32::from(step.bps);
    }
    if totals
        .values()
        .any(|total| *total != u32::from(BPS_DENOMINATOR))
    {
        return Err("steps leaving one token must split exactly 10,000 bps".to_owned());
    }
    let mut data = ROUTE_V2_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&params.in_amount.to_le_bytes());
    data.extend_from_slice(&params.quoted_out_amount.to_le_bytes());
    data.extend_from_slice(&params.slippage_bps.to_le_bytes());
    data.extend_from_slice(&0_u16.to_le_bytes()); // platform_fee_bps
    data.extend_from_slice(&0_u16.to_le_bytes()); // positive_slippage_bps
    data.extend_from_slice(&(steps.len() as u32).to_le_bytes());
    for step in steps {
        step.leg.swap.encode(&mut data);
        data.extend_from_slice(&step.bps.to_le_bytes());
        data.push(step.input_index);
        data.push(step.output_index);
    }
    let mut accounts = vec![
        AccountMeta::new_readonly(params.user, true),
        AccountMeta::new(params.source_token_account, false),
        AccountMeta::new(params.destination_token_account, false),
        AccountMeta::new_readonly(params.source_mint, false),
        AccountMeta::new_readonly(params.destination_mint, false),
        AccountMeta::new_readonly(params.source_token_program, false),
        AccountMeta::new_readonly(params.destination_token_program, false),
        // Optional `destination_token_account`: Anchor reads the program ID as None.
        AccountMeta::new_readonly(params.program_id, false),
        AccountMeta::new_readonly(event_authority(params.program_id), false),
        AccountMeta::new_readonly(params.program_id, false),
    ];
    for step in steps {
        accounts.extend(step.leg.accounts.iter().cloned());
    }
    Ok(Instruction {
        program_id: params.program_id,
        accounts,
        data,
    })
}

/// Split `amounts` into bps that sum to exactly 10,000 (largest remainder).
pub fn split_bps(amounts: &[u64]) -> Result<Vec<u16>, String> {
    let total: u128 = amounts.iter().map(|amount| u128::from(*amount)).sum();
    if total == 0 {
        return Err("cannot split a zero amount".to_owned());
    }
    let denominator = u128::from(BPS_DENOMINATOR);
    let mut bps: Vec<u16> = amounts
        .iter()
        .map(|amount| (u128::from(*amount) * denominator / total) as u16)
        .collect();
    let mut remainders: Vec<(usize, u128)> = amounts
        .iter()
        .enumerate()
        .map(|(index, amount)| (index, u128::from(*amount) * denominator % total))
        .collect();
    remainders.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let assigned: u16 = bps.iter().sum();
    for (index, _) in remainders
        .iter()
        .take(usize::from(BPS_DENOMINATOR - assigned))
    {
        bps[*index] += 1;
    }
    Ok(bps)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leg(swap: SwapVariant, accounts: usize) -> Leg {
        Leg {
            swap,
            accounts: (0..accounts)
                .map(|i| AccountMeta::new(Pubkey::new_from_array([i as u8 + 50; 32]), false))
                .collect(),
        }
    }

    fn params() -> RouteParams {
        RouteParams {
            program_id: Pubkey::new_from_array([7; 32]),
            user: Pubkey::new_from_array([1; 32]),
            source_token_account: Pubkey::new_from_array([2; 32]),
            destination_token_account: Pubkey::new_from_array([3; 32]),
            source_mint: Pubkey::new_from_array([4; 32]),
            destination_mint: Pubkey::new_from_array([5; 32]),
            source_token_program: Pubkey::new_from_array([6; 32]),
            destination_token_program: Pubkey::new_from_array([6; 32]),
            in_amount: 1_000,
            quoted_out_amount: 990,
            slippage_bps: 50,
        }
    }

    #[test]
    fn encodes_two_hop_route() {
        let ix = route_v2_instruction(
            params(),
            &[
                Step {
                    leg: leg(
                        SwapVariant::WhirlpoolSwapV2 {
                            a_to_b: true,
                            supplemental_tick_arrays: 2,
                        },
                        3,
                    ),
                    bps: 10_000,
                    input_index: 0,
                    output_index: 1,
                },
                Step {
                    leg: leg(
                        SwapVariant::Manifest {
                            side: ManifestSide::Ask,
                        },
                        2,
                    ),
                    bps: 10_000,
                    input_index: 1,
                    output_index: 2,
                },
            ],
        )
        .expect("builds");
        let mut expected = ROUTE_V2_DISCRIMINATOR.to_vec();
        expected.extend_from_slice(&1_000_u64.to_le_bytes());
        expected.extend_from_slice(&990_u64.to_le_bytes());
        expected.extend_from_slice(&50_u16.to_le_bytes());
        expected.extend_from_slice(&[0, 0, 0, 0]);
        expected.extend_from_slice(&2_u32.to_le_bytes());
        expected.extend_from_slice(&[47, 1, 1, 1, 0, 0, 0, 6, 2]);
        expected.extend_from_slice(&10_000_u16.to_le_bytes());
        expected.extend_from_slice(&[0, 1]);
        expected.extend_from_slice(&[116, 1]);
        expected.extend_from_slice(&10_000_u16.to_le_bytes());
        expected.extend_from_slice(&[1, 2]);
        assert_eq!(ix.data, expected);
        assert_eq!(ix.accounts.len(), 10 + 3 + 2);
        assert!(ix.accounts[0].is_signer);
        assert_eq!(ix.accounts[8].pubkey, event_authority(params().program_id));
    }

    #[test]
    fn rejects_unbalanced_splits() {
        let step = |bps| Step {
            leg: leg(SwapVariant::RaydiumClmmV2, 1),
            bps,
            input_index: 0,
            output_index: 1,
        };
        assert!(route_v2_instruction(params(), &[step(6_000), step(3_000)]).is_err());
        assert!(route_v2_instruction(params(), &[step(6_000), step(4_000)]).is_ok());
    }

    #[test]
    fn bps_split_sums_exactly() {
        let bps = split_bps(&[1, 1, 1]).expect("splits");
        assert_eq!(bps.iter().sum::<u16>(), 10_000);
        assert_eq!(bps, vec![3_334, 3_333, 3_333]);
        assert_eq!(split_bps(&[750, 250]).expect("splits"), vec![7_500, 2_500]);
    }
}
