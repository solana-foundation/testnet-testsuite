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
//!
//! `CloseTokenAccounts` — token accounts (pool / market vaults) held by one
//! of the program's PDAs. The client passes that PDA's seeds (bump last);
//! the program derives the address from them and its own program ID, so it
//! can sign for any PDA the DEX could, whatever the protocol. Any balance
//! is burned, then the account is closed. Data: `[CLOSE_TOKEN_ACCOUNTS,
//! seed count, (seed length, seed bytes)...]`; accounts:
//!
//! 0. `[signer, writable]` authority (receives the rent)
//! 1. `[]` the token program (Token or Token-2022)
//! 2. `[]` the PDA the seeds derive (the token accounts' owner)
//! 3. `..` pairs of `[writable]` token account, `[writable]` its mint
#![cfg_attr(not(feature = "client"), no_std)]

/// Instruction discriminator: close the listed accounts.
pub const CLOSE: u8 = 0;
/// Instruction discriminator: burn and close token accounts a PDA holds.
pub const CLOSE_TOKEN_ACCOUNTS: u8 = 1;

/// Seed limits of `create_program_address` (bump included).
pub const MAX_SEEDS: usize = 16;
pub const MAX_SEED_LEN: usize = 32;

pub const TOKEN_PROGRAM_ID: [u8; 32] = [
    6, 221, 246, 225, 215, 101, 161, 147, 217, 203, 225, 70, 206, 235, 121, 172, 28, 180, 133, 237,
    95, 91, 55, 145, 58, 140, 245, 133, 126, 255, 0, 169,
];
pub const TOKEN_2022_PROGRAM_ID: [u8; 32] = [
    6, 221, 246, 225, 238, 117, 143, 222, 24, 66, 93, 188, 228, 108, 205, 218, 182, 26, 252, 77,
    131, 185, 13, 39, 254, 189, 249, 40, 216, 161, 139, 252,
];

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
    /// The seed data is malformed or does not derive a valid PDA.
    pub const INVALID_SEEDS: u32 = 4;
    /// The PDA account given is not the one the seeds derive.
    pub const WRONG_SIGNER_PDA: u32 = 5;
    /// The token program is neither Token nor Token-2022.
    pub const NOT_A_TOKEN_PROGRAM: u32 = 6;
    /// A token account is not an initialized account of that token
    /// program, held by the PDA, for the mint passed with it.
    pub const NOT_A_PDA_TOKEN_ACCOUNT: u32 = 7;
}

/// Targets per `Close` that keep a legacy transaction (payer, two compute
/// budget instructions, program ID) under the 1232-byte limit with margin.
pub const MAX_TARGETS_PER_TRANSACTION: usize = 24;
/// Token accounts per `CloseTokenAccounts` (two accounts each, plus up to
/// ~100 bytes of seeds) under the same limit.
pub const MAX_TOKEN_ACCOUNTS_PER_TRANSACTION: usize = 10;

/// `CloseTokenAccounts` instruction data for `seeds` (bump included), or
/// `None` past the seed limits.
pub fn close_token_accounts_data(seeds: &[&[u8]], out: &mut [u8]) -> Option<usize> {
    if seeds.len() > MAX_SEEDS || seeds.iter().any(|seed| seed.len() > MAX_SEED_LEN) {
        return None;
    }
    let mut at = 0;
    let mut put = |byte: u8| {
        let slot = out.get_mut(at)?;
        *slot = byte;
        at += 1;
        Some(())
    };
    put(CLOSE_TOKEN_ACCOUNTS)?;
    put(seeds.len() as u8)?;
    for seed in seeds {
        put(seed.len() as u8)?;
        for byte in *seed {
            put(*byte)?;
        }
    }
    Some(at)
}

/// The seeds in `CloseTokenAccounts` data (after the discriminator), into
/// `out`; returns how many. `None` if malformed (counts, lengths, trailing
/// bytes).
pub fn parse_seeds<'a>(data: &'a [u8], out: &mut [&'a [u8]; MAX_SEEDS]) -> Option<usize> {
    let (&count, mut rest) = data.split_first()?;
    let count = usize::from(count);
    if count == 0 || count > MAX_SEEDS {
        return None;
    }
    for slot in out.iter_mut().take(count) {
        let (&len, tail) = rest.split_first()?;
        let len = usize::from(len);
        if len > MAX_SEED_LEN {
            return None;
        }
        let (seed, tail) = tail.split_at_checked(len)?;
        *slot = seed;
        rest = tail;
    }
    rest.is_empty().then_some(count)
}

#[cfg(feature = "client")]
pub use client::{close, close_token_accounts};

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

    /// `CloseTokenAccounts` for `vaults` (`(token account, mint)`) held by
    /// `pda`, which `seeds` (bump last) derive under `program_id`.
    pub fn close_token_accounts(
        program_id: Pubkey,
        authority: Pubkey,
        token_program: Pubkey,
        pda: Pubkey,
        seeds: &[&[u8]],
        vaults: &[(Pubkey, Pubkey)],
    ) -> Option<Instruction> {
        let mut data = vec![0; 2 + seeds.len() * (1 + super::MAX_SEED_LEN)];
        let len = super::close_token_accounts_data(seeds, &mut data)?;
        data.truncate(len);
        let mut accounts = vec![
            AccountMeta::new(authority, true),
            AccountMeta::new_readonly(token_program, false),
            AccountMeta::new_readonly(pda, false),
        ];
        for (vault, mint) in vaults {
            accounts.push(AccountMeta::new(*vault, false));
            accounts.push(AccountMeta::new(*mint, false));
        }
        Some(Instruction {
            program_id,
            accounts,
            data,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn seeds_round_trip_and_reject_malformed_data() {
            let seeds: [&[u8]; 3] = [b"whirlpool", &[7; 32], &[254]];
            let mut data = [0; 64];
            let len = crate::close_token_accounts_data(&seeds, &mut data).expect("fits");
            assert_eq!(data[0], crate::CLOSE_TOKEN_ACCOUNTS);
            let mut out = [&[][..]; crate::MAX_SEEDS];
            assert_eq!(crate::parse_seeds(&data[1..len], &mut out), Some(3));
            assert_eq!(&out[..3], &seeds);
            // trailing garbage, truncation, zero seeds, an oversized seed
            let mut long = data[1..len].to_vec();
            long.push(0);
            assert_eq!(crate::parse_seeds(&long, &mut out), None);
            assert_eq!(crate::parse_seeds(&data[1..len - 1], &mut out), None);
            assert_eq!(crate::parse_seeds(&[0], &mut out), None);
            assert_eq!(crate::parse_seeds(&[1, 33], &mut out), None);
            let too_long: [&[u8]; 1] = [&[0; 33]];
            assert_eq!(crate::close_token_accounts_data(&too_long, &mut data), None);
        }

        #[test]
        fn token_program_ids_match_the_well_known_addresses() {
            assert_eq!(
                Pubkey::new_from_array(crate::TOKEN_PROGRAM_ID).to_string(),
                "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
            );
            assert_eq!(
                Pubkey::new_from_array(crate::TOKEN_2022_PROGRAM_ID).to_string(),
                "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"
            );
        }

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
