use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use mtm_dex_harness::cli::{
    Commitment, EphemeralConfig, EphemeralRun, EphemeralTarget, EphemeralUserArgs, TransactionArgs,
    connected_chain, keypair_from_environment, resolve_pubkey, run_ephemeral_users,
    signer_from_environment, submission_json, transaction_json, unique_mints, unordered_pairs,
};
use mtm_telemetry::init;
use orca_whirlpool_orchestrator::{
    EnsureConfigParams, OrcaWhirlpoolAdminOrchestrator, OrcaWhirlpoolOrchestrator,
    OrchestrationConfig, PoolCreationOutcome, PoolCreationParams, TransactionOutcome,
    UserFlowOutcome, UserFlowParams, derive_pool_address,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use solana_pubkey::Pubkey;
use solana_signer::Signer;

#[derive(Debug, Parser)]
#[command(
    name = "orca-whirlpool-orchestrator",
    version,
    about = "Configure, create, and exercise an Orca Whirlpool deployment"
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
    /// Create or validate the WhirlpoolsConfig and its fee tier.
    Admin(AdminArgs),
}

/// Program, config, and fee-tier selection shared by every command.
#[derive(Debug, Args)]
struct DeploymentArgs {
    #[arg(long, default_value = "http://localhost:8899")]
    rpc_url: String,
    #[arg(long)]
    program_id: Option<Pubkey>,
    #[arg(long, default_value = "KEYPAIR_ORCA_WHIRLPOOL")]
    program_keypair_env: String,
    /// WhirlpoolsConfig address. Defaults to the public key of --config-keypair-env.
    #[arg(long)]
    whirlpools_config: Option<Pubkey>,
    #[arg(long, default_value = "KEYPAIR_ORCA_WHIRLPOOLS_CONFIG")]
    config_keypair_env: String,
    #[arg(long, default_value_t = 64)]
    tick_spacing: u16,
}

impl DeploymentArgs {
    fn program_id(&self) -> Result<Pubkey> {
        resolve_pubkey(
            self.program_id,
            &self.program_keypair_env,
            "Whirlpool program",
        )
    }

    fn whirlpools_config(&self) -> Result<Pubkey> {
        resolve_pubkey(
            self.whirlpools_config,
            &self.config_keypair_env,
            "WhirlpoolsConfig",
        )
    }
}

#[derive(Debug, Args)]
struct CreatePoolsArgs {
    #[command(flatten)]
    deployment: DeploymentArgs,
    /// Mint public key. Repeat this flag to create every unordered pair.
    #[arg(long = "mint", value_name = "MINT")]
    mints: Vec<Pubkey>,
    #[arg(long, default_value = "KEYPAIR_FAUCET")]
    payer_keypair_env: String,
    /// Initial price, expressed as mint B per mint A for every listed pair.
    #[arg(long, default_value = "1")]
    initial_price: Decimal,
    /// Lower bound (mint B per mint A) of the tick arrays seeded per pool.
    #[arg(long, default_value = "0.5")]
    seed_lower_price: Decimal,
    /// Upper bound (mint B per mint A) of the tick arrays seeded per pool.
    #[arg(long, default_value = "2")]
    seed_upper_price: Decimal,
    #[command(flatten)]
    transaction: TransactionArgs,
}

