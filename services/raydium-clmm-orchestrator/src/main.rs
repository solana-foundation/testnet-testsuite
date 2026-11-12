use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use mtm_dex_harness::cli::{
    Commitment, EphemeralConfig, EphemeralRun, EphemeralTarget, EphemeralUserArgs, TransactionArgs,
    connected_chain, resolve_pubkey, run_ephemeral_users, signer_from_environment, submission_json,
    unique_mints, unordered_pairs,
};
use mtm_telemetry::init;
use raydium_clmm_orchestrator::{
    EnsureAmmConfigParams, OrchestrationConfig, PoolCreationOutcome, PoolCreationParams,
    RaydiumClmmAdminOrchestrator, RaydiumClmmOrchestrator, TransactionOutcome, UserFlowOutcome,
    UserFlowParams, derive_pool_address,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use solana_pubkey::Pubkey;

#[derive(Debug, Parser)]
#[command(
    name = "raydium-clmm-orchestrator",
    version,
    about = "Configure, create, and exercise a Raydium CLMM deployment"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a pool for every unordered pair supplied with --mint.
    CreatePools(CreatePoolsArgs),
    /// Run one random user's lifecycle on every existing pair supplied with --mint.
    #[command(alias = "flow")]
    UserFlow(UserFlowArgs),
    /// Create or validate a canonical AMM config.
    Admin(AdminArgs),
}

#[derive(Debug, Args)]
struct CreatePoolsArgs {
    #[arg(long, default_value = "http://localhost:8899")]
    rpc_url: String,
    #[arg(long)]
    program_id: Option<Pubkey>,
    #[arg(long, default_value = "KEYPAIR_RAYDIUM_CLMM")]
    program_keypair_env: String,
    #[arg(long)]
    amm_config: Pubkey,
    /// Mint public key. Repeat this flag to create every unordered pair.
    #[arg(long = "mint", value_name = "MINT")]
    mints: Vec<Pubkey>,
    #[arg(long, default_value = "KEYPAIR_FAUCET")]
    payer_keypair_env: String,
    /// Initial price, expressed as mint B per mint A for every listed pair.
    #[arg(long, default_value = "1")]
    initial_price: Decimal,
    #[arg(long)]
    open_time: Option<u64>,
    #[command(flatten)]
    transaction: TransactionArgs,
}

#[derive(Debug, Args)]
struct UserFlowArgs {
    #[arg(long, default_value = "http://localhost:8899")]
    rpc_url: String,
    #[arg(long)]
    program_id: Option<Pubkey>,
    #[arg(long, default_value = "KEYPAIR_RAYDIUM_CLMM")]
    program_keypair_env: String,
    #[arg(long)]
    amm_config: Pubkey,
    /// Mint public key. Repeat this flag to exercise every unordered pair's existing pool.
    #[arg(long = "mint", value_name = "MINT")]
    mints: Vec<Pubkey>,
    #[command(flatten)]
    users: EphemeralUserArgs,
    /// Directory for ephemeral user keypairs, retained when a recovery fails.
    #[arg(long, default_value = ".raydium-clmm-recovery")]
    recovery_dir: PathBuf,
    #[arg(long, default_value = "10000")]
    funding_a: Decimal,
    #[arg(long, default_value = "10000")]
    funding_b: Decimal,
    #[arg(long, default_value = "0.5")]
    lower_price: Decimal,
    #[arg(long, default_value = "2")]
    upper_price: Decimal,
    #[arg(long, default_value = "1000")]
    open_amount: Decimal,
    #[arg(long, default_value = "500")]
    increase_amount: Decimal,
    #[arg(long, default_value = "10")]
    exact_in_amount: Decimal,
    #[arg(long, default_value = "5")]
    exact_out_amount: Decimal,
    #[arg(long, default_value_t = false)]
    with_metadata: bool,
    #[command(flatten)]
    transaction: TransactionArgs,
}

