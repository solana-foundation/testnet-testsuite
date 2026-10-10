//! Ask a running router for a transaction, sign it as the user, submit it,
//! and print the quote next to the token deltas that actually landed.
//! Development only.
//!
//! ```sh
//! KEYPAIR_USER=... cargo run -p router --example swap_via_router -- \
//!   --router http://127.0.0.1:8081 --rpc-url http://localhost:8899 \
//!   --input <mint> --output <mint> --amount 1000000 [--exact-out] [--jupiter]
//! ```

use std::str::FromStr;

use anyhow::{Context, Result, bail};
use base64::Engine;
use mtm_chain::ChainClient;
use mtm_dex_harness::cli::keypair_from_environment;
use mtm_dex_harness::spl;
use router_client::{Execution, QuoteRequest, RouterClient, SwapMode};
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| -> Option<String> {
        args.iter()
            .position(|arg| arg == name)
            .and_then(|index| args.get(index + 1).cloned())
    };
    let has = |name: &str| args.iter().any(|arg| arg == name);
    let router =
        RouterClient::new(flag("--router").unwrap_or_else(|| "http://127.0.0.1:8081".into()));
    let chain = ChainClient::new(
        flag("--rpc-url").unwrap_or_else(|| "http://localhost:8899".into()),
        "confirmed",
    )?;
    let user = keypair_from_environment("KEYPAIR_USER")?;
    let input = Pubkey::from_str(&flag("--input").context("--input")?)?;
    let output = Pubkey::from_str(&flag("--output").context("--output")?)?;
    let amount: u64 = flag("--amount").context("--amount")?.parse()?;
    let quote = router
        .quote(&QuoteRequest {
            input_mint: input.to_string(),
            output_mint: output.to_string(),
            amount,
            mode: if has("--exact-out") {
                SwapMode::ExactOut
            } else {
                SwapMode::ExactIn
            },
            slippage_bps: 100,
            execution: if has("--jupiter") {
                Execution::Jupiter
            } else {
                Execution::Direct
            },
            user: Some(user.pubkey().to_string()),
        })
        .await?;
    println!("{}", serde_json::to_string_pretty(&quote)?);
    let bytes = base64::engine::general_purpose::STANDARD.decode(
        quote
            .tx_base64
            .as_deref()
            .context("router returned no transaction")?,
    )?;
    let unsigned: VersionedTransaction = wincode::deserialize(&bytes)?;
    let signed = VersionedTransaction::try_new(unsigned.message, &[&user])?;
    let before = balances(&chain, &user.pubkey(), &[input, output]).await?;
    let simulation = chain.rpc().simulate_transaction(&signed).await?;
    if let Some(error) = simulation.value.err {
        for log in simulation.value.logs.unwrap_or_default() {
            eprintln!("{log}");
        }
        bail!("simulation failed: {error:?}");
    }
    let signature = chain.rpc().send_and_confirm_transaction(&signed).await?;
    let after = balances(&chain, &user.pubkey(), &[input, output]).await?;
    println!(
        "signature {signature}\nspent {} (quoted {}), received {} (quoted {}, min {}), cu {}",
        before[0] - after[0],
        quote.in_amount,
        after[1] - before[1],
        quote.out_amount,
        quote.min_out_amount,
        simulation.value.units_consumed.unwrap_or_default(),
    );
    Ok(())
}

async fn balances(chain: &ChainClient, owner: &Pubkey, mints: &[Pubkey]) -> Result<Vec<i128>> {
    let mut out = Vec::new();
    for mint in mints {
        let program = chain.rpc().get_account(mint).await?.owner;
        let ata = spl::associated_token_address(owner, mint, &program);
        let amount = match chain.rpc().get_account(&ata).await {
            Ok(account) => spl::decode_token_account(&account.data).map_or(0, |a| a.amount),
            Err(_) => 0,
        };
        out.push(i128::from(amount));
    }
    Ok(out)
}
