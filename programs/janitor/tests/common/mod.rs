//! Shared LiteSVM setup for the janitor tests.
#![allow(dead_code)]

use litesvm::LiteSVM;
use solana_account::Account;
use solana_instruction::Instruction;
use solana_instruction::error::InstructionError;
use solana_keypair::Keypair;
use solana_message::Message;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::Transaction;
use solana_transaction_error::TransactionError;

pub const RENT: u64 = 2_039_280;

pub fn fixture(name: &str) -> Keypair {
    let path = format!("{}/tests/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"));
    let bytes: Vec<u8> =
        serde_json_bytes(&std::fs::read_to_string(&path).expect("fixture keypair"));
    Keypair::try_from(bytes.as_slice()).expect("64-byte keypair")
}

/// `[1,2,...]` → bytes, without pulling in serde_json.
fn serde_json_bytes(text: &str) -> Vec<u8> {
    text.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|n| n.trim().parse().expect("byte"))
        .collect()
}

pub struct Env {
    pub svm: LiteSVM,
    pub program: Pubkey,
}

pub fn env() -> Env {
    let so = std::env::var("JANITOR_SO")
        .expect("JANITOR_SO: run `just test-janitor` (it builds the test .so first)");
    let mut svm = LiteSVM::new();
    let program = Pubkey::new_unique();
    svm.add_program_from_file(program, so)
        .expect("load janitor.so");
    for name in ["authority-a", "authority-b", "outsider"] {
        svm.airdrop(&fixture(name).pubkey(), 1_000_000_000)
            .expect("airdrop");
    }
    Env { svm, program }
}

impl Env {
    /// A data account owned by `owner` (the program, by default).
    pub fn account(&mut self, owner: Pubkey) -> Pubkey {
        let key = Pubkey::new_unique();
        self.svm
            .set_account(
                key,
                Account {
                    lamports: RENT,
                    data: vec![7; 165],
                    owner,
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .expect("set account");
        key
    }

    pub fn send(&mut self, signer: &Keypair, ix: Instruction) -> Result<u64, TransactionError> {
        let message = Message::new(&[ix], Some(&signer.pubkey()));
        let tx = Transaction::new(&[signer], message, self.svm.latest_blockhash());
        let before = self.svm.get_balance(&signer.pubkey()).unwrap_or_default();
        self.svm.send_transaction(tx).map_err(|failed| failed.err)?;
        let after = self.svm.get_balance(&signer.pubkey()).unwrap_or_default();
        Ok(after + 5_000 - before) // credited, net of the 1-signature fee
    }

    pub fn gone(&self, key: &Pubkey) -> bool {
        self.svm
            .get_account(key)
            .is_none_or(|account| account.lamports == 0 && account.data.is_empty())
    }
}

pub fn custom(code: u32) -> TransactionError {
    TransactionError::InstructionError(0, InstructionError::Custom(code))
}
