//! Instruction + transaction assembly (docs/router-architecture.md §5).
//!
//! `direct` execution emits each leg's own top-level venue instructions,
//! segments in order (per-leg amounts and minimums come from the engine).
//! `jupiter` execution emits one `route_v2` of the configured Jupiter fork with
//! every leg as a route step. Both prepend idempotent ATA creation for every
//! mint the route touches. Transactions are v1 (SIMD-0385): up to 4,096
//! bytes, 64 addresses, and 64 instructions, with the compute budget in the
//! message config instead of ComputeBudget instructions. There are no
//! lookup tables in v1, so every touched account counts against the 64.
//! Transactions are UNSIGNED; the caller signs as payer.

use std::collections::HashSet;

use anyhow::Context;
use base64::Engine;
use mtm_venues::{PoolSnapshot, jupiter};
use router_client::Execution;
use solana_hash::Hash;
use solana_instruction::Instruction;
use solana_message::VersionedMessage;
use solana_message::v1::{self, TransactionConfig};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;

use crate::AppState;
use crate::engine::{Segment, route_cu_estimate};

pub const ATA_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
/// Maximum account data a route may load (the runtime's 64 MiB ceiling).
/// V1 treats an unset limit as zero, so it is always sent explicitly.
pub const DEFAULT_LOADED_ACCOUNTS_DATA_SIZE_LIMIT: u32 = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct Assembled {
    pub instructions: Vec<Instruction>,
    /// CU the route instructions are expected to consume (no margin)
    pub cu_estimate: u32,
}

/// The route's instructions, without compute budget.
pub fn route_instructions(
    state: &AppState,
    user: &Pubkey,
    execution: Execution,
    slippage_bps: u16,
    segments: &[Segment<PoolSnapshot>],
) -> anyhow::Result<Assembled> {
    match execution {
        Execution::Direct => direct_instructions(user, segments),
        Execution::Jupiter => jupiter_instructions(state, user, slippage_bps, segments),
    }
}

fn direct_instructions(
    user: &Pubkey,
    segments: &[Segment<PoolSnapshot>],
) -> anyhow::Result<Assembled> {
    let mut seen_atas = HashSet::new();
    let mut instructions = Vec::new();
    for segment in segments {
        for leg in &segment.legs {
            for ix in leg
                .source
                .swap_instructions(*user, &leg.quote)
                .context("building venue swap")?
            {
                // Legs of one route share ATAs; create each one once.
                if ix.program_id == ATA_PROGRAM_ID {
                    let ata = ix.accounts.get(1).map(|meta| meta.pubkey);
                    if !seen_atas.insert(ata) {
                        continue;
                    }
                }
                instructions.push(ix);
            }
        }
    }
    Ok(Assembled {
        instructions,
        cu_estimate: route_cu_estimate(segments, Execution::Direct, 0),
    })
}

fn jupiter_instructions(
    state: &AppState,
    user: &Pubkey,
    slippage_bps: u16,
    segments: &[Segment<PoolSnapshot>],
) -> anyhow::Result<Assembled> {
    let program_id = state
        .jupiter_program
        .context("execution = jupiter requires [router.jupiter] program_id")?;
    let first = segments.first().context("empty route")?;
    let last = segments.last().context("empty route")?;
    let first_leg = first.legs.first().context("empty segment")?;
    let last_leg = last.legs.first().context("empty segment")?;
    let source_program = first_leg
        .source
        .token_program(&first.input_mint)
        .context("source token program")?;
    let destination_program = last_leg
        .source
        .token_program(&last.output_mint)
        .context("destination token program")?;

    // Every mint after the source needs an ATA: intermediate hops land in
    // the user's own accounts in non-shared-accounts routes.
    let mut instructions = Vec::new();
    for segment in segments {
        let leg = segment.legs.first().context("empty segment")?;
        let token_program = leg
            .source
            .token_program(&segment.output_mint)
            .context("token program")?;
        instructions.push(create_ata_idempotent(
            user,
            &segment.output_mint,
            &token_program,
        ));
    }

    let mut steps = Vec::new();
    for (index, segment) in segments.iter().enumerate() {
        let amounts: Vec<u64> = segment
            .legs
            .iter()
            .map(|leg| leg.amounts.in_amount)
            .collect();
        let bps = jupiter::split_bps(&amounts).map_err(anyhow::Error::msg)?;
        for (leg, bps) in segment.legs.iter().zip(bps) {
            steps.push(jupiter::Step {
                leg: leg
                    .source
                    .jupiter_leg(*user, &leg.quote)
                    .context("building Jupiter leg")?,
                bps,
                input_index: u8::try_from(index)?,
                output_index: u8::try_from(index + 1)?,
            });
        }
    }
    let in_amount = first.legs.iter().map(|leg| leg.amounts.in_amount).sum();
    let quoted_out_amount: u64 = last.legs.iter().map(|leg| leg.amounts.out_amount).sum();
    instructions.push(
        jupiter::route_v2_instruction(
            jupiter::RouteParams {
                program_id,
                user: *user,
                source_token_account: derive_ata(user, &source_program, &first.input_mint),
                destination_token_account: derive_ata(
                    user,
                    &destination_program,
                    &last.output_mint,
                ),
                source_mint: first.input_mint,
                destination_mint: last.output_mint,
                source_token_program: source_program,
                destination_token_program: destination_program,
                in_amount,
                quoted_out_amount,
                slippage_bps,
            },
            &steps,
        )
        .map_err(anyhow::Error::msg)?,
    );
    Ok(Assembled {
        instructions,
        cu_estimate: route_cu_estimate(segments, Execution::Jupiter, state.jupiter_overhead_cu),
    })
}

