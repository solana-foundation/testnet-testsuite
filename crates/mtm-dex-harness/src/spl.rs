//! Pure SPL Token / ATA / System instruction builders and account decoders.
//!
//! These cover only the handful of operations the DEX harness needs (fund,
//! transfer back, close) and work for both classic Token and Token-2022 base
//! layouts. No I/O.

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;

pub const TOKEN_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const TOKEN_2022_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
pub const ASSOCIATED_TOKEN_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
pub const MEMO_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");
pub const SYSTEM_PROGRAM_ID: Pubkey = Pubkey::from_str_const("11111111111111111111111111111111");
pub const RENT_SYSVAR_ID: Pubkey =
    Pubkey::from_str_const("SysvarRent111111111111111111111111111111111");

const MINT_BASE_LEN: usize = 82;
const MINT_AUTHORITY_OFFSET: usize = 0;
const MINT_DECIMALS_OFFSET: usize = 44;
const MINT_FREEZE_AUTHORITY_OFFSET: usize = 46;
const TOKEN_ACCOUNT_MINT: core::ops::Range<usize> = 0..32;
const TOKEN_ACCOUNT_OWNER: core::ops::Range<usize> = 32..64;
const TOKEN_ACCOUNT_AMOUNT: core::ops::Range<usize> = 64..72;

/// Base mint fields shared by the classic and Token-2022 layouts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MintSummary {
    pub token_program: Pubkey,
    pub decimals: u8,
    pub mint_authority: Option<Pubkey>,
    pub freeze_authority: Option<Pubkey>,
}

/// Base token-account fields shared by the classic and Token-2022 layouts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenAccountSummary {
    pub mint: Pubkey,
    pub owner: Pubkey,
    pub amount: u64,
}

pub fn is_token_program(program: &Pubkey) -> bool {
    *program == TOKEN_PROGRAM_ID || *program == TOKEN_2022_PROGRAM_ID
}

/// Decode a mint owned by either token program. Returns `None` for any other
/// owner or for data shorter than the base mint layout.
pub fn decode_mint(owner: Pubkey, data: &[u8]) -> Option<MintSummary> {
    if !is_token_program(&owner) || data.len() < MINT_BASE_LEN {
        return None;
    }
    Some(MintSummary {
        token_program: owner,
        decimals: data[MINT_DECIMALS_OFFSET],
        mint_authority: coption_pubkey(data, MINT_AUTHORITY_OFFSET)?,
        freeze_authority: coption_pubkey(data, MINT_FREEZE_AUTHORITY_OFFSET)?,
    })
}

pub fn decode_token_account(data: &[u8]) -> Option<TokenAccountSummary> {
    Some(TokenAccountSummary {
        mint: Pubkey::new_from_array(data.get(TOKEN_ACCOUNT_MINT)?.try_into().ok()?),
        owner: Pubkey::new_from_array(data.get(TOKEN_ACCOUNT_OWNER)?.try_into().ok()?),
        amount: u64::from_le_bytes(data.get(TOKEN_ACCOUNT_AMOUNT)?.try_into().ok()?),
    })
}

/// Transfer fees withheld on a Token-2022 account (its `TransferFeeAmount`
/// extension), or 0. Token-2022 refuses to close an account while any are
/// withheld. Extensions follow the 165-byte base account: an account-type
/// byte, then `(u16 type, u16 length, value)` entries.
pub fn withheld_transfer_fee(data: &[u8]) -> u64 {
    const BASE: usize = 165;
    const TRANSFER_FEE_AMOUNT: u16 = 2;
    let mut cursor = BASE + 1;
    while let (Some(kind), Some(length)) = (
        data.get(cursor..cursor + 2),
        data.get(cursor + 2..cursor + 4),
    ) {
        let kind = u16::from_le_bytes([kind[0], kind[1]]);
        let length = usize::from(u16::from_le_bytes([length[0], length[1]]));
        let value = cursor + 4;
        if kind == 0 {
            break;
        }
        if kind == TRANSFER_FEE_AMOUNT {
            return data
                .get(value..value + 8)
                .and_then(|bytes| bytes.try_into().ok())
                .map_or(0, u64::from_le_bytes);
        }
        cursor = value + length;
    }
    0
}

fn coption_pubkey(data: &[u8], offset: usize) -> Option<Option<Pubkey>> {
    let tag = u32::from_le_bytes(data.get(offset..offset + 4)?.try_into().ok()?);
    match tag {
        0 => Some(None),
        1 => Some(Some(Pubkey::new_from_array(
            data.get(offset + 4..offset + 36)?.try_into().ok()?,
        ))),
        _ => None,
    }
}

pub fn associated_token_address(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ASSOCIATED_TOKEN_PROGRAM_ID,
    )
    .0
}

/// `CreateIdempotent` (discriminant 1) for the owner's ATA.
pub fn create_associated_token_account_idempotent(
    payer: &Pubkey,
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: ASSOCIATED_TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(associated_token_address(owner, mint, token_program), false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(*token_program, false),
        ],
        data: vec![1],
    }
}

/// `MintToChecked` (discriminant 14).
pub fn mint_to_checked(
    token_program: &Pubkey,
    mint: &Pubkey,
    destination: &Pubkey,
    authority: &Pubkey,
    amount: u64,
    decimals: u8,
) -> Instruction {
    let mut data = Vec::with_capacity(10);
    data.push(14);
    data.extend_from_slice(&amount.to_le_bytes());
    data.push(decimals);
    Instruction {
        program_id: *token_program,
        accounts: vec![
            AccountMeta::new(*mint, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*authority, true),
        ],
        data,
    }
}

