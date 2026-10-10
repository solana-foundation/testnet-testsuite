use std::{path::PathBuf, sync::Arc};

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};
use manifest_dex_orchestrator::{
    ManifestOrchestrator, MarketCreationOutcome, MarketCreationParams, OrchestrationConfig,
    TransactionOutcome, UserFlowOutcome, UserFlowParams, find_markets,
};
use mtm_dex_harness::cli::{
    EphemeralConfig, EphemeralRun, EphemeralTarget, EphemeralUserArgs, TransactionArgs,
    connected_chain, resolve_pubkey, run_ephemeral_users, signer_from_environment, submission_json,
    transaction_json, unique_mints, unordered_pairs,
};
use mtm_telemetry::init;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use solana_pubkey::Pubkey;

#[derive(Debug, Parser)]
#[command(
    name = "manifest-dex-orchestrator",
    version,
    about = "Create and exercise Manifest markets on a self-deployed Manifest program"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Ensure a market exists for every unordered pair supplied with --mint.
    CreateMarkets(CreateMarketsArgs),
    /// Run one random user's maker/taker lifecycle on every pair's existing market.
    #[command(alias = "flow")]
    UserFlow(UserFlowArgs),
}

#[derive(Debug, Args)]
struct DeploymentArgs {
    #[arg(long, default_value = "http://localhost:8899")]
    rpc_url: String,
    #[arg(long)]
    program_id: Option<Pubkey>,
    #[arg(long, default_value = "KEYPAIR_MANIFEST")]
    program_keypair_env: String,
}

impl DeploymentArgs {
    fn program_id(&self) -> Result<Pubkey> {
        resolve_pubkey(
            self.program_id,
            &self.program_keypair_env,
            "Manifest program",
        )
    }
}

#[derive(Debug, Args)]
struct CreateMarketsArgs {
    #[command(flatten)]
    deployment: DeploymentArgs,
    /// Mint public key. Repeat this flag; each pair's lower address is the base mint.
    #[arg(long = "mint", value_name = "MINT")]
    mints: Vec<Pubkey>,
    #[arg(long, default_value = "KEYPAIR_FAUCET")]
    payer_keypair_env: String,
    /// Create a new market even when one already trades the pair.
    #[arg(long, default_value_t = false)]
    allow_duplicate: bool,
    #[command(flatten)]
    transaction: TransactionArgs,
}

#[derive(Debug, Args)]
struct UserFlowArgs {
    #[command(flatten)]
    deployment: DeploymentArgs,
    /// Mint public key. Repeat this flag; each pair's lower address is the base mint.
    #[arg(long = "mint", value_name = "MINT")]
    mints: Vec<Pubkey>,
    #[command(flatten)]
    users: EphemeralUserArgs,
    /// Directory for ephemeral user keypairs, retained when a flow or recovery fails.
    #[arg(long, default_value = ".manifest-recovery")]
    recovery_dir: PathBuf,
    #[arg(long, default_value = "10000")]
    funding_base: Decimal,
    #[arg(long, default_value = "10000")]
    funding_quote: Decimal,
    #[arg(long, default_value = "1000")]
    deposit_base: Decimal,
    #[arg(long, default_value = "1000")]
    deposit_quote: Decimal,
    /// Quote per base around which the bid and ask rest.
    #[arg(long, default_value = "1")]
    mid_price: Decimal,
    #[arg(long, default_value_t = 100)]
    spread_bps: u16,
    #[arg(long, default_value = "100")]
    order_size: Decimal,
    #[arg(long, default_value = "10")]
    exact_in_amount: Decimal,
    #[arg(long, default_value = "5")]
    exact_out_amount: Decimal,
    #[command(flatten)]
    transaction: TransactionArgs,
}

#[derive(Clone, Copy)]
struct UserMarket {
    market: Pubkey,
    base_mint: Pubkey,
    quote_mint: Pubkey,
}

#[tokio::main]
async fn main() -> Result<()> {
    init("manifest-dex-orchestrator", None)?;
    match Cli::parse().command {
        Command::CreateMarkets(args) => create_all_markets(args).await,
        Command::UserFlow(args) => execute_user_flows(args).await,
    }
}

async fn create_all_markets(args: CreateMarketsArgs) -> Result<()> {
    let payer = signer_from_environment(&args.payer_keypair_env)?;
    let program_id = args.deployment.program_id()?;
    let mints = unique_mints(args.mints, "--mint")?;
    let chain = connected_chain(&args.deployment.rpc_url, args.transaction.commitment).await?;
    let orchestrator = ManifestOrchestrator::new(
        chain,
        program_id,
        Arc::clone(&payer),
        payer,
        orchestration_config(&args.transaction),
    )?;
    let mut outcomes = Vec::new();
    for (base_mint, quote_mint) in unordered_pairs(&mints) {
        tracing::info!(%base_mint, %quote_mint, "ensuring Manifest market");
        let outcome = orchestrator
            .create_market_flow(MarketCreationParams {
                base_mint,
                quote_mint,
                allow_duplicate: args.allow_duplicate,
            })
            .await?;
        outcomes.push(market_creation_json(base_mint, quote_mint, &outcome));
    }
    println!("{}", serde_json::to_string_pretty(&outcomes)?);
    Ok(())
}

