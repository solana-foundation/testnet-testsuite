//! Seed standing liquidity on every pool and market the router can see, so
//! a local stack has something to route through. Development only: the
//! faucet mints test tokens to itself, opens a wide position on every Raydium
//! CLMM and Orca Whirlpool pool, and rests a bid and an ask on every Manifest
//! market. Positions and orders are left open.
//!
//! ```sh
//! KEYPAIR_FAUCET=... KEYPAIR_TOKEN_MINT_AUTHORITY=... \
//! cargo run -p router --example seed_liquidity -- \
//!   --rpc-url http://localhost:8899 \
//!   --raydium-clmm <program> --orca-whirlpool <program> --manifest <program>
//! ```

use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use manifest_dex_client::{ManifestClient, OrderSpec, Side, TokenAmount as ManifestAmount};
use mtm_dex_harness::cli::{Commitment, connected_chain, signer_from_environment};
use mtm_dex_harness::funding::{Grant, TestTokenFunder};
use mtm_venues::{PoolSnapshot, VenueKind};
use rust_decimal::Decimal;
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| -> Option<String> {
        args.iter()
            .position(|arg| arg == name)
            .and_then(|index| args.get(index + 1).cloned())
    };
    let rpc_url = flag("--rpc-url").unwrap_or_else(|| "http://localhost:8899".to_owned());
    let amount = Decimal::from_str(&flag("--amount").unwrap_or_else(|| "100000".to_owned()))?;
    let faucet = signer_from_environment("KEYPAIR_FAUCET")?;
    let mint_authority = signer_from_environment("KEYPAIR_TOKEN_MINT_AUTHORITY")?;
    let chain = connected_chain(&rpc_url, Commitment::Confirmed).await?;
    let confirmed = CommitmentConfig::confirmed();

    let mut minted = std::collections::BTreeSet::new();
    for kind in VenueKind::ALL {
        let Some(program) = flag(&format!("--{}", kind.as_str())) else {
            continue;
        };
        let program = Pubkey::from_str(&program).context("program id")?;
        for pool in mtm_venues::discover(&chain, kind, program).await? {
            let snapshot = match PoolSnapshot::load(&chain, kind, program, pool).await {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    eprintln!("skip {pool}: {error}");
                    continue;
                }
            };
            let (mint_a, mint_b) = snapshot.mints();
            let grants: Vec<Grant> = [mint_a, mint_b]
                .into_iter()
                .filter(|mint| minted.insert((*mint, pool)))
                .map(|mint| Grant {
                    mint,
                    amount: amount * Decimal::from(4),
                })
                .collect();
            TestTokenFunder::new(
                chain.clone(),
                Arc::clone(&faucet),
                Arc::clone(&mint_authority),
                Default::default(),
            )
            .fund(&grants)
            .await?;
            match kind {
                VenueKind::RaydiumClmm => {
                    let client = raydium_clmm_client::RaydiumClmmClient::new(
                        chain.clone(),
                        program,
                        Arc::clone(&faucet),
                        100,
                        1_400_000,
                        0,
                        confirmed,
                    )?;
                    let outcome = client
                        .open_position(raydium_clmm_client::OpenPositionParams {
                            pool,
                            lower_price: Decimal::new(25, 2),
                            upper_price: Decimal::from(4),
                            input: raydium_clmm_client::TokenAmount {
                                mint: mint_a,
                                amount,
                            },
                            with_metadata: false,
                        })
                        .await?;
                    println!(
                        "raydium-clmm {pool}: position {}",
                        outcome.accounts.position_nft_mint
                    );
                }
                VenueKind::OrcaWhirlpool => {
                    let client = orca_whirlpool_client::OrcaWhirlpoolClient::new(
                        chain.clone(),
                        program,
                        Arc::clone(&faucet),
                        100,
                        1_400_000,
                        0,
                        confirmed,
                    )?;
                    let outcome = client
                        .open_position(orca_whirlpool_client::OpenPositionParams {
                            pool,
                            lower_price: Decimal::new(5, 1),
                            upper_price: Decimal::from(2),
                            input: orca_whirlpool_client::TokenAmount {
                                mint: mint_a,
                                amount,
                            },
                            with_metadata: false,
                        })
                        .await?;
                    println!(
                        "orca-whirlpool {pool}: position {}",
                        outcome.accounts.position_mint
                    );
                }
                VenueKind::Manifest => {
                    let client = ManifestClient::new(
                        chain.clone(),
                        program,
                        Arc::clone(&faucet),
                        100,
                        1_400_000,
                        0,
                        confirmed,
                    )?;
                    client.claim_seat(pool).await?;
                    for mint in [mint_a, mint_b] {
                        client
                            .deposit(pool, ManifestAmount { mint, amount })
                            .await?;
                    }
                    let half = amount / Decimal::from(4);
                    let outcome = client
                        .place_orders(
                            pool,
                            &[
                                OrderSpec {
                                    side: Side::Bid,
                                    price: Decimal::new(995, 3),
                                    base_amount: half,
                                },
                                OrderSpec {
                                    side: Side::Ask,
                                    price: Decimal::new(1005, 3),
                                    base_amount: half,
                                },
                            ],
                        )
                        .await?;
                    println!("manifest {pool}: {} resting orders", outcome.resting.len());
                }
            }
        }
    }
    Ok(())
}
