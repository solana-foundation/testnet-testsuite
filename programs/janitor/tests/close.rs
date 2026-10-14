//! The janitor's `Close` on LiteSVM, against an SBF build that embeds the
//! test-only authorities in `tests/fixtures/authority-{a,b}.json`:
//!
//!     just test-janitor
//!
//! (builds `janitor.so` with those authorities and points `JANITOR_SO` at it).

use janitor_interface::{MAX_TARGETS_PER_TRANSACTION, close, error};
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

const RENT: u64 = 2_039_280;

fn fixture(name: &str) -> Keypair {
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

struct Env {
    svm: LiteSVM,
    program: Pubkey,
}

fn env() -> Env {
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
    fn account(&mut self, owner: Pubkey) -> Pubkey {
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

    fn send(&mut self, signer: &Keypair, ix: Instruction) -> Result<u64, TransactionError> {
        let message = Message::new(&[ix], Some(&signer.pubkey()));
        let tx = Transaction::new(&[signer], message, self.svm.latest_blockhash());
        let before = self.svm.get_balance(&signer.pubkey()).unwrap_or_default();
        self.svm.send_transaction(tx).map_err(|failed| failed.err)?;
        let after = self.svm.get_balance(&signer.pubkey()).unwrap_or_default();
        Ok(after + 5_000 - before) // credited, net of the 1-signature fee
    }

    fn gone(&self, key: &Pubkey) -> bool {
        self.svm
            .get_account(key)
            .is_none_or(|account| account.lamports == 0 && account.data.is_empty())
    }
}

fn custom(code: u32) -> TransactionError {
    TransactionError::InstructionError(0, InstructionError::Custom(code))
}

#[test]
fn an_authority_closes_program_accounts_and_receives_their_rent() {
    let mut env = env();
    let targets: Vec<Pubkey> = (0..3).map(|_| env.account(env.program)).collect();
    let authority = fixture("authority-a");
    let credited = env
        .send(&authority, close(env.program, authority.pubkey(), &targets))
        .expect("close lands");
    assert_eq!(credited, 3 * RENT);
    assert!(targets.iter().all(|target| env.gone(target)));
}

#[test]
fn every_compiled_in_authority_may_close() {
    let mut env = env();
    let target = env.account(env.program);
    let authority = fixture("authority-b");
    env.send(
        &authority,
        close(env.program, authority.pubkey(), &[target]),
    )
    .expect("second authority");
    assert!(env.gone(&target));
}

#[test]
fn a_full_batch_fits_one_transaction() {
    let mut env = env();
    let targets: Vec<Pubkey> = (0..MAX_TARGETS_PER_TRANSACTION)
        .map(|_| env.account(env.program))
        .collect();
    let authority = fixture("authority-a");
    let ix = close(env.program, authority.pubkey(), &targets);
    let message = Message::new(std::slice::from_ref(&ix), Some(&authority.pubkey()));
    let tx = Transaction::new(&[&authority], message, env.svm.latest_blockhash());
    // A legacy transaction is capped at 1232 bytes; leave room for the two
    // compute-budget instructions ChainClient::submit prepends (~50 bytes).
    let size = tx.message.serialize().len() + 1 + 64;
    assert!(size + 64 <= 1232, "{size} bytes");
    let credited = env.send(&authority, ix).expect("batch lands");
    assert_eq!(credited, MAX_TARGETS_PER_TRANSACTION as u64 * RENT);
}

#[test]
fn anyone_else_is_refused_and_nothing_moves() {
    let mut env = env();
    let target = env.account(env.program);
    let outsider = fixture("outsider");
    let result = env.send(&outsider, close(env.program, outsider.pubkey(), &[target]));
    assert_eq!(result, Err(custom(error::NOT_AN_AUTHORITY)));
    assert_eq!(env.svm.get_balance(&target), Some(RENT));
}

#[test]
fn an_authority_that_does_not_sign_is_refused() {
    let mut env = env();
    let target = env.account(env.program);
    let (authority, payer) = (fixture("authority-a"), fixture("outsider"));
    let mut ix = close(env.program, authority.pubkey(), &[target]);
    ix.accounts[0].is_signer = false;
    assert_eq!(
        env.send(&payer, ix),
        Err(TransactionError::InstructionError(
            0,
            InstructionError::MissingRequiredSignature
        ))
    );
}

#[test]
fn accounts_of_other_owners_are_refused_atomically() {
    let mut env = env();
    let ours = env.account(env.program);
    let foreign = env.account(Pubkey::new_unique());
    let authority = fixture("authority-a");
    let result = env.send(
        &authority,
        close(env.program, authority.pubkey(), &[ours, foreign]),
    );
    assert_eq!(result, Err(custom(error::NOT_OWNED)));
    // the first target was not closed either: all or nothing
    assert_eq!(env.svm.get_balance(&ours), Some(RENT));
}

#[test]
fn a_target_listed_twice_fails_the_whole_close() {
    let mut env = env();
    let target = env.account(env.program);
    let authority = fixture("authority-a");
    let result = env.send(
        &authority,
        close(env.program, authority.pubkey(), &[target, target]),
    );
    assert!(result.is_err(), "{result:?}");
    assert_eq!(env.svm.get_balance(&target), Some(RENT));
}

#[test]
fn empty_and_unknown_instructions_are_refused() {
    let mut env = env();
    let authority = fixture("authority-a");
    assert_eq!(
        env.send(&authority, close(env.program, authority.pubkey(), &[])),
        Err(custom(error::NO_TARGETS))
    );
    let target = env.account(env.program);
    let mut ix = close(env.program, authority.pubkey(), &[target]);
    ix.data = vec![9];
    assert_eq!(
        env.send(&authority, ix),
        Err(TransactionError::InstructionError(
            0,
            InstructionError::InvalidInstructionData
        ))
    );
}
