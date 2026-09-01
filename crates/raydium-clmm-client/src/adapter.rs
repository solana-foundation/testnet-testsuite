//! The Raydium interface is pinned to Solana 2.x while the workspace RPC stack
//! uses Agave 4.x modular types. Byte-preserving conversions stay in this module.

use anchor_lang::prelude::AccountMeta as LegacyAccountMeta;
use anchor_lang::prelude::Pubkey as LegacyPubkey;
use anchor_lang::solana_program::instruction::Instruction as LegacyInstruction;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;

pub(crate) fn legacy_pubkey(key: Pubkey) -> LegacyPubkey {
    LegacyPubkey::new_from_array(key.to_bytes())
}

pub(crate) fn current_pubkey(key: LegacyPubkey) -> Pubkey {
    Pubkey::new_from_array(key.to_bytes())
}

pub(crate) fn instruction(ix: LegacyInstruction) -> Instruction {
    Instruction {
        program_id: current_pubkey(ix.program_id),
        accounts: ix.accounts.into_iter().map(account_meta).collect(),
        data: ix.data,
    }
}

fn account_meta(meta: LegacyAccountMeta) -> AccountMeta {
    if meta.is_writable {
        AccountMeta::new(current_pubkey(meta.pubkey), meta.is_signer)
    } else {
        AccountMeta::new_readonly(current_pubkey(meta.pubkey), meta.is_signer)
    }
}
