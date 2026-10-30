//! The janitor's `Close` on LiteSVM, against an SBF build that embeds the
//! test-only authorities in `tests/fixtures/authority-{a,b}.json`:
//!
//!     just test-janitor
//!
//! (builds `janitor.so` with those authorities and points `JANITOR_SO` at it).

mod common;

use common::{RENT, custom, env, fixture};
use janitor_interface::{MAX_TARGETS_PER_TRANSACTION, close, error};
use solana_instruction::error::InstructionError;
use solana_message::Message;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::Transaction;
use solana_transaction_error::TransactionError;

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