/// `TransferChecked` (discriminant 12).
pub fn transfer_checked(
    token_program: &Pubkey,
    source: &Pubkey,
    mint: &Pubkey,
    destination: &Pubkey,
    authority: &Pubkey,
    amount: u64,
    decimals: u8,
) -> Instruction {
    let mut data = Vec::with_capacity(10);
    data.push(12);
    data.extend_from_slice(&amount.to_le_bytes());
    data.push(decimals);
    Instruction {
        program_id: *token_program,
        accounts: vec![
            AccountMeta::new(*source, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*authority, true),
        ],
        data,
    }
}

/// `CloseAccount` (discriminant 9); the account must hold zero tokens.
pub fn close_account(
    token_program: &Pubkey,
    account: &Pubkey,
    destination: &Pubkey,
    authority: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: *token_program,
        accounts: vec![
            AccountMeta::new(*account, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*authority, true),
        ],
        data: vec![9],
    }
}

/// System program `Transfer` (discriminant 2).
pub fn system_transfer(source: &Pubkey, destination: &Pubkey, lamports: u64) -> Instruction {
    let mut data = 2_u32.to_le_bytes().to_vec();
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction {
        program_id: SYSTEM_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*source, true),
            AccountMeta::new(*destination, false),
        ],
        data,
    }
}

/// Convert a UI amount into raw atoms, rejecting sub-atom precision.
pub fn ui_to_raw(amount: Decimal, decimals: u8) -> Result<u64, String> {
    if amount <= Decimal::ZERO {
        return Err("token amount must be greater than zero".to_owned());
    }
    let scale = 10_u64
        .checked_pow(u32::from(decimals))
        .ok_or_else(|| format!("mint decimals {decimals} cannot be represented as a u64 scale"))?;
    let raw = amount
        .checked_mul(Decimal::from(scale))
        .ok_or_else(|| "token amount overflow".to_owned())?;
    if raw.fract() != Decimal::ZERO {
        return Err(format!(
            "token amount has more than {decimals} decimal places"
        ));
    }
    raw.to_u64()
        .ok_or_else(|| "token amount does not fit u64".to_owned())
}

#[cfg(test)]
mod tests {
    #[test]
    fn withheld_transfer_fee_reads_the_extension() {
        let mut data = vec![0_u8; 165];
        assert_eq!(super::withheld_transfer_fee(&data), 0);
        data.push(2); // AccountType::Account
        // An unrelated extension (type 7, 1 byte) before TransferFeeAmount.
        data.extend_from_slice(&[7, 0, 1, 0, 0xff]);
        data.extend_from_slice(&[2, 0, 8, 0]);
        data.extend_from_slice(&42_u64.to_le_bytes());
        assert_eq!(super::withheld_transfer_fee(&data), 42);
        assert_eq!(super::withheld_transfer_fee(&data[..data.len() - 4]), 0);
    }

    use super::*;

    fn mint_bytes(authority: Option<Pubkey>, decimals: u8, freeze: Option<Pubkey>) -> Vec<u8> {
        let mut data = vec![0_u8; MINT_BASE_LEN];
        if let Some(key) = authority {
            data[0..4].copy_from_slice(&1_u32.to_le_bytes());
            data[4..36].copy_from_slice(key.as_ref());
        }
        data[MINT_DECIMALS_OFFSET] = decimals;
        data[45] = 1;
        if let Some(key) = freeze {
            data[46..50].copy_from_slice(&1_u32.to_le_bytes());
            data[50..82].copy_from_slice(key.as_ref());
        }
        data
    }

    #[test]
    fn decodes_base_mint_fields() {
        let authority = Pubkey::new_from_array([7; 32]);
        let freeze = Pubkey::new_from_array([8; 32]);
        let summary = decode_mint(
            TOKEN_2022_PROGRAM_ID,
            &mint_bytes(Some(authority), 6, Some(freeze)),
        )
        .expect("decodes");
        assert_eq!(summary.decimals, 6);
        assert_eq!(summary.mint_authority, Some(authority));
        assert_eq!(summary.freeze_authority, Some(freeze));
        assert_eq!(summary.token_program, TOKEN_2022_PROGRAM_ID);
    }

    #[test]
    fn rejects_foreign_owner_and_bad_tags() {
        let data = mint_bytes(None, 9, None);
        assert!(decode_mint(SYSTEM_PROGRAM_ID, &data).is_none());
        let mut corrupt = data;
        corrupt[0] = 7;
        assert!(decode_mint(TOKEN_PROGRAM_ID, &corrupt).is_none());
    }

    #[test]
    fn ui_to_raw_rejects_excess_precision() {
        assert_eq!(ui_to_raw(Decimal::new(125, 2), 6), Ok(1_250_000));
        assert!(ui_to_raw(Decimal::new(1, 7), 6).is_err());
        assert!(ui_to_raw(Decimal::ZERO, 6).is_err());
    }

    #[test]
    fn ata_matches_known_derivation() {
        // USDC ATA of the system program, cross-checked with spl-associated-token-account.
        let owner = Pubkey::from_str_const("11111111111111111111111111111111");
        let mint = Pubkey::from_str_const("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
        let ata = associated_token_address(&owner, &mint, &TOKEN_PROGRAM_ID);
        let again = associated_token_address(&owner, &mint, &TOKEN_PROGRAM_ID);
        assert_eq!(ata, again);
        assert_ne!(
            ata,
            associated_token_address(&owner, &mint, &TOKEN_2022_PROGRAM_ID)
        );
    }
}