pub fn derive_ata(owner: &Pubkey, token_program: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ATA_PROGRAM_ID,
    )
    .0
}

/// spl-associated-token-account `CreateIdempotent` (discriminant 1).
fn create_ata_idempotent(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Instruction {
    use solana_instruction::AccountMeta;
    Instruction {
        program_id: ATA_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*owner, true),
            AccountMeta::new(derive_ata(owner, token_program, mint), false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(Pubkey::default(), false),
            AccountMeta::new_readonly(*token_program, false),
        ],
        data: vec![1],
    }
}

/// Hard per-transaction limits of v1 messages (SIMD-0385). V1 has no
/// address lookup tables, so every account the route touches counts
/// against `MAX_ADDRESSES`.
pub const MAX_TX_BYTES: usize = v1::MAX_TRANSACTION_SIZE;
pub const MAX_ACCOUNTS: usize = v1::MAX_ADDRESSES as usize;
pub const MAX_INSTRUCTIONS: usize = v1::MAX_INSTRUCTIONS as usize;
/// Runtime cap on a transaction's compute-unit limit.
pub const MAX_COMPUTE_UNIT_LIMIT: u32 = 1_400_000;

/// A route that does not fit one v1 transaction. The API narrows the plan
/// and retries on any of these.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum RouteTooLarge {
    #[error("route needs {0} bytes (v1 limit {MAX_TX_BYTES})")]
    Bytes(usize),
    #[error("route touches {0} accounts (v1 limit {MAX_ACCOUNTS})")]
    Accounts(usize),
    #[error("route needs {0} instructions (v1 limit {MAX_INSTRUCTIONS})")]
    Instructions(usize),
}

pub fn is_oversized(error: &anyhow::Error) -> bool {
    error.downcast_ref::<RouteTooLarge>().is_some()
}

/// Compute budget carried in the v1 message config (v1 replaces the
/// ComputeBudget instructions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub compute_unit_limit: u32,
    /// Total priority fee in lamports (v1 prices the transaction, not each CU).
    pub priority_fee_lamports: u64,
    pub loaded_accounts_data_size_limit: u32,
}

impl Budget {
    pub fn for_route(state: &AppState, assembled: &Assembled) -> Self {
        let compute_unit_limit = assembled
            .cu_estimate
            .saturating_add(state.cfg.cu_margin)
            .min(MAX_COMPUTE_UNIT_LIMIT);
        Self {
            compute_unit_limit,
            priority_fee_lamports: priority_fee_lamports(
                compute_unit_limit,
                state.cfg.cu_price_micro_lamports,
            ),
            loaded_accounts_data_size_limit: state.cfg.loaded_accounts_data_size_limit,
        }
    }
}

/// `cu_limit × micro-lamports per CU`, rounded up to whole lamports.
pub fn priority_fee_lamports(compute_unit_limit: u32, micro_lamports_per_cu: u64) -> u64 {
    (u128::from(compute_unit_limit) * u128::from(micro_lamports_per_cu)).div_ceil(1_000_000) as u64
}

