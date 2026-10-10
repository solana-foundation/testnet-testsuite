//! Instruction + transaction assembly (docs/router-architecture.md §5).
//!
//! `direct` execution emits each leg's own top-level venue instructions,
//! segments in order (per-leg amounts and minimums come from the engine).
//! `jupiter` execution emits one `route_v2` of the configured Jupiter fork with
//! every leg as a route step. Both prepend idempotent ATA creation for every
//! mint the route touches. ALTs are not wired yet, so a hard size guard
//! rejects routes that do not fit one raw v0 transaction. Transactions are
//! UNSIGNED; the caller signs as payer.

use std::collections::HashSet;

use anyhow::Context;
use base64::Engine;
use mtm_venues::{PoolSnapshot, jupiter};
use router_client::Execution;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_instruction::Instruction;
use solana_message::VersionedMessage;
use solana_message::v0::Message as MessageV0;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;

use crate::AppState;
use crate::engine::Segment;

pub const ATA_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
/// v0 transaction wire limit
pub const MAX_TX_BYTES: usize = 1232;

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
    let mut cu_estimate: u32 = 0;
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
            cu_estimate = cu_estimate.saturating_add(leg.amounts.cu_estimate);
        }
    }
    Ok(Assembled {
        instructions,
        cu_estimate,
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
    let mut cu_estimate = state.jupiter_overhead_cu;
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
            cu_estimate = cu_estimate.saturating_add(leg.amounts.cu_estimate);
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
        cu_estimate,
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

#[derive(Debug, thiserror::Error)]
#[error(
    "route needs {bytes} bytes (limit {MAX_TX_BYTES}): too many legs without address lookup tables"
)]
pub struct Oversized {
    pub bytes: usize,
}

pub fn is_oversized(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Oversized>().is_some()
}

/// Full unsigned v0 transaction: compute budget + route instructions.
pub async fn route_tx(
    state: &AppState,
    user: &Pubkey,
    assembled: Assembled,
) -> anyhow::Result<String> {
    let mut instructions = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(
            assembled
                .cu_estimate
                .saturating_add(state.cfg.cu_margin)
                .min(1_400_000),
        ),
        ComputeBudgetInstruction::set_compute_unit_price(state.cfg.cu_price_micro_lamports),
    ];
    instructions.extend(assembled.instructions);

    let blockhash = state
        .chain
        .rpc()
        .get_latest_blockhash()
        .await
        .context("fetching blockhash")?;
    let message = MessageV0::try_compile(user, &instructions, &[], blockhash)
        .context("compiling v0 message")?;
    let num_signatures = message.header.num_required_signatures as usize;
    let tx = VersionedTransaction {
        signatures: vec![Signature::default(); num_signatures],
        message: VersionedMessage::V0(message),
    };
    let bytes = bincode::serialize(&tx).context("serializing transaction")?;
    if bytes.len() > MAX_TX_BYTES {
        return Err(Oversized { bytes: bytes.len() }.into());
    }
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}
