//! The janitor program's instruction format.
//!
//! The janitor is deployed *over* a scenario's DEX programs at the end of a
//! run (an upgrade in place, so it runs at their program IDs and owns every
//! account they created) and closes those accounts, returning their rent to
//! one of a fixed set of authorities compiled into the program.
//!
//! `Close` — data: `[CLOSE]`; accounts:
//!
//! 0. `[signer, writable]` authority (receives the rent)
//! 1. `..` `[writable]` accounts owned by the program to close
#![cfg_attr(not(feature = "client"), no_std)]

/// Instruction discriminator: close the listed accounts.
pub const CLOSE: u8 = 0;

/// Custom error codes (`ProgramError::Custom`).
pub mod error {
    /// The signer is not one of the compiled-in authorities.
    pub const NOT_AN_AUTHORITY: u32 = 0;
    /// `Close` was given no accounts to close.
    pub const NO_TARGETS: u32 = 1;
    /// A target is not owned by the program it was sent to.
    pub const NOT_OWNED: u32 = 2;
    /// A target is executable (a program account).
    pub const EXECUTABLE: u32 = 3;
}

/// Targets per `Close` that keep a legacy transaction (payer, two compute
/// budget instructions, program ID) under the 1232-byte limit with margin.
pub const MAX_TARGETS_PER_TRANSACTION: usize = 24;

#[cfg(feature = "client")]
pub use client::close;

#[cfg(feature = "client")]
mod client {
    use solana_instruction::{AccountMeta, Instruction};
    use solana_pubkey::Pubkey;

    /// `Close` for `targets`, sent to `program_id` (the DEX program ID the
    /// janitor was deployed over); the rent goes to `authority`.
    pub fn close(program_id: Pubkey, authority: Pubkey, targets: &[Pubkey]) -> Instruction {
        let mut accounts = Vec::with_capacity(targets.len() + 1);
        accounts.push(AccountMeta::new(authority, true));
        accounts.extend(
            targets
                .iter()
                .map(|target| AccountMeta::new(*target, false)),
        );
        Instruction {
            program_id,
            accounts,
            data: vec![super::CLOSE],
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn close_puts_the_authority_first_and_every_target_writable() {
            let (program, authority) = (Pubkey::new_unique(), Pubkey::new_unique());
            let targets = [Pubkey::new_unique(), Pubkey::new_unique()];
            let ix = close(program, authority, &targets);
            assert_eq!(ix.program_id, program);
            assert_eq!(ix.data, [crate::CLOSE]);
            assert!(ix.accounts[0].is_signer && ix.accounts[0].is_writable);
            assert_eq!(ix.accounts[0].pubkey, authority);
            for (meta, target) in ix.accounts[1..].iter().zip(&targets) {
                assert!(meta.is_writable && !meta.is_signer);
                assert_eq!(meta.pubkey, *target);
            }
        }
    }
}
