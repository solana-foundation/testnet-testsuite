use std::env;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use mtm_chain::ChainClient;
use mtm_telemetry::init;
use raydium_clmm_orchestrator::{
    EnsureAmmConfigParams, FullFlowOutcome, FullFlowParams, OrchestrationConfig,
    RaydiumClmmAdminOrchestrator, RaydiumClmmOrchestrator, TransactionOutcome,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use solana_commitment_config::CommitmentConfig;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Commitment {
    Processed,
    Confirmed,
    Finalized,
}

impl Commitment {
    fn config(self) -> CommitmentConfig {
        match self {
            Self::Processed => CommitmentConfig::processed(),
            Self::Confirmed => CommitmentConfig::confirmed(),
            Self::Finalized => CommitmentConfig::finalized(),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Processed => "processed",
            Self::Confirmed => "confirmed",
            Self::Finalized => "finalized",
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "raydium-clmm-orchestrator",
    version,
    about = "Configure and exercise a Raydium CLMM deployment"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Execute a complete pool and position lifecycle.
    Flow(Box<FlowArgs>),
    /// Create or validate a canonical AMM config.
    Admin(AdminArgs),
}

#[derive(Debug, Args)]
struct FlowArgs {
    #[arg(long, default_value = "http://localhost:8899")]
    rpc_url: String,

    /// Raydium program address. When omitted, it is derived from the injected keypair.
    #[arg(long)]
    program_id: Option<Pubkey>,

    #[arg(long, default_value = "KEYPAIR_RAYDIUM_CLMM")]
    program_keypair_env: String,

    #[arg(long)]
    amm_config: Pubkey,

    /// First fungible mint. When omitted, it is derived from the injected keypair.
    #[arg(long)]
    mint_a: Option<Pubkey>,

    #[arg(long, default_value = "KEYPAIR_TOKEN_USDC_MINT")]
    mint_a_keypair_env: String,

    /// Second fungible mint. When omitted, it is derived from the injected keypair.
    #[arg(long)]
    mint_b: Option<Pubkey>,

    #[arg(long, default_value = "KEYPAIR_TOKEN_RAY_MINT")]
    mint_b_keypair_env: String,

    #[arg(long, default_value = "KEYPAIR_FAUCET")]
    payer_keypair_env: String,

    #[arg(long, default_value = "KEYPAIR_TOKEN_MINT_AUTHORITY")]
    mint_authority_keypair_env: String,

    #[arg(long, default_value = "10000")]
    funding_a: Decimal,

    #[arg(long, default_value = "10000")]
    funding_b: Decimal,

    /// Initial price expressed as mint B per mint A.
    #[arg(long, default_value = "1")]
    initial_price: Decimal,

    /// Lower position price expressed as mint B per mint A.
    #[arg(long, default_value = "0.5")]
    lower_price: Decimal,

    /// Upper position price expressed as mint B per mint A.
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

    #[arg(long)]
    open_time: Option<u64>,

    #[arg(long, default_value_t = false)]
    with_metadata: bool,

    #[arg(long, default_value_t = 50)]
    slippage_bps: u16,

    #[arg(long, default_value_t = 1_400_000)]
    compute_unit_limit: u32,

    #[arg(long, default_value_t = 0)]
    priority_fee_micro_lamports: u64,

    #[arg(long, value_enum, default_value_t = Commitment::Confirmed)]
    commitment: Commitment,
}

#[derive(Debug, Args)]
struct AdminArgs {
    #[arg(long, default_value = "http://localhost:8899")]
    rpc_url: String,

    /// Program address. When omitted, derive it from the injected program keypair.
    #[arg(long)]
    program_id: Option<Pubkey>,

    #[arg(long, default_value = "KEYPAIR_RAYDIUM_CLMM")]
    program_keypair_env: String,

    /// Must match the admin public key compiled into the deployed program.
    #[arg(long, default_value = "KEYPAIR_PROGRAM_AUTHORITY")]
    admin_keypair_env: String,

    #[arg(long, default_value_t = 0)]
    config_index: u16,

    #[arg(long, default_value_t = 10)]
    tick_spacing: u16,

    /// Trade fee rate with denominator 1,000,000; 500 is 0.05%.
    #[arg(long, default_value_t = 500)]
    trade_fee_rate: u32,

    /// Share of trade fees assigned to the protocol, denominator 1,000,000.
    #[arg(long, default_value_t = 120_000)]
    protocol_fee_rate: u32,

    /// Share of trade fees assigned to the fund owner, denominator 1,000,000.
    #[arg(long, default_value_t = 0)]
    fund_fee_rate: u32,

    #[arg(long, default_value_t = 400_000)]
    compute_unit_limit: u32,

    #[arg(long, default_value_t = 0)]
    priority_fee_micro_lamports: u64,

    #[arg(long, value_enum, default_value_t = Commitment::Confirmed)]
    commitment: Commitment,
}

#[tokio::main]
async fn main() -> Result<()> {
    init("raydium-clmm-orchestrator", None)?;
    match Cli::parse().command {
        Command::Flow(args) => run_flow(*args).await,
        Command::Admin(args) => run_admin(args).await,
    }
}

async fn run_flow(args: FlowArgs) -> Result<()> {
    let payer: Arc<dyn Signer + Send + Sync> =
        Arc::new(keypair_from_environment(&args.payer_keypair_env)?);
    let mint_authority: Arc<dyn Signer + Send + Sync> =
        Arc::new(keypair_from_environment(&args.mint_authority_keypair_env)?);
    let program_id = resolve_pubkey(
        args.program_id,
        &args.program_keypair_env,
        "Raydium program",
    )?;
    let mint_a = resolve_pubkey(args.mint_a, &args.mint_a_keypair_env, "mint A")?;
    let mint_b = resolve_pubkey(args.mint_b, &args.mint_b_keypair_env, "mint B")?;
    let commitment = args.commitment.config();
    let chain = ChainClient::new(&args.rpc_url, args.commitment.name())?;
    chain
        .current_slot()
        .await
        .with_context(|| format!("connect to {}", args.rpc_url))?;
    let orchestrator = RaydiumClmmOrchestrator::new(
        chain,
        program_id,
        payer,
        mint_authority,
        OrchestrationConfig {
            slippage_bps: args.slippage_bps,
            compute_unit_limit: args.compute_unit_limit,
            priority_fee_micro_lamports: args.priority_fee_micro_lamports,
            confirmation_commitment: commitment,
        },
    )?;
    tracing::info!(%program_id, %mint_a, %mint_b, payer = %orchestrator.payer(), "starting Raydium CLMM flow");
    let outcome = orchestrator
        .run_full_flow(FullFlowParams {
            amm_config: args.amm_config,
            mint_a,
            mint_b,
            funding_a: args.funding_a,
            funding_b: args.funding_b,
            initial_price: args.initial_price,
            lower_price: args.lower_price,
            upper_price: args.upper_price,
            open_amount: args.open_amount,
            increase_amount: args.increase_amount,
            exact_in_amount: args.exact_in_amount,
            exact_out_amount: args.exact_out_amount,
            open_time: args.open_time,
            with_metadata: args.with_metadata,
        })
        .await?;
    println!("{}", serde_json::to_string_pretty(&outcome_json(&outcome))?);
    Ok(())
}

async fn run_admin(args: AdminArgs) -> Result<()> {
    let program_id = resolve_pubkey(
        args.program_id,
        &args.program_keypair_env,
        "Raydium program",
    )?;
    let admin: Arc<dyn Signer + Send + Sync> =
        Arc::new(keypair_from_environment(&args.admin_keypair_env)?);
    let chain = ChainClient::new(&args.rpc_url, args.commitment.name())?;
    chain
        .current_slot()
        .await
        .with_context(|| format!("connect to {}", args.rpc_url))?;
    let orchestrator = RaydiumClmmAdminOrchestrator::new(
        chain,
        program_id,
        admin,
        args.compute_unit_limit,
        args.priority_fee_micro_lamports,
        args.commitment.config(),
    )?;
    tracing::info!(
        %program_id,
        admin = %orchestrator.admin(),
        config_index = args.config_index,
        "ensuring Raydium AMM config"
    );
    let outcome = orchestrator
        .ensure_amm_config(EnsureAmmConfigParams {
            index: args.config_index,
            tick_spacing: args.tick_spacing,
            trade_fee_rate: args.trade_fee_rate,
            protocol_fee_rate: args.protocol_fee_rate,
            fund_fee_rate: args.fund_fee_rate,
        })
        .await?;
    let transaction = outcome.transaction.as_ref().map(transaction_json);
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "programId": orchestrator.program_id().to_string(),
            "admin": outcome.admin.to_string(),
            "ammConfig": outcome.amm_config.to_string(),
            "created": outcome.created,
            "index": outcome.params.index,
            "tickSpacing": outcome.params.tick_spacing,
            "tradeFeeRate": outcome.params.trade_fee_rate,
            "protocolFeeRate": outcome.params.protocol_fee_rate,
            "fundFeeRate": outcome.params.fund_fee_rate,
            "transaction": transaction,
        }))?
    );
    Ok(())
}

