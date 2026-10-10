//! The pinned Orca client is generated against Solana 2.x types while the
//! workspace RPC stack uses Agave 4.x modular types. Byte-preserving
//! conversions stay in this module.

use solana_instruction::{AccountMeta, Instruction};
use solana_instruction_v2::{AccountMeta as LegacyAccountMeta, Instruction as LegacyInstruction};
use solana_pubkey::Pubkey;
use solana_pubkey_v2::Pubkey as LegacyPubkey;

pub(crate) fn legacy_pubkey(key: Pubkey) -> LegacyPubkey {
    LegacyPubkey::new_from_array(key.to_bytes())
}

pub(crate) fn current_pubkey(key: LegacyPubkey) -> Pubkey {
    Pubkey::new_from_array(key.to_bytes())
}

pub(crate) fn legacy_meta(meta: AccountMeta) -> LegacyAccountMeta {
    LegacyAccountMeta {
        pubkey: legacy_pubkey(meta.pubkey),
        is_signer: meta.is_signer,
        is_writable: meta.is_writable,
    }
}

/// Convert a generated instruction and retarget it at the deployed program.
pub(crate) fn instruction(ix: LegacyInstruction, program_id: Pubkey) -> Instruction {
    Instruction {
        program_id,
        accounts: ix
            .accounts
            .into_iter()
            .map(|meta| AccountMeta {
                pubkey: current_pubkey(meta.pubkey),
                is_signer: meta.is_signer,
                is_writable: meta.is_writable,
            })
            .collect(),
        data: ix.data,
    }
}