/// Compile `instructions` into an unsigned v1 transaction and check it
/// against every v1 limit.
pub fn compile_v1(
    payer: &Pubkey,
    instructions: &[Instruction],
    budget: Budget,
    blockhash: Hash,
) -> anyhow::Result<VersionedTransaction> {
    if instructions.len() > MAX_INSTRUCTIONS {
        return Err(RouteTooLarge::Instructions(instructions.len()).into());
    }
    let config = TransactionConfig::empty()
        .with_compute_unit_limit(budget.compute_unit_limit)
        .with_priority_fee(budget.priority_fee_lamports)
        .with_loaded_accounts_data_size_limit(budget.loaded_accounts_data_size_limit);
    let message = v1::Message::try_compile_with_config(payer, instructions, blockhash, config)
        .context("compiling v1 message")?;
    if message.account_keys.len() > MAX_ACCOUNTS {
        return Err(RouteTooLarge::Accounts(message.account_keys.len()).into());
    }
    message.validate().context("invalid v1 message")?;
    let tx = VersionedTransaction {
        signatures: vec![Signature::default(); usize::from(message.header.num_required_signatures)],
        message: VersionedMessage::V1(message),
    };
    let bytes = wincode::serialize(&tx)
        .context("serializing transaction")?
        .len();
    if bytes > MAX_TX_BYTES {
        return Err(RouteTooLarge::Bytes(bytes).into());
    }
    Ok(tx)
}

/// Whether the route fits one v1 transaction (no RPC: size and account
/// counts do not depend on the blockhash).
pub fn check_fits(state: &AppState, user: &Pubkey, assembled: &Assembled) -> anyhow::Result<()> {
    compile_v1(
        user,
        &assembled.instructions,
        Budget::for_route(state, assembled),
        Hash::default(),
    )
    .map(|_| ())
}

/// Full unsigned v1 transaction (wincode wire format, base64): the route's
/// instructions with the compute budget in the message config.
pub async fn route_tx(
    state: &AppState,
    user: &Pubkey,
    assembled: &Assembled,
) -> anyhow::Result<String> {
    let blockhash = state
        .chain
        .rpc()
        .get_latest_blockhash()
        .await
        .context("fetching blockhash")?;
    let tx = compile_v1(
        user,
        &assembled.instructions,
        Budget::for_route(state, assembled),
        blockhash,
    )?;
    let bytes = wincode::serialize(&tx).context("serializing transaction")?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_instruction::AccountMeta;

    fn budget() -> Budget {
        Budget {
            compute_unit_limit: 200_000,
            priority_fee_lamports: 200,
            loaded_accounts_data_size_limit: DEFAULT_LOADED_ACCOUNTS_DATA_SIZE_LIMIT,
        }
    }

    fn ix(accounts: u8, data: usize) -> Instruction {
        Instruction {
            program_id: Pubkey::new_from_array([200; 32]),
            accounts: (0..accounts)
                .map(|i| AccountMeta::new(Pubkey::new_from_array([i; 32]), false))
                .collect(),
            data: vec![0; data],
        }
    }

    fn too_large(result: anyhow::Result<VersionedTransaction>) -> Option<RouteTooLarge> {
        result.err()?.downcast_ref::<RouteTooLarge>().cloned()
    }

    #[test]
    fn priority_fee_rounds_up_to_lamports() {
        assert_eq!(priority_fee_lamports(200_000, 1_000), 200);
        assert_eq!(priority_fee_lamports(1, 1), 1);
        assert_eq!(priority_fee_lamports(200_000, 0), 0);
    }

    #[test]
    fn v1_limits_are_enforced() {
        let payer = Pubkey::new_from_array([250; 32]);
        let tx = compile_v1(&payer, &[ix(40, 100)], budget(), Hash::default()).expect("fits");
        assert!(matches!(tx.message, VersionedMessage::V1(_)));
        // 63 unique accounts + payer + program = 65 addresses
        let too_wide = compile_v1(&payer, &[ix(63, 10)], budget(), Hash::default());
        assert_eq!(too_large(too_wide), Some(RouteTooLarge::Accounts(65)));
        let too_big = compile_v1(
            &payer,
            &[ix(4, 3_000), ix(4, 1_000)],
            budget(),
            Hash::default(),
        );
        assert!(matches!(too_large(too_big), Some(RouteTooLarge::Bytes(_))));
        let too_many: Vec<Instruction> = (0..65).map(|_| ix(1, 0)).collect();
        assert_eq!(
            too_large(compile_v1(&payer, &too_many, budget(), Hash::default())),
            Some(RouteTooLarge::Instructions(65))
        );
    }

    #[test]
    fn v1_round_trips_through_wincode() {
        let payer = Pubkey::new_from_array([250; 32]);
        let tx = compile_v1(&payer, &[ix(3, 8)], budget(), Hash::default()).expect("fits");
        let bytes = wincode::serialize(&tx).expect("serializes");
        let back: VersionedTransaction = wincode::deserialize(&bytes).expect("deserializes");
        assert_eq!(back, tx);
    }
}