#[derive(Debug, Args)]
struct UserFlowArgs {
    #[command(flatten)]
    deployment: DeploymentArgs,
    /// Mint public key. Repeat this flag to exercise every unordered pair's existing pool.
    #[arg(long = "mint", value_name = "MINT")]
    mints: Vec<Pubkey>,
    #[command(flatten)]
    users: EphemeralUserArgs,
    /// Directory for ephemeral user keypairs, retained when a flow or recovery fails.
    #[arg(long, default_value = ".orca-whirlpool-recovery")]
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
    #[command(flatten)]
    deployment: DeploymentArgs,
    #[arg(long, default_value = "KEYPAIR_PROGRAM_AUTHORITY")]
    admin_keypair_env: String,
    /// Basis points of each trade fee routed to the protocol (max 2,500).
    #[arg(long, default_value_t = 300)]
    default_protocol_fee_rate: u16,
    /// Trade fee in hundredths of a basis point (3,000 = 0.30%).
    #[arg(long, default_value_t = 3_000)]
    default_fee_rate: u16,
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
    init("orca-whirlpool-orchestrator", None)?;
    match Cli::parse().command {
        Command::CreatePools(args) => create_all_pools(args).await,
        Command::UserFlow(args) => execute_user_flows(args).await,
        Command::Admin(args) => run_admin(args).await,
    }
}

/// Creates and seeds one pool for each unordered pair of supplied mints.
async fn create_all_pools(args: CreatePoolsArgs) -> Result<()> {
    let payer = signer_from_environment(&args.payer_keypair_env)?;
    let program_id = args.deployment.program_id()?;
    let whirlpools_config = args.deployment.whirlpools_config()?;
    let mints = unique_mints(args.mints, "--mint")?;
    let chain = connected_chain(&args.deployment.rpc_url, args.transaction.commitment).await?;
    let orchestrator = OrcaWhirlpoolOrchestrator::new(
        chain,
        program_id,
        Arc::clone(&payer),
        payer,
        orchestration_config(&args.transaction),
    )?;
    let mut outcomes = Vec::new();
    for (mint_a, mint_b) in unordered_pairs(&mints) {
        tracing::info!(%mint_a, %mint_b, "creating Orca Whirlpool");
        let outcome = orchestrator
            .create_pool_flow(PoolCreationParams {
                whirlpools_config,
                tick_spacing: args.deployment.tick_spacing,
                mint_a,
                mint_b,
                initial_price: args.initial_price,
                seed_lower_price: args.seed_lower_price,
                seed_upper_price: args.seed_upper_price,
            })
            .await?;
        outcomes.push(pool_creation_json(&outcome));
    }
    println!("{}", serde_json::to_string_pretty(&outcomes)?);
    Ok(())
}