fn resolve_pubkey(value: Option<Pubkey>, keypair_env: &str, label: &str) -> Result<Pubkey> {
    match value {
        Some(pubkey) => Ok(pubkey),
        None => keypair_from_environment(keypair_env)
            .map(|keypair| keypair.pubkey())
            .with_context(|| format!("derive {label} address from {keypair_env}")),
    }
}

fn keypair_from_environment(name: &str) -> Result<Keypair> {
    let encoded =
        env::var(name).with_context(|| format!("environment variable {name} is unset"))?;
    let bytes: Vec<u8> = serde_json::from_str(&encoded)
        .with_context(|| format!("{name} must be a JSON-encoded keypair byte array"))?;
    if bytes.len() != 64 {
        bail!("{name} must contain exactly 64 byte values");
    }
    Keypair::try_from(bytes.as_slice()).with_context(|| format!("{name} is not a valid keypair"))
}

fn transaction_json(transaction: &TransactionOutcome) -> Value {
    json!({
        "signature": transaction.signature.to_string(),
        "confirmationSlot": transaction.confirmation_slot,
        "simulatedComputeUnits": transaction.simulated_compute_units,
    })
}

fn outcome_json(outcome: &FullFlowOutcome) -> Value {
    json!({
        "funding": {
            "transaction": transaction_json(&outcome.funding.transaction),
            "tokenAccountA": outcome.funding.token_account_a.to_string(),
            "tokenAccountB": outcome.funding.token_account_b.to_string(),
            "amountARaw": outcome.funding.amount_a_raw,
            "amountBRaw": outcome.funding.amount_b_raw,
        },
        "pool": {
            "address": outcome.create_pool.accounts.pool.to_string(),
            "mint0": outcome.create_pool.quote.mint_0.to_string(),
            "mint1": outcome.create_pool.quote.mint_1.to_string(),
            "create": transaction_json(&outcome.create_pool.transaction),
        },
        "position": {
            "nftMint": outcome.open_position.accounts.position_nft_mint.to_string(),
            "personalPosition": outcome.open_position.accounts.personal_position.to_string(),
            "open": transaction_json(&outcome.open_position.transaction),
            "increase": transaction_json(&outcome.increase_liquidity.transaction),
            "decrease": transaction_json(&outcome.decrease_liquidity.transaction),
            "collect": transaction_json(&outcome.collect_position.transaction),
            "close": transaction_json(&outcome.close_position.transaction),
        },
        "swaps": {
            "exactIn": {
                "transaction": transaction_json(&outcome.swap_exact_in.transaction),
                "expectedInputRaw": outcome.swap_exact_in.quote.expected_input_raw,
                "expectedOutputRaw": outcome.swap_exact_in.quote.expected_output_raw,
            },
            "exactOut": {
                "transaction": transaction_json(&outcome.swap_exact_out.transaction),
                "expectedInputRaw": outcome.swap_exact_out.quote.expected_input_raw,
                "expectedOutputRaw": outcome.swap_exact_out.quote.expected_output_raw,
            },
        },
    })
}
