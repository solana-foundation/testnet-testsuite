//! The janitor: a teardown-only program deployed over a scenario's DEX
//! programs (an upgrade in place). Running at their program IDs, it owns
//! every account they created, and `Close` returns those accounts' rent to
//! an authority compiled into the binary (see `build.rs`).
//!
//! Everything it accepts is checked before anything moves: the signer is an
//! authority, and every target is writable, owned by this program ID and not
//! executable. A target listed twice fails the second check (closing zeroes
//! the owner), so the instruction is all-or-nothing.
#![cfg_attr(target_os = "solana", no_std)]

use janitor_interface::{CLOSE, error};
use pinocchio::error::ProgramError;
use pinocchio::{AccountView, Address, ProgramResult};

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
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

/// Is `key` one of `authorities`?
pub fn is_authority(key: &Address, authorities: &[[u8; 32]]) -> bool {
    authorities
        .iter()
        .any(|authority| authority == key.as_array())
}

fn close(
    program_id: &Address,
    accounts: &mut [AccountView],
    authorities: &[[u8; 32]],
) -> ProgramResult {
    let [authority, targets @ ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    if !authority.is_signer() {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if !authority.is_writable() {
        return Err(ProgramError::InvalidAccountData);
    }
    if !is_authority(authority.address(), authorities) {
        return Err(ProgramError::Custom(error::NOT_AN_AUTHORITY));
    }
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