async fn execute_user_flows(args: UserFlowArgs) -> Result<()> {
    let faucet = signer_from_environment(&args.users.faucet_keypair_env)?;
    let mint_authority = signer_from_environment(&args.users.mint_authority_keypair_env)?;
    let program_id = args.deployment.program_id()?;
    let chain = connected_chain(&args.deployment.rpc_url, args.transaction.commitment).await?;
    let mut targets = Vec::new();
    for (base_mint, quote_mint) in unordered_pairs(&unique_mints(args.mints, "--mint")?) {
        let markets = find_markets(&chain, program_id, base_mint, quote_mint).await?;
        let Some(market) = markets.first().copied() else {
            bail!("no Manifest market trades {base_mint}/{quote_mint}; run create-markets first")
        };
        targets.push(EphemeralTarget {
            target: UserMarket {
                market,
                base_mint,
                quote_mint,
            },
            label: market.to_string(),
            mints: vec![base_mint, quote_mint],
        });
    }
    let config = orchestration_config(&args.transaction);
    let ephemeral = EphemeralConfig {
        recovery_dir: args.recovery_dir,
        user_lamports: args.users.user_lamports,
        submission: args.transaction.submission(),
    };
    let runs = run_ephemeral_users(&chain, faucet, &ephemeral, targets, |target, user| {
        let orchestrator = ManifestOrchestrator::new(
            chain.clone(),
            program_id,
            user,
            Arc::clone(&mint_authority),
            config,
        );
        let params = UserFlowParams {
            market: target.market,
            base_mint: target.base_mint,
            quote_mint: target.quote_mint,
            funding_base: args.funding_base,
            funding_quote: args.funding_quote,
            deposit_base: args.deposit_base,
            deposit_quote: args.deposit_quote,
            mid_price: args.mid_price,
            spread_bps: args.spread_bps,
            order_size: args.order_size,
            exact_in_amount: args.exact_in_amount,
            exact_out_amount: args.exact_out_amount,
        };
        async move { Ok(orchestrator?.run_user_flow(params).await?) }
    })
    .await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&runs.iter().map(user_flow_run_json).collect::<Vec<_>>())?
    );
    Ok(())
}

fn orchestration_config(args: &TransactionArgs) -> OrchestrationConfig {
    OrchestrationConfig {
        slippage_bps: args.slippage_bps,
        compute_unit_limit: args.compute_unit_limit,
        priority_fee_micro_lamports: args.priority_fee_micro_lamports,
        confirmation_commitment: args.commitment.config(),
    }
}

fn tx_json(transaction: &TransactionOutcome) -> Value {
    transaction_json(
        &transaction.signature,
        transaction.confirmation_slot,
        transaction.simulated_compute_units,
    )
}

fn optional_tx_json(transaction: Option<&TransactionOutcome>) -> Value {
    transaction.map_or(Value::Null, tx_json)
}

fn market_creation_json(
    base_mint: Pubkey,
    quote_mint: Pubkey,
    outcome: &MarketCreationOutcome,
) -> Value {
    json!({
        "market": outcome.market.to_string(),
        "baseMint": base_mint.to_string(),
        "quoteMint": quote_mint.to_string(),
        "created": outcome.created.is_some(),
        "create": optional_tx_json(outcome.created.as_ref().map(|created| &created.transaction)),
    })
}

fn user_flow_run_json(run: &EphemeralRun<UserMarket, UserFlowOutcome>) -> Value {
    let flow = &run.outcome;
    json!({
        "market": run.target.market.to_string(), "baseMint": run.target.base_mint.to_string(), "quoteMint": run.target.quote_mint.to_string(), "user": run.user.to_string(), "startupFunding": submission_json(&run.startup_funding),
        "fundsReturned": run.recovery.json(),
        "flow": {
            "funding": { "transaction": tx_json(&flow.funding.transaction), "amountBaseRaw": flow.funding.amount_base_raw, "amountQuoteRaw": flow.funding.amount_quote_raw },
            "seat": { "index": flow.claim_seat.seat.index, "claim": optional_tx_json(flow.claim_seat.transaction.as_ref()) },
            "deposits": { "base": optional_tx_json(flow.deposit_base.transaction.as_ref()), "quote": optional_tx_json(flow.deposit_quote.transaction.as_ref()) },
            "orders": { "place": optional_tx_json(flow.place_orders.transaction.as_ref()), "resting": flow.place_orders.resting.len(), "cancel": optional_tx_json(flow.cancel_orders.transaction.as_ref()) },
            "swaps": {
                "exactIn": { "transaction": tx_json(&flow.swap_exact_in.transaction), "expectedOutputRaw": flow.swap_exact_in.quote.expected_output_raw, "executedOutputRaw": flow.swap_exact_in.executed.output_raw },
                "exactOut": { "transaction": tx_json(&flow.swap_exact_out.transaction), "expectedInputRaw": flow.swap_exact_out.quote.expected_input_raw, "executedInputRaw": flow.swap_exact_out.executed.input_raw },
            },
            "withdraw": { "transaction": optional_tx_json(flow.withdraw.transaction.as_ref()), "baseLeft": flow.withdraw.seat.base_withdrawable, "quoteLeft": flow.withdraw.seat.quote_withdrawable },
        },
    })
}