#[derive(Debug, Args)]
struct AdminArgs {
    #[arg(long, default_value = "http://localhost:8899")]
    rpc_url: String,
    #[arg(long)]
    program_id: Option<Pubkey>,
    #[arg(long, default_value = "KEYPAIR_RAYDIUM_CLMM")]
    program_keypair_env: String,
    #[arg(long, default_value = "KEYPAIR_PROGRAM_AUTHORITY")]
    admin_keypair_env: String,
    #[arg(long, default_value_t = 0)]
    config_index: u16,
    #[arg(long, default_value_t = 10)]
    tick_spacing: u16,
    #[arg(long, default_value_t = 500)]
    trade_fee_rate: u32,
    #[arg(long, default_value_t = 120_000)]
    protocol_fee_rate: u32,
    #[arg(long, default_value_t = 0)]
    fund_fee_rate: u32,
    #[arg(long, default_value_t = 400_000)]
    compute_unit_limit: u32,
    #[arg(long, default_value_t = 0)]
    priority_fee_micro_lamports: u64,
    #[arg(long, value_enum, default_value_t = Commitment::Confirmed)]
    commitment: Commitment,
}

#[derive(Clone, Copy)]
struct UserPool {
    pool: Pubkey,
    mint_a: Pubkey,
    mint_b: Pubkey,
}

#[tokio::main]
async fn main() -> Result<()> {
    init("raydium-clmm-orchestrator", None)?;
    match Cli::parse().command {
        Command::CreatePools(args) => create_all_pools(args).await,
        Command::UserFlow(args) => execute_user_flows(args).await,
        Command::Admin(args) => run_admin(args).await,
    }
}

/// Creates one pool for each unordered pair of mints named by the selected environment variable.
async fn create_all_pools(args: CreatePoolsArgs) -> Result<()> {
    let payer = signer_from_environment(&args.payer_keypair_env)?;
    let program_id = resolve_pubkey(
        args.program_id,
        &args.program_keypair_env,
        "Raydium program",
    )?;
    let mints = unique_mints(args.mints, "--mint")?;
    let chain = connected_chain(&args.rpc_url, args.transaction.commitment).await?;
    let orchestrator = RaydiumClmmOrchestrator::new(
        chain,
        program_id,
        Arc::clone(&payer),
        payer,
        orchestration_config(&args.transaction),
    )?;
    let mut outcomes = Vec::new();
    for (mint_a, mint_b) in unordered_pairs(&mints) {
        tracing::info!(%mint_a, %mint_b, "creating Raydium CLMM pool");
        let outcome = orchestrator
            .create_pool_flow(PoolCreationParams {
                amm_config: args.amm_config,
                mint_a,
                mint_b,
                initial_price: args.initial_price,
                open_time: args.open_time,
            })
            .await?;
        outcomes.push(pool_creation_json(&outcome));
    }
    println!("{}", serde_json::to_string_pretty(&outcomes)?);
    Ok(())
}

