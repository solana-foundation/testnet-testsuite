use std::{collections::BTreeSet, env, str::FromStr, sync::Arc};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use mtm_chain::{ChainClient, SubmissionConfig, SubmissionOutcome};
use mtm_telemetry::init;
use raydium_clmm_orchestrator::{
    EnsureAmmConfigParams, OrchestrationConfig, PoolCreationOutcome, PoolCreationParams,
    RaydiumClmmAdminOrchestrator, RaydiumClmmOrchestrator, TransactionOutcome, UserFlowOutcome,
    UserFlowParams, derive_pool_address,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use solana_commitment_config::CommitmentConfig;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;

const TOKEN_AMOUNT: core::ops::Range<usize> = 64..72;
const MINT_DECIMALS: usize = 44;

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
    #[arg(long, default_value = "KEYPAIR_FAUCET")]
    faucet_keypair_env: String,
    #[arg(long, default_value = "KEYPAIR_TOKEN_MINT_AUTHORITY")]
    mint_authority_keypair_env: String,
    /// Lamports sent by the faucet to each ephemeral user before its flow.
    #[arg(long, default_value_t = 1_000_000_000)]
    user_lamports: u64,
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
struct TransactionArgs {
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

struct UserFlowRun {
    pool: UserPool,
    user: Pubkey,
    startup_funding: TransactionOutcome,
    outcome: UserFlowOutcome,
    token_returns: Vec<TransactionOutcome>,
    lamports_returned: u64,
    lamport_return: Option<TransactionOutcome>,
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
    let payer: Arc<dyn Signer + Send + Sync> =
        Arc::new(keypair_from_environment(&args.payer_keypair_env)?);
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

/// Runs one independent random user against every existing unordered mint pair.
async fn execute_user_flows(args: UserFlowArgs) -> Result<()> {
    let faucet: Arc<dyn Signer + Send + Sync> =
        Arc::new(keypair_from_environment(&args.faucet_keypair_env)?);
    let mint_authority: Arc<dyn Signer + Send + Sync> =
        Arc::new(keypair_from_environment(&args.mint_authority_keypair_env)?);
    let program_id = resolve_pubkey(
        args.program_id,
        &args.program_keypair_env,
        "Raydium program",
    )?;
    let pools = unordered_pairs(&unique_mints(args.mints, "--mint")?)
        .into_iter()
        .map(|(mint_a, mint_b)| {
            Ok(UserPool {
                pool: derive_pool_address(program_id, args.amm_config, mint_a, mint_b)?,
                mint_a,
                mint_b,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let chain = connected_chain(&args.rpc_url, args.transaction.commitment).await?;
    let config = orchestration_config(&args.transaction);
    let submission = submission_config(&args.transaction);
    let users: Vec<(UserPool, Arc<dyn Signer + Send + Sync>)> = pools
        .into_iter()
        .map(|pool| {
            (
                pool,
                Arc::new(Keypair::new()) as Arc<dyn Signer + Send + Sync>,
            )
        })
        .collect();
    let mut funded_users: Vec<(UserPool, Arc<dyn Signer + Send + Sync>, TransactionOutcome)> =
        Vec::with_capacity(users.len());
    for (pool, user) in users {
        let user_pubkey = user.pubkey();
        let startup_funding = transfer_lamports(
            &chain,
            faucet.as_ref(),
            faucet.as_ref(),
            user_pubkey,
            args.user_lamports,
            submission,
        )
        .await;
        let startup_funding = match startup_funding {
            Ok(outcome) => outcome,
            Err(error) => {
                for (funded_pool, funded_user, _) in funded_users {
                    let _ = reclaim_user_funds(
                        &chain,
                        faucet.as_ref(),
                        funded_user.as_ref(),
                        [funded_pool.mint_a, funded_pool.mint_b],
                        submission,
                    )
                    .await;
                }
                return Err(error).context("fund ephemeral user from KEYPAIR_FAUCET");
            }
        };
        funded_users.push((pool, user, startup_funding));
    }
    let mut runs = Vec::new();
    for (pool, user, startup_funding) in funded_users {
        let user_pubkey = user.pubkey();
        let orchestrator = RaydiumClmmOrchestrator::new(
            chain.clone(),
            program_id,
            Arc::clone(&user),
            Arc::clone(&mint_authority),
            config,
        )?;
        tracing::info!(pool = %pool.pool, user = %user_pubkey, "starting Raydium CLMM user flow");
        let flow = orchestrator
            .run_user_flow(UserFlowParams {
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
            })
            .await;
        let recovery = reclaim_user_funds(
            &chain,
            faucet.as_ref(),
            user.as_ref(),
            [pool.mint_a, pool.mint_b],
            submission,
        )
        .await;
        let outcome = flow.context("run Raydium CLMM user flow")?;
        let (token_returns, lamports_returned, lamport_return) =
            recovery.context("return ephemeral user funds to KEYPAIR_FAUCET")?;
        runs.push(UserFlowRun {
            pool,
            user: user_pubkey,
            startup_funding,
            outcome,
            token_returns,
            lamports_returned,
            lamport_return,
        });
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&runs.iter().map(user_flow_run_json).collect::<Vec<_>>())?
    );
    Ok(())
}

async fn connected_chain(rpc_url: &str, commitment: Commitment) -> Result<ChainClient> {
    let chain = ChainClient::new(rpc_url, commitment.name())?;
    chain
        .current_slot()
        .await
        .with_context(|| format!("connect to {rpc_url}"))?;
    Ok(chain)
}

fn orchestration_config(args: &TransactionArgs) -> OrchestrationConfig {
    OrchestrationConfig {
        slippage_bps: args.slippage_bps,
        compute_unit_limit: args.compute_unit_limit,
        priority_fee_micro_lamports: args.priority_fee_micro_lamports,
        confirmation_commitment: args.commitment.config(),
    }
}

fn submission_config(args: &TransactionArgs) -> SubmissionConfig {
    SubmissionConfig {
        compute_unit_limit: args.compute_unit_limit,
        compute_unit_price_micro_lamports: args.priority_fee_micro_lamports,
        commitment: args.commitment.config(),
        ..SubmissionConfig::default()
    }
}

async fn reclaim_user_funds(
    chain: &ChainClient,
    faucet: &dyn Signer,
    user: &dyn Signer,
    mints: [Pubkey; 2],
    submission: SubmissionConfig,
) -> Result<(Vec<TransactionOutcome>, u64, Option<TransactionOutcome>)> {
    let mut token_returns = Vec::new();
    for mint in mints {
        if let Some(transaction) =
            return_token_balance(chain, faucet, user, mint, submission).await?
        {
            token_returns.push(transaction);
        }
    }
    let lamports = chain
        .rpc()
        .get_balance(&user.pubkey())
        .await
        .context("read ephemeral user balance before recovery")?;
    let transaction = if lamports == 0 {
        None
    } else {
        Some(transfer_lamports(chain, faucet, user, faucet.pubkey(), lamports, submission).await?)
    };
    Ok((token_returns, lamports, transaction))
}

async fn return_token_balance(
    chain: &ChainClient,
    faucet: &dyn Signer,
    user: &dyn Signer,
    mint: Pubkey,
    submission: SubmissionConfig,
) -> Result<Option<TransactionOutcome>> {
    let mint_account = chain
        .rpc()
        .get_account(&mint)
        .await
        .with_context(|| format!("read mint {mint} for recovery"))?;
    let token_program = mint_account.owner;
    let source = associated_token_address(user.pubkey(), mint, token_program)?;
    let Some(source_account) = chain
        .rpc()
        .get_account_with_commitment(&source, submission.commitment)
        .await
        .context("read ephemeral user token balance")?
        .value
    else {
        return Ok(None);
    };
    let amount = token_account_amount(&source_account.data, source)?;
    let destination = associated_token_address(faucet.pubkey(), mint, token_program)?;
    let decimals = *mint_account
        .data
        .get(MINT_DECIMALS)
        .with_context(|| format!("mint {mint} is too short to contain decimals"))?;
    let mut instructions = vec![create_ata_instruction(
        faucet.pubkey(),
        faucet.pubkey(),
        mint,
        token_program,
        destination,
    )?];
    if amount > 0 {
        instructions.push(transfer_checked_instruction(
            source,
            mint,
            destination,
            user.pubkey(),
            token_program,
            amount,
            decimals,
        ));
    }
    // Closing the now-empty ATA returns its rent to the faucet as well as its
    // token balance, so the generated user does not retain stranded lamports.
    instructions.push(close_token_account_instruction(
        source,
        faucet.pubkey(),
        user.pubkey(),
        token_program,
    ));
    let outcome = chain
        .submit(instructions, faucet, &[user], submission)
        .await?;
    Ok(Some(transaction_outcome(outcome)))
}

async fn transfer_lamports(
    chain: &ChainClient,
    fee_payer: &dyn Signer,
    source: &dyn Signer,
    destination: Pubkey,
    lamports: u64,
    submission: SubmissionConfig,
) -> Result<TransactionOutcome> {
    if lamports == 0 {
        bail!("lamport transfer amount must be greater than zero")
    }
    Ok(transaction_outcome(
        chain
            .submit(
                vec![system_transfer_instruction(
                    source.pubkey(),
                    destination,
                    lamports,
                )],
                fee_payer,
                &[source],
                submission,
            )
            .await?,
    ))
}

fn transaction_outcome(outcome: SubmissionOutcome) -> TransactionOutcome {
    TransactionOutcome {
        signature: outcome.signature,
        confirmation_slot: outcome.confirmation_slot,
        simulated_compute_units: outcome.simulated_compute_units,
    }
}

fn system_transfer_instruction(source: Pubkey, destination: Pubkey, lamports: u64) -> Instruction {
    let mut data = 2_u32.to_le_bytes().to_vec();
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction {
        program_id: Pubkey::default(),
        accounts: vec![
            AccountMeta::new(source, true),
            AccountMeta::new(destination, false),
        ],
        data,
    }
}

fn associated_token_address(owner: Pubkey, mint: Pubkey, token_program: Pubkey) -> Result<Pubkey> {
    let associated_program = pubkey("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL")?;
    Ok(Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &associated_program,
    )
    .0)
}

fn create_ata_instruction(
    payer: Pubkey,
    owner: Pubkey,
    mint: Pubkey,
    token_program: Pubkey,
    ata: Pubkey,
) -> Result<Instruction> {
    Ok(Instruction {
        program_id: pubkey("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL")?,
        accounts: vec![
            AccountMeta::new(payer, true),
            AccountMeta::new(ata, false),
            AccountMeta::new_readonly(owner, false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(Pubkey::default(), false),
            AccountMeta::new_readonly(token_program, false),
        ],
        data: vec![1],
    })
}

fn transfer_checked_instruction(
    source: Pubkey,
    mint: Pubkey,
    destination: Pubkey,
    authority: Pubkey,
    token_program: Pubkey,
    amount: u64,
    decimals: u8,
) -> Instruction {
    let mut data = vec![12];
    data.extend_from_slice(&amount.to_le_bytes());
    data.push(decimals);
    Instruction {
        program_id: token_program,
        accounts: vec![
            AccountMeta::new(source, false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new(destination, false),
            AccountMeta::new_readonly(authority, true),
        ],
        data,
    }
}

fn close_token_account_instruction(
    account: Pubkey,
    destination: Pubkey,
    authority: Pubkey,
    token_program: Pubkey,
) -> Instruction {
    Instruction {
        program_id: token_program,
        accounts: vec![
            AccountMeta::new(account, false),
            AccountMeta::new(destination, false),
            AccountMeta::new_readonly(authority, true),
        ],
        data: vec![9],
    }
}

fn token_account_amount(data: &[u8], address: Pubkey) -> Result<u64> {
    let amount = data
        .get(TOKEN_AMOUNT)
        .with_context(|| format!("token account {address} is too short to contain a balance"))?;
    let bytes: [u8; 8] = amount
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid token balance length"))?;
    Ok(u64::from_le_bytes(bytes))
}

fn unique_mints(mints: Vec<Pubkey>, source: &str) -> Result<Vec<Pubkey>> {
    let mints: BTreeSet<Pubkey> = mints.into_iter().collect();
    if mints.len() < 2 {
        bail!("{source} must provide at least two distinct mints")
    }
    Ok(mints.into_iter().collect())
}

fn unordered_pairs(mints: &[Pubkey]) -> Vec<(Pubkey, Pubkey)> {
    let mut pairs = Vec::new();
    for (i, mint_a) in mints.iter().enumerate() {
        for mint_b in &mints[i + 1..] {
            pairs.push((*mint_a, *mint_b));
        }
    }
    pairs
}

async fn run_admin(args: AdminArgs) -> Result<()> {
    let program_id = resolve_pubkey(
        args.program_id,
        &args.program_keypair_env,
        "Raydium program",
    )?;
    let admin: Arc<dyn Signer + Send + Sync> =
        Arc::new(keypair_from_environment(&args.admin_keypair_env)?);
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

fn resolve_pubkey(value: Option<Pubkey>, keypair_env: &str, label: &str) -> Result<Pubkey> {
    match value {
        Some(pubkey) => Ok(pubkey),
        None => keypair_from_environment(keypair_env)
            .map(|keypair| keypair.pubkey())
            .with_context(|| format!("derive {label} address from {keypair_env}")),
    }
}

fn keypair_from_environment(name: &str) -> Result<Keypair> {
    keypair_from_encoded(
        &env::var(name).with_context(|| format!("environment variable {name} is unset"))?,
    )
    .with_context(|| format!("{name} is not a valid keypair"))
}

fn keypair_from_encoded(encoded: &str) -> Result<Keypair> {
    let bytes: Vec<u8> =
        serde_json::from_str(encoded).context("keypair must be a JSON-encoded byte array")?;
    if bytes.len() != 64 {
        bail!("keypair must contain exactly 64 byte values")
    }
    Keypair::try_from(bytes.as_slice()).context("invalid keypair bytes")
}

fn pubkey(value: &str) -> Result<Pubkey> {
    Pubkey::from_str(value).with_context(|| format!("invalid public key {value}"))
}

fn transaction_json(transaction: &TransactionOutcome) -> Value {
    json!({ "signature": transaction.signature.to_string(), "confirmationSlot": transaction.confirmation_slot, "simulatedComputeUnits": transaction.simulated_compute_units })
}

fn pool_creation_json(outcome: &PoolCreationOutcome) -> Value {
    json!({ "pool": outcome.create_pool.accounts.pool.to_string(), "mint0": outcome.create_pool.quote.mint_0.to_string(), "mint1": outcome.create_pool.quote.mint_1.to_string(), "create": transaction_json(&outcome.create_pool.transaction) })
}

fn user_flow_run_json(run: &UserFlowRun) -> Value {
    json!({
        "pool": run.pool.pool.to_string(), "mintA": run.pool.mint_a.to_string(), "mintB": run.pool.mint_b.to_string(), "user": run.user.to_string(), "startupFunding": transaction_json(&run.startup_funding),
        "fundsReturned": { "lamports": run.lamports_returned, "lamportTransaction": run.lamport_return.as_ref().map(transaction_json), "tokenTransactions": run.token_returns.iter().map(transaction_json).collect::<Vec<_>>() }, "flow": user_flow_json(&run.outcome),
    })
}

fn user_flow_json(outcome: &UserFlowOutcome) -> Value {
    json!({
        "funding": { "transaction": transaction_json(&outcome.funding.transaction), "tokenAccountA": outcome.funding.token_account_a.to_string(), "tokenAccountB": outcome.funding.token_account_b.to_string(), "amountARaw": outcome.funding.amount_a_raw, "amountBRaw": outcome.funding.amount_b_raw },
        "position": { "nftMint": outcome.open_position.accounts.position_nft_mint.to_string(), "personalPosition": outcome.open_position.accounts.personal_position.to_string(), "open": transaction_json(&outcome.open_position.transaction), "increase": transaction_json(&outcome.increase_liquidity.transaction), "decrease": transaction_json(&outcome.decrease_liquidity.transaction), "collect": transaction_json(&outcome.collect_position.transaction), "close": transaction_json(&outcome.close_position.transaction) },
        "swaps": { "exactIn": { "transaction": transaction_json(&outcome.swap_exact_in.transaction), "expectedInputRaw": outcome.swap_exact_in.quote.expected_input_raw, "expectedOutputRaw": outcome.swap_exact_in.quote.expected_output_raw }, "exactOut": { "transaction": transaction_json(&outcome.swap_exact_out.transaction), "expectedInputRaw": outcome.swap_exact_out.quote.expected_input_raw, "expectedOutputRaw": outcome.swap_exact_out.quote.expected_output_raw } },
    })
}
