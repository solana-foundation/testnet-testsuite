//! Idempotent top-ups: bring a wallet up to target SOL and token balances.
//! Re-running only sends what is missing, so bootstrap is replayable.

use anyhow::{Context, Result};
use futures::StreamExt;
use mtm_chain::{ChainClient, SubmissionConfig};
use mtm_dex_harness::spl;
use rust_decimal::Decimal;
use solana_account_decoder_client_types::UiAccountEncoding;
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;
use solana_rpc_client_api::config::RpcAccountInfoConfig;
use solana_signer::Signer;

use crate::Token;
use crate::venue::to_atoms;

pub const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

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

/// One wallet's lamports and token balances, read in a single
/// `getMultipleAccounts` at the client's commitment. With `min_context_slot`
/// the node must have reached that slot, so a read right after a
/// confirmation in that slot cannot see the pre-transaction state. Returns
/// the context slot of the read alongside the balances.
pub async fn wallet_balances(
    chain: &ChainClient,
    owner: &Pubkey,
    tokens: &[&Token],
    min_context_slot: Option<u64>,
) -> Result<WalletBalances> {
    let mut keys = vec![*owner];
    keys.extend(
        tokens
            .iter()
            .map(|token| spl::associated_token_address(owner, &token.mint, &token.token_program)),
    );
    let config = RpcAccountInfoConfig {
        encoding: Some(UiAccountEncoding::Base64),
        commitment: Some(chain.rpc().commitment()),
        min_context_slot,
        ..RpcAccountInfoConfig::default()
    };
    let response = chain
        .rpc()
        .get_multiple_ui_accounts_with_config(&keys, config)
        .await
        .context("reading wallet balances")?;
    let mut accounts = response.value.into_iter();
    let lamports = accounts
        .next()
        .flatten()
        .map_or(0, |account| account.lamports);
    let tokens = accounts
        .map(|account| {
            account
                .and_then(|account| account.data.decode())
                .and_then(|data| spl::decode_token_account(&data))
                .map_or(0, |state| state.amount)
        })
        .collect();
    Ok(WalletBalances {
        slot: response.context.slot,
        lamports,
        tokens,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletBalances {
    /// context slot the accounts were read at
    pub slot: u64,
    pub lamports: u64,
    /// atoms, in the order the tokens were requested (0 for a missing ATA)
    pub tokens: Vec<u64>,
}

/// Token balances for many `(owner, token)` pairs and SOL balances for
/// many wallets, read with batched `getMultipleAccounts` (100 per call) so a
/// snapshot of thousands of wallets spans a few RPC round trips.
pub async fn balances(
    chain: &ChainClient,
    tokens: &[(Pubkey, &Token)],
    wallets: &[Pubkey],
) -> Result<(Vec<u64>, Vec<u64>)> {
    let atas: Vec<Pubkey> = tokens
        .iter()
        .map(|(owner, token)| {
            spl::associated_token_address(owner, &token.mint, &token.token_program)
        })
        .collect();
    let read = |keys: Vec<Pubkey>| async move {
        let chunks = futures::stream::iter(keys.chunks(100).map(|chunk| async move {
            chain
                .rpc()
                .get_multiple_accounts(chunk)
                .await
                .context("reading balances")
        }))
        .buffered(16)
        .collect::<Vec<_>>()
        .await;
        let mut out = Vec::with_capacity(keys.len());
        for chunk in chunks {
            out.extend(chunk?);
        }
        Ok::<_, anyhow::Error>(out)
    };
    let (token_accounts, wallet_accounts) = futures::try_join!(read(atas), read(wallets.to_vec()))?;
    Ok((
        token_accounts
            .into_iter()
            .map(|account| {
                account
                    .and_then(|account| spl::decode_token_account(&account.data))
                    .map_or(0, |state| state.amount)
            })
            .collect(),
        wallet_accounts
            .into_iter()
            .map(|account| account.map_or(0, |account| account.lamports))
            .collect(),
    ))
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
    // Groups that must land together: a create-ATA and its mint share a
    // transaction, or a failed second half would leave an empty ATA that
    // `Endowment` then treats as already funded.
    let mut groups: Vec<Vec<Instruction>> = Vec::new();
    if lamports < target_lamports {
        outcome.lamports_sent = target_lamports - lamports;
        groups.push(vec![spl::system_transfer(
            &faucet.pubkey(),
            owner,
            outcome.lamports_sent,
        )]);
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
        groups.push(vec![
            spl::create_associated_token_account_idempotent(
                &faucet.pubkey(),
                owner,
                &token.mint,
                &token.token_program,
            ),
            spl::mint_to_checked(
                &token.token_program,
                &token.mint,
                &ata,
                &mint_authority.pubkey(),
                target - balance,
                token.decimals,
            ),
        ]);
        outcome.minted.push((token.name.clone(), target - balance));
    }
    // A legacy transaction fits the SOL transfer plus four (create ATA,
    // mint) pairs; larger inventories go out in several transactions, whole
    // groups at a time.
    const PER_TRANSACTION: usize = 9;
    let mut transactions: Vec<Vec<Instruction>> = Vec::new();
    for group in groups {
        match transactions.last_mut() {
            Some(last) if last.len() + group.len() <= PER_TRANSACTION => last.extend(group),
            _ => transactions.push(group),
        }
    }
    for chunk in transactions {
        // The mint authority signs only when the chunk mints.
        let mints = chunk.iter().any(|ix| {
            ix.accounts
                .iter()
                .any(|meta| meta.is_signer && meta.pubkey == mint_authority.pubkey())
        });
        let signers: &[&dyn Signer] = if mints { &[mint_authority] } else { &[] };
        chain
            .submit(chunk, faucet, signers, SubmissionConfig::default())
            .await
            .with_context(|| format!("topping up {owner}"))?;
    }
    Ok(outcome)
}