/// Runs one independent random user against every existing unordered mint pair
/// (the shared ephemeral-user lifecycle: fund, run, always reclaim).
async fn execute_user_flows(args: UserFlowArgs) -> Result<()> {
    let faucet = signer_from_environment(&args.users.faucet_keypair_env)?;
    let mint_authority = signer_from_environment(&args.users.mint_authority_keypair_env)?;
    let program_id = resolve_pubkey(
        args.program_id,
        &args.program_keypair_env,
        "Raydium program",
    )?;
    let targets = unordered_pairs(&unique_mints(args.mints, "--mint")?)
        .into_iter()
        .map(|(mint_a, mint_b)| {
            let pool = derive_pool_address(program_id, args.amm_config, mint_a, mint_b)?;
            Ok(EphemeralTarget {
                target: UserPool {
                    pool,
                    mint_a,
                    mint_b,
                },
                label: pool.to_string(),
                mints: vec![mint_a, mint_b],
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let chain = connected_chain(&args.rpc_url, args.transaction.commitment).await?;
    let config = orchestration_config(&args.transaction);
    let ephemeral = EphemeralConfig {
        recovery_dir: args.recovery_dir,
        user_lamports: args.users.user_lamports,
        submission: args.transaction.submission(),
    };
    let runs = run_ephemeral_users(&chain, faucet, &ephemeral, targets, |pool, user| {
        let orchestrator = RaydiumClmmOrchestrator::new(
            chain.clone(),
            program_id,
            user,
            Arc::clone(&mint_authority),
            config,
        );
        let params = UserFlowParams {
            amm_config: args.amm_config,
            pool: pool.pool,
            mint_a: pool.mint_a,
            mint_b: pool.mint_b,
            funding_a: args.funding_a,
            funding_b: args.funding_b,
            lower_price: args.lower_price,
            upper_price: args.upper_price,
            open_amount: args.open_amount,
            increase_amount: args.increase_amount,
            exact_in_amount: args.exact_in_amount,
            exact_out_amount: args.exact_out_amount,
            with_metadata: args.with_metadata,
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

async fn run_admin(args: AdminArgs) -> Result<()> {
    let program_id = resolve_pubkey(
        args.program_id,
        &args.program_keypair_env,
        "Raydium program",
    )?;
    let admin = signer_from_environment(&args.admin_keypair_env)?;
    let orchestrator = RaydiumClmmAdminOrchestrator::new(
        connected_chain(&args.rpc_url, args.commitment).await?,
        program_id,
        admin,
        args.compute_unit_limit,
        args.priority_fee_micro_lamports,
        args.commitment.config(),
    )?;
    let outcome = orchestrator
        .ensure_amm_config(EnsureAmmConfigParams {
            index: args.config_index,
            tick_spacing: args.tick_spacing,
            trade_fee_rate: args.trade_fee_rate,
            protocol_fee_rate: args.protocol_fee_rate,
            fund_fee_rate: args.fund_fee_rate,
        })
        .await?;
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({ "programId": orchestrator.program_id().to_string(), "admin": outcome.admin.to_string(), "ammConfig": outcome.amm_config.to_string(), "created": outcome.created, "transaction": outcome.transaction.as_ref().map(transaction_json) })
        )?
    );
    Ok(())
}

fn transaction_json(transaction: &TransactionOutcome) -> Value {
    json!({ "signature": transaction.signature.to_string(), "confirmationSlot": transaction.confirmation_slot, "simulatedComputeUnits": transaction.simulated_compute_units })
}

fn pool_creation_json(outcome: &PoolCreationOutcome) -> Value {
    json!({ "pool": outcome.create_pool.accounts.pool.to_string(), "mint0": outcome.create_pool.quote.mint_0.to_string(), "mint1": outcome.create_pool.quote.mint_1.to_string(), "create": transaction_json(&outcome.create_pool.transaction) })
}

fn user_flow_run_json(run: &EphemeralRun<UserPool, UserFlowOutcome>) -> Value {
    json!({
        "pool": run.target.pool.to_string(), "mintA": run.target.mint_a.to_string(), "mintB": run.target.mint_b.to_string(), "user": run.user.to_string(), "startupFunding": submission_json(&run.startup_funding),
        "fundsReturned": run.recovery.json(), "flow": user_flow_json(&run.outcome),
    })
}

fn user_flow_json(outcome: &UserFlowOutcome) -> Value {
    json!({
        "funding": { "transaction": transaction_json(&outcome.funding.transaction), "tokenAccountA": outcome.funding.token_account_a.to_string(), "tokenAccountB": outcome.funding.token_account_b.to_string(), "amountARaw": outcome.funding.amount_a_raw, "amountBRaw": outcome.funding.amount_b_raw },
        "position": { "nftMint": outcome.open_position.accounts.position_nft_mint.to_string(), "personalPosition": outcome.open_position.accounts.personal_position.to_string(), "open": transaction_json(&outcome.open_position.transaction), "increase": transaction_json(&outcome.increase_liquidity.transaction), "decrease": transaction_json(&outcome.decrease_liquidity.transaction), "collect": transaction_json(&outcome.collect_position.transaction), "close": transaction_json(&outcome.close_position.transaction) },
        "swaps": { "exactIn": { "transaction": transaction_json(&outcome.swap_exact_in.transaction), "expectedInputRaw": outcome.swap_exact_in.quote.expected_input_raw, "expectedOutputRaw": outcome.swap_exact_in.quote.expected_output_raw }, "exactOut": { "transaction": transaction_json(&outcome.swap_exact_out.transaction), "expectedInputRaw": outcome.swap_exact_out.quote.expected_input_raw, "expectedOutputRaw": outcome.swap_exact_out.quote.expected_output_raw } },
    })
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    #[test]
    fn command_line_is_well_formed() {
        super::Cli::command().debug_assert();
    }
}
