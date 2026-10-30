//! `CloseTokenAccounts` on LiteSVM (`just test-janitor`): vaults held by one
//! of the program's PDAs, against the real Token and Token-2022 programs.

mod common;

use common::{RENT, custom, env, fixture};
use janitor_interface::{
    MAX_TOKEN_ACCOUNTS_PER_TRANSACTION, TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID,
    close_token_accounts, error,
};
use solana_account::Account;
use solana_pubkey::Pubkey;
use solana_signer::Signer;

const MINT_RENT: u64 = 1_461_600;

fn token() -> Pubkey {
    Pubkey::new_from_array(TOKEN_PROGRAM_ID)
}

fn token_2022() -> Pubkey {
    Pubkey::new_from_array(TOKEN_2022_PROGRAM_ID)
}

/// An initialized SPL mint (82 bytes) with `supply`.
fn mint(env: &mut common::Env, program: Pubkey, supply: u64) -> Pubkey {
    let mut data = vec![0; 82];
    data[0..4].copy_from_slice(&1_u32.to_le_bytes()); // mint authority: Some
    data[4..36].copy_from_slice(Pubkey::new_unique().as_ref());
    data[36..44].copy_from_slice(&supply.to_le_bytes());
    data[44] = 6; // decimals
    data[45] = 1; // initialized
    let key = Pubkey::new_unique();
    env.svm
        .set_account(
            key,
            Account {
                lamports: MINT_RENT,
                data,
                owner: program,
                executable: false,
                rent_epoch: 0,
            },
        )
        .expect("mint");
    key
}

/// An initialized SPL token account (165 bytes) at `key`.
fn token_account(
    env: &mut common::Env,
    program: Pubkey,
    key: Pubkey,
    mint: Pubkey,
    owner: Pubkey,
    amount: u64,
) {
    let mut data = vec![0; 165];
    data[0..32].copy_from_slice(mint.as_ref());
    data[32..64].copy_from_slice(owner.as_ref());
    data[64..72].copy_from_slice(&amount.to_le_bytes());
    data[108] = 1; // initialized
    env.svm
        .set_account(
            key,
            Account {
                lamports: RENT,
                data,
                owner: program,
                executable: false,
                rent_epoch: 0,
            },
        )
        .expect("token account");
}

fn supply(env: &common::Env, mint: &Pubkey) -> u64 {
    let data = env.svm.get_account(mint).expect("mint").data;
    u64::from_le_bytes(data[36..44].try_into().expect("8 bytes"))
}

/// A pool-style PDA of the janitor's program: `["pool", tag, bump]`.
fn pool_pda(program: &Pubkey, tag: &[u8]) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[b"pool", tag], program)
}

#[test]
fn vaults_held_by_a_pda_are_burned_and_closed() {
    let mut env = env();
    let (pda, bump) = pool_pda(&env.program, b"a");
    let (mint_a, mint_b) = (mint(&mut env, token(), 1_000), mint(&mut env, token(), 50));
    let (vault_a, vault_b) = (Pubkey::new_unique(), Pubkey::new_unique());
    token_account(&mut env, token(), vault_a, mint_a, pda, 400);
    token_account(&mut env, token(), vault_b, mint_b, pda, 0);
    let authority = fixture("authority-a");
    let ix = close_token_accounts(
        env.program,
        authority.pubkey(),
        token(),
        pda,
        &[b"pool", b"a", &[bump]],
        &[(vault_a, mint_a), (vault_b, mint_b)],
    )
    .expect("seeds fit");
    let credited = env.send(&authority, ix).expect("close lands");
    assert_eq!(credited, 2 * RENT);
    assert!(env.gone(&vault_a) && env.gone(&vault_b));
    // the balance was burned, not moved
    assert_eq!(supply(&env, &mint_a), 600);
    assert_eq!(supply(&env, &mint_b), 50);
}

#[test]
fn a_self_owned_vault_pda_closes_too() {
    // Manifest-style: the vault is the PDA and holds itself.
    let mut env = env();
    let market = Pubkey::new_unique();
    let mint = mint(&mut env, token(), 10);
    let (vault, bump) =
        Pubkey::find_program_address(&[b"vault", market.as_ref(), mint.as_ref()], &env.program);
    token_account(&mut env, token(), vault, mint, vault, 10);
    let authority = fixture("authority-b");
    let ix = close_token_accounts(
        env.program,
        authority.pubkey(),
        token(),
        vault,
        &[b"vault", market.as_ref(), mint.as_ref(), &[bump]],
        &[(vault, mint)],
    )
    .expect("seeds fit");
    assert_eq!(env.send(&authority, ix), Ok(RENT));
    assert!(env.gone(&vault));
    assert_eq!(supply(&env, &mint), 0);
}

#[test]
fn token_2022_vaults_close_through_the_same_instruction() {
    let mut env = env();
    let (pda, bump) = pool_pda(&env.program, b"t22");
    let mint = mint(&mut env, token_2022(), 7);
    let vault = Pubkey::new_unique();
    token_account(&mut env, token_2022(), vault, mint, pda, 7);
    let authority = fixture("authority-a");
    let ix = close_token_accounts(
        env.program,
        authority.pubkey(),
        token_2022(),
        pda,
        &[b"pool", b"t22", &[bump]],
        &[(vault, mint)],
    )
    .expect("seeds fit");
    assert_eq!(env.send(&authority, ix), Ok(RENT));
    assert!(env.gone(&vault));
}

