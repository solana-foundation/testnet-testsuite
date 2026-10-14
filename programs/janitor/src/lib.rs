//! The janitor: a teardown-only program deployed over a scenario's DEX
//! programs (an upgrade in place). Running at their program IDs, it owns
//! every account they created, and `Close` returns those accounts' rent to
//! an authority compiled into the binary (see `build.rs`).
//!
//! `Close` takes accounts the program owns; `CloseTokenAccounts` takes token
//! accounts (vaults) held by one of its PDAs, whose seeds the client passes,
//! so one instruction covers every protocol's vault layout.
//!
//! Every account is checked before it is touched, and any failure fails the
//! whole instruction (nothing half-closes): the signer is an authority, a
//! `Close` target is writable, owned by this program ID and not executable
//! (a target listed twice fails the second check: closing zeroes the
//! owner), and a token account is an initialized account of the given
//! token program, held by the PDA the seeds derive, for the mint passed
//! with it.
#![cfg_attr(target_os = "solana", no_std)]

use janitor_interface::{
    CLOSE, CLOSE_TOKEN_ACCOUNTS, MAX_SEEDS, TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID, error,
    parse_seeds,
};
use pinocchio::cpi::{Seed, Signer, invoke_signed};
use pinocchio::error::ProgramError;
use pinocchio::instruction::{InstructionAccount, InstructionView};
use pinocchio::{AccountView, Address, ProgramResult};

/// SPL token account layout (identical in Token-2022's base account).
const TOKEN_ACCOUNT_LEN: usize = 165;
const MINT_OFFSET: usize = 0;
const OWNER_OFFSET: usize = 32;
const AMOUNT_OFFSET: usize = 64;
const STATE_OFFSET: usize = 108;
const INITIALIZED: u8 = 1;
/// Token instruction tags, the same in both token programs.
const BURN: u8 = 8;
const CLOSE_ACCOUNT: u8 = 9;

include!(concat!(env!("OUT_DIR"), "/authorities.rs"));

#[cfg(target_os = "solana")]
#[allow(unsafe_code)] // the entrypoint is an `extern "C"` symbol by definition
mod entrypoint {
    pinocchio::program_entrypoint!(crate::process_instruction);
    pinocchio::no_allocator!();
    pinocchio::nostd_panic_handler!();
}

