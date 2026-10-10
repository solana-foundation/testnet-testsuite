//! Idempotent top-ups: bring a wallet up to target SOL and token balances.
//! Re-running only sends what is missing, so bootstrap is replayable.

use anyhow::{Context, Result};
use mtm_chain::{ChainClient, SubmissionConfig};
use mtm_dex_harness::spl;
use rust_decimal::Decimal;
use solana_pubkey::Pubkey;
use solana_signer::Signer;

use crate::Token;
use crate::venue::to_atoms;

const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

#[derive(Debug, Clone, Default)]
pub struct TopUp {
    pub lamports_sent: u64,
    /// `(token name, atoms minted)` for every token that needed minting.
    pub minted: Vec<(String, u64)>,
}

pub async fn token_account_exists(
    chain: &ChainClient,
    owner: &Pubkey,
    token: &Token,
) -> Result<bool> {
    let ata = spl::associated_token_address(owner, &token.mint, &token.token_program);
    Ok(chain
        .rpc()
        .get_multiple_accounts(&[ata])
        .await
        .context("reading token account")?
        .into_iter()
        .next()
        .flatten()
        .is_some())
}

pub async fn token_balance(chain: &ChainClient, owner: &Pubkey, token: &Token) -> Result<u64> {
    let ata = spl::associated_token_address(owner, &token.mint, &token.token_program);
    let accounts = chain
        .rpc()
        .get_multiple_accounts(&[ata])
        .await
        .context("reading token balance")?;
    Ok(accounts
        .into_iter()
        .next()
        .flatten()
        .and_then(|account| spl::decode_token_account(&account.data))
        .map_or(0, |state| state.amount))
}

/// How token targets are applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Funding {
    /// Mint up to the target whenever the wallet holds less (takers).
    TopUp,
    /// Mint the target once, only if the wallet never had a token account
    /// for the mint (makers: their inventory moves into seats and positions,
    /// so a low wallet balance is not a reason to mint more).
    Endowment,
}

/// Send SOL from `faucet` and mint tokens with `mint_authority` until
/// `owner` holds at least `sol` and every `(token, amount)` per `funding`.
pub async fn top_up(
    chain: &ChainClient,
    faucet: &dyn Signer,
    mint_authority: &dyn Signer,
    owner: &Pubkey,
    sol: Decimal,
    tokens: &[(&Token, Decimal)],
    funding: Funding,
) -> Result<TopUp> {
    let mut outcome = TopUp::default();
    let target_lamports = sol
        .checked_mul(Decimal::from(LAMPORTS_PER_SOL))
        .and_then(|value| rust_decimal::prelude::ToPrimitive::to_u64(&value.floor()))
        .context("SOL target overflow")?;
    let lamports = chain
        .rpc()
        .get_balance(owner)
        .await
        .context("reading SOL balance")?;
    let mut instructions = Vec::new();
    if lamports < target_lamports {
        outcome.lamports_sent = target_lamports - lamports;
        instructions.push(spl::system_transfer(
            &faucet.pubkey(),
            owner,
            outcome.lamports_sent,
        ));
    }
    for (token, amount) in tokens {
        let target = to_atoms(token, *amount).context("token target overflow")?;
        if funding == Funding::Endowment && token_account_exists(chain, owner, token).await? {
            continue;
        }
        let balance = token_balance(chain, owner, token).await?;
        if balance >= target {
            continue;
        }
        let ata = spl::associated_token_address(owner, &token.mint, &token.token_program);
        instructions.push(spl::create_associated_token_account_idempotent(
            &faucet.pubkey(),
            owner,
            &token.mint,
            &token.token_program,
        ));
        instructions.push(spl::mint_to_checked(
            &token.token_program,
            &token.mint,
            &ata,
            &mint_authority.pubkey(),
            target - balance,
            token.decimals,
        ));
        outcome.minted.push((token.name.clone(), target - balance));
    }
    if !instructions.is_empty() {
        // The mint authority signs only when something is minted.
        let signers: &[&dyn Signer] = if outcome.minted.is_empty() {
            &[]
        } else {
            &[mint_authority]
        };
        chain
            .submit(instructions, faucet, signers, SubmissionConfig::default())
            .await
            .with_context(|| format!("topping up {owner}"))?;
    }
    Ok(outcome)
}