/// Runs one independent random user against every existing unordered mint pair.
async fn execute_user_flows(args: UserFlowArgs) -> Result<()> {
    let faucet = signer_from_environment(&args.users.faucet_keypair_env)?;
    let mint_authority = signer_from_environment(&args.users.mint_authority_keypair_env)?;
    let program_id = args.deployment.program_id()?;
    let whirlpools_config = args.deployment.whirlpools_config()?;
    let tick_spacing = args.deployment.tick_spacing;
    let targets = unordered_pairs(&unique_mints(args.mints, "--mint")?)
        .into_iter()
        .map(|(mint_a, mint_b)| {
            let pool =
                derive_pool_address(program_id, whirlpools_config, mint_a, mint_b, tick_spacing)?;
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
    let chain = connected_chain(&args.deployment.rpc_url, args.transaction.commitment).await?;
    let config = orchestration_config(&args.transaction);
    let ephemeral = EphemeralConfig {
        recovery_dir: args.recovery_dir,
        user_lamports: args.users.user_lamports,
        submission: args.transaction.submission(),
    };
    let runs = run_ephemeral_users(&chain, faucet, &ephemeral, targets, |pool, user| {
        let orchestrator = OrcaWhirlpoolOrchestrator::new(
            chain.clone(),
            program_id,
            user,
            Arc::clone(&mint_authority),
            config,
        );
        let params = UserFlowParams {
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

async fn run_admin(args: AdminArgs) -> Result<()> {
    let program_id = args.deployment.program_id()?;
    let admin = signer_from_environment(&args.admin_keypair_env)?;
    let config_signer = keypair_from_environment(&args.deployment.config_keypair_env)?;
    if let Some(expected) = args.deployment.whirlpools_config {
        anyhow::ensure!(
            expected == config_signer.pubkey(),
            "--whirlpools-config {expected} does not match {}",
            args.deployment.config_keypair_env
        );
    }
    let orchestrator = OrcaWhirlpoolAdminOrchestrator::new(
        connected_chain(&args.deployment.rpc_url, args.commitment).await?,
        program_id,
        admin,
        args.compute_unit_limit,
        args.priority_fee_micro_lamports,
        args.commitment.config(),
    )?;
    let outcome = orchestrator
        .ensure_config(
            &config_signer,
            EnsureConfigParams {
                default_protocol_fee_rate: args.default_protocol_fee_rate,
                tick_spacing: args.deployment.tick_spacing,
                default_fee_rate: args.default_fee_rate,
            },
        )
        .await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "programId": orchestrator.program_id().to_string(),
            "admin": outcome.admin.to_string(),
            "whirlpoolsConfig": outcome.whirlpools_config.to_string(),
            "feeTier": outcome.fee_tier.to_string(),
            "createdConfig": outcome.created_config,
            "createdFeeTier": outcome.created_fee_tier,
            "configTransaction": outcome.config_transaction.as_ref().map(tx_json),
            "feeTierTransaction": outcome.fee_tier_transaction.as_ref().map(tx_json),
        }))?
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

fn pool_creation_json(outcome: &PoolCreationOutcome) -> Value {
    json!({
        "pool": outcome.create_pool.accounts.pool.to_string(),
        "mintA": outcome.create_pool.quote.mint_a.to_string(),
        "mintB": outcome.create_pool.quote.mint_b.to_string(),
        "tokenVaultA": outcome.create_pool.accounts.token_vault_a.to_string(),
        "tokenVaultB": outcome.create_pool.accounts.token_vault_b.to_string(),
        "create": tx_json(&outcome.create_pool.transaction),
        "seededTickArrays": outcome.tick_arrays.initialized.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "tickArrayTransactions": outcome.tick_arrays.transactions.iter().map(tx_json).collect::<Vec<_>>(),
    })
}

fn user_flow_run_json(run: &EphemeralRun<UserPool, UserFlowOutcome>) -> Value {
    json!({
        "pool": run.target.pool.to_string(), "mintA": run.target.mint_a.to_string(), "mintB": run.target.mint_b.to_string(), "user": run.user.to_string(), "startupFunding": submission_json(&run.startup_funding),
        "fundsReturned": run.recovery.json(), "flow": user_flow_json(&run.outcome),
    })
}

fn user_flow_json(outcome: &UserFlowOutcome) -> Value {
    json!({
        "funding": { "transaction": tx_json(&outcome.funding.transaction), "tokenAccountA": outcome.funding.token_account_a.to_string(), "tokenAccountB": outcome.funding.token_account_b.to_string(), "amountARaw": outcome.funding.amount_a_raw, "amountBRaw": outcome.funding.amount_b_raw },
        "position": { "mint": outcome.open_position.accounts.position_mint.to_string(), "position": outcome.open_position.accounts.position.to_string(), "open": tx_json(&outcome.open_position.transaction), "increase": tx_json(&outcome.increase_liquidity.transaction), "decrease": tx_json(&outcome.decrease_liquidity.transaction), "collect": tx_json(&outcome.collect_position.transaction), "close": tx_json(&outcome.close_position.transaction) },
        "swaps": { "exactIn": { "transaction": tx_json(&outcome.swap_exact_in.transaction), "expectedInputRaw": outcome.swap_exact_in.quote.expected_input_raw, "expectedOutputRaw": outcome.swap_exact_in.quote.expected_output_raw }, "exactOut": { "transaction": tx_json(&outcome.swap_exact_out.transaction), "expectedInputRaw": outcome.swap_exact_out.quote.expected_input_raw, "expectedOutputRaw": outcome.swap_exact_out.quote.expected_output_raw } },
    })
}