#[test]
fn a_full_batch_fits_one_transaction() {
    let mut env = env();
    // Seeds as long as a real pool's (an Orca whirlpool: three 32-byte keys).
    let (config, a, b) = (
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    );
    let spacing = 64_u16.to_le_bytes();
    let (pda, bump) = Pubkey::find_program_address(
        &[
            b"whirlpool",
            config.as_ref(),
            a.as_ref(),
            b.as_ref(),
            &spacing,
        ],
        &env.program,
    );
    let mut vaults = Vec::new();
    for _ in 0..MAX_TOKEN_ACCOUNTS_PER_TRANSACTION {
        let mint = mint(&mut env, token(), 1);
        let vault = Pubkey::new_unique();
        token_account(&mut env, token(), vault, mint, pda, 1);
        vaults.push((vault, mint));
    }
    let seeds: [&[u8]; 6] = [
        b"whirlpool",
        config.as_ref(),
        a.as_ref(),
        b.as_ref(),
        &spacing,
        &[bump],
    ];
    let authority = fixture("authority-a");
    let ix = close_token_accounts(
        env.program,
        authority.pubkey(),
        token(),
        pda,
        &seeds,
        &vaults,
    )
    .expect("seeds fit");
    let message =
        solana_message::Message::new(std::slice::from_ref(&ix), Some(&authority.pubkey()));
    let size = message.serialize().len() + 1 + 64;
    // leave room for submit's two compute-budget instructions
    assert!(size + 64 <= 1232, "{size} bytes");
    let credited = env.send(&authority, ix).expect("batch lands");
    assert_eq!(credited, MAX_TOKEN_ACCOUNTS_PER_TRANSACTION as u64 * RENT);
}

#[test]
fn seeds_that_derive_another_address_are_refused() {
    let mut env = env();
    let (pda, bump) = pool_pda(&env.program, b"a");
    let mint = mint(&mut env, token(), 5);
    let vault = Pubkey::new_unique();
    token_account(&mut env, token(), vault, mint, pda, 5);
    let authority = fixture("authority-a");
    let ix = close_token_accounts(
        env.program,
        authority.pubkey(),
        token(),
        pda,
        &[b"pool", b"b", &[bump]],
        &[(vault, mint)],
    )
    .expect("seeds fit");
    let result = env.send(&authority, ix);
    // `b"b"` with a's bump either derives a different PDA or no PDA at all
    assert!(
        result == Err(custom(error::WRONG_SIGNER_PDA))
            || result == Err(custom(error::INVALID_SEEDS)),
        "{result:?}"
    );
    assert_eq!(env.svm.get_balance(&vault), Some(RENT));
}

#[test]
fn token_accounts_the_pda_does_not_hold_are_refused() {
    let mut env = env();
    let (pda, bump) = pool_pda(&env.program, b"a");
    let mint = mint(&mut env, token(), 5);
    let (ours, theirs) = (Pubkey::new_unique(), Pubkey::new_unique());
    token_account(&mut env, token(), ours, mint, pda, 0);
    token_account(&mut env, token(), theirs, mint, Pubkey::new_unique(), 5);
    let authority = fixture("authority-a");
    let ix = close_token_accounts(
        env.program,
        authority.pubkey(),
        token(),
        pda,
        &[b"pool", b"a", &[bump]],
        &[(ours, mint), (theirs, mint)],
    )
    .expect("seeds fit");
    assert_eq!(
        env.send(&authority, ix),
        Err(custom(error::NOT_A_PDA_TOKEN_ACCOUNT))
    );
    // atomic: the first account was not closed either
    assert_eq!(env.svm.get_balance(&ours), Some(RENT));
    assert_eq!(env.svm.get_balance(&theirs), Some(RENT));
}

#[test]
fn a_mint_that_is_not_the_accounts_mint_is_refused() {
    let mut env = env();
    let (pda, bump) = pool_pda(&env.program, b"a");
    let (real, other) = (mint(&mut env, token(), 5), mint(&mut env, token(), 5));
    let vault = Pubkey::new_unique();
    token_account(&mut env, token(), vault, real, pda, 5);
    let authority = fixture("authority-a");
    let ix = close_token_accounts(
        env.program,
        authority.pubkey(),
        token(),
        pda,
        &[b"pool", b"a", &[bump]],
        &[(vault, other)],
    )
    .expect("seeds fit");
    assert_eq!(
        env.send(&authority, ix),
        Err(custom(error::NOT_A_PDA_TOKEN_ACCOUNT))
    );
}

#[test]
fn outsiders_and_fake_token_programs_are_refused() {
    let mut env = env();
    let (pda, bump) = pool_pda(&env.program, b"a");
    let mint = mint(&mut env, token(), 5);
    let vault = Pubkey::new_unique();
    token_account(&mut env, token(), vault, mint, pda, 5);
    let seeds: [&[u8]; 3] = [b"pool", b"a", &[bump]];
    let outsider = fixture("outsider");
    let ix = close_token_accounts(
        env.program,
        outsider.pubkey(),
        token(),
        pda,
        &seeds,
        &[(vault, mint)],
    )
    .expect("seeds fit");
    assert_eq!(
        env.send(&outsider, ix),
        Err(custom(error::NOT_AN_AUTHORITY))
    );
    let authority = fixture("authority-a");
    let fake = Pubkey::new_unique();
    let ix = close_token_accounts(
        env.program,
        authority.pubkey(),
        fake,
        pda,
        &seeds,
        &[(vault, mint)],
    )
    .expect("seeds fit");
    assert_eq!(
        env.send(&authority, ix),
        Err(custom(error::NOT_A_TOKEN_PROGRAM))
    );
    assert_eq!(env.svm.get_balance(&vault), Some(RENT));
}