pub fn process_instruction(
    program_id: &Address,
    accounts: &mut [AccountView],
    data: &[u8],
) -> ProgramResult {
    match data {
        [CLOSE] => close(program_id, accounts, AUTHORITIES),
        [CLOSE_TOKEN_ACCOUNTS, seeds @ ..] => {
            close_token_accounts(program_id, accounts, seeds, AUTHORITIES)
        }
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

/// Is `key` one of `authorities`?
pub fn is_authority(key: &Address, authorities: &[[u8; 32]]) -> bool {
    authorities
        .iter()
        .any(|authority| authority == key.as_array())
}

/// The authority must sign, be writable (it receives the rent) and be
/// one of `authorities`.
fn check_authority(authority: &AccountView, authorities: &[[u8; 32]]) -> ProgramResult {
    if !authority.is_signer() {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if !authority.is_writable() {
        return Err(ProgramError::InvalidAccountData);
    }
    if !is_authority(authority.address(), authorities) {
        return Err(ProgramError::Custom(error::NOT_AN_AUTHORITY));
    }
    Ok(())
}

fn close(
    program_id: &Address,
    accounts: &mut [AccountView],
    authorities: &[[u8; 32]],
) -> ProgramResult {
    let [authority, targets @ ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    check_authority(authority, authorities)?;
    if targets.is_empty() {
        return Err(ProgramError::Custom(error::NO_TARGETS));
    }
    for target in targets.iter_mut() {
        if !target.owned_by(program_id) {
            return Err(ProgramError::Custom(error::NOT_OWNED));
        }
        // Unreachable under today's runtime (programs never own executable
        // accounts); kept so a program account can never be emptied here.
        if target.executable() {
            return Err(ProgramError::Custom(error::EXECUTABLE));
        }
        if !target.is_writable() {
            return Err(ProgramError::InvalidAccountData);
        }
        let lamports = target.lamports();
        let credited = authority
            .lamports()
            .checked_add(lamports)
            .ok_or(ProgramError::ArithmeticOverflow)?;
        authority.set_lamports(credited);
        target.set_lamports(0);
        // Zeroes owner (→ system program), lamports and data length; the
        // runtime wipes the data and drops the account after the instruction.
        target.close()?;
    }
    Ok(())
}

/// The address `seeds` (bump included) derive under `program_id`.
#[cfg(target_os = "solana")]
fn derive(seeds: &[&[u8]], program_id: &Address) -> Result<Address, ProgramError> {
    Address::create_program_address(seeds, program_id)
        .map_err(|_| ProgramError::Custom(error::INVALID_SEEDS))
}

/// Host builds (clippy, IDE) never run the program.
#[cfg(not(target_os = "solana"))]
fn derive(_: &[&[u8]], _: &Address) -> Result<Address, ProgramError> {
    Err(ProgramError::Custom(error::INVALID_SEEDS))
}

/// Is `account` an initialized `token_program` account of `mint` held by
/// `owner`? Returns its balance.
fn pda_token_account(
    account: &AccountView,
    mint: &AccountView,
    owner: &Address,
    token_program: &Address,
) -> Result<u64, ProgramError> {
    let refuse = || ProgramError::Custom(error::NOT_A_PDA_TOKEN_ACCOUNT);
    if !account.is_writable() || !mint.is_writable() {
        return Err(ProgramError::InvalidAccountData);
    }
    if !account.owned_by(token_program) || !mint.owned_by(token_program) {
        return Err(refuse());
    }
    let data = account.try_borrow()?;
    if data.len() < TOKEN_ACCOUNT_LEN
        || data[MINT_OFFSET..MINT_OFFSET + 32] != *mint.address().as_array()
        || data[OWNER_OFFSET..OWNER_OFFSET + 32] != *owner.as_array()
        || data[STATE_OFFSET] != INITIALIZED
    {
        return Err(refuse());
    }
    let amount: [u8; 8] = data[AMOUNT_OFFSET..AMOUNT_OFFSET + 8]
        .try_into()
        .map_err(|_| refuse())?;
    Ok(u64::from_le_bytes(amount))
}

fn close_token_accounts(
    program_id: &Address,
    accounts: &mut [AccountView],
    seed_data: &[u8],
    authorities: &[[u8; 32]],
) -> ProgramResult {
    let [authority, token_program, pda, pairs @ ..] = &*accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    check_authority(authority, authorities)?;
    let program = token_program.address().as_array();
    if *program != TOKEN_PROGRAM_ID && *program != TOKEN_2022_PROGRAM_ID {
        return Err(ProgramError::Custom(error::NOT_A_TOKEN_PROGRAM));
    }
    if pairs.is_empty() || pairs.len() % 2 != 0 {
        return Err(ProgramError::Custom(error::NO_TARGETS));
    }
    let mut parsed: [&[u8]; MAX_SEEDS] = [&[]; MAX_SEEDS];
    let count =
        parse_seeds(seed_data, &mut parsed).ok_or(ProgramError::Custom(error::INVALID_SEEDS))?;
    let seeds = &parsed[..count];
    if derive(seeds, program_id)? != *pda.address() {
        return Err(ProgramError::Custom(error::WRONG_SIGNER_PDA));
    }
    let signer_seeds: [Seed; MAX_SEEDS] = core::array::from_fn(|i| Seed::from(parsed[i]));
    let signer = Signer::from(&signer_seeds[..count]);

    for pair in pairs.chunks_exact(2) {
        let (account, mint) = (&pair[0], &pair[1]);
        let amount = pda_token_account(account, mint, pda.address(), token_program.address())?;
        if amount > 0 {
            let mut data = [0; 9];
            data[0] = BURN;
            data[1..].copy_from_slice(&amount.to_le_bytes());
            let metas = [
                InstructionAccount::writable(account.address()),
                InstructionAccount::writable(mint.address()),
                InstructionAccount::readonly_signer(pda.address()),
            ];
            invoke_signed(
                &InstructionView {
                    program_id: token_program.address(),
                    accounts: &metas,
                    data: &data,
                },
                &[account, mint, pda],
                core::slice::from_ref(&signer),
            )?;
        }
        let metas = [
            InstructionAccount::writable(account.address()),
            InstructionAccount::writable(authority.address()),
            InstructionAccount::readonly_signer(pda.address()),
        ];
        invoke_signed(
            &InstructionView {
                program_id: token_program.address(),
                accounts: &metas,
                data: &[CLOSE_ACCOUNT],
            },
            &[account, authority, pda],
            core::slice::from_ref(&signer),
        )?;
    }
    Ok(())
}
