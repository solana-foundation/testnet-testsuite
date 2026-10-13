//! Scenario bootstrap: everything the market-making scenario needs on chain,
//! idempotently, so `bootstrap` can be re-run after any partial failure.
//!
//! 1. Venue configs: Orca WhirlpoolsConfig + fee tier, Raydium AMM config.
//! 2. Pools and markets: each configured pool is created at the live oracle
//!    price if it does not exist (existing ones are left alone).
//! 3. Wallets: every maker and taker (derived from the scenario seed) is
//!    topped up to its configured SOL and token inventory.
//!
//! `snapshot` and `report` account for every participant (see `pnl`).

mod pnl;
mod rpc;
mod teardown;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use futures::StreamExt;
use mtm_chain::ChainClient;
use mtm_common::config::RpcConfig;
use mtm_dex_harness::cli::{keypair_from_environment, signer_from_environment};
use mtm_scenario::funding::{Funding, LAMPORTS_PER_SOL, balances, top_up};
use mtm_scenario::pricing::PriceBook;
use mtm_scenario::venue::to_ui;
use mtm_scenario::{Scenario, ScenarioConfig, Token, Venue};
use oracle_client::OracleClient;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{Value, json};
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use solana_signer::Signer;

#[derive(Debug, Parser)]
#[command(
    name = "scenario",
    about = "Bootstrap and inspect the market-making scenario"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Ensure venue configs, pools, and wallet inventories (idempotent).
    Bootstrap {
        #[arg(long, default_value = "KEYPAIR_FAUCET")]
        faucet_keypair_env: String,
        #[arg(long, default_value = "KEYPAIR_TOKEN_MINT_AUTHORITY")]
        mint_authority_keypair_env: String,
        #[arg(long, default_value = "KEYPAIR_PROGRAM_AUTHORITY")]
        admin_keypair_env: String,
        /// Skip wallet top-ups (pools and configs only).
        #[arg(long)]
        no_fund: bool,
    },
    /// Print every scenario pool and wallet with its balances.
    Status,
    /// Wait until the oracle has a fresh price for every scenario token
    /// (pools are created at oracle prices).
    WaitPrices {
        #[arg(long, default_value_t = 120)]
        timeout_s: u64,
    },
    /// Record every participant's holdings (wallet, seats, LP positions) and
    /// the oracle prices, for `report`.
    Snapshot {
        #[arg(long)]
        out: PathBuf,
    },
    /// PnL, transactions, and trades per participant between two snapshots.
    Report {
        #[arg(long)]
        start: PathBuf,
        #[arg(long)]
        end: PathBuf,
        /// Prometheus scraping the scenario services (for trade counts).
        #[arg(long)]
        prometheus: Option<String>,
        /// Sample per-venue compute from every n-th slot's full block (0: off).
        #[arg(long, default_value_t = 10)]
        compute_sample_every: u64,
    },
    /// Unwind every participant (LP positions, Manifest orders and seats),
    /// burn its scenario tokens, close its token accounts, and sweep its SOL
    /// to the faucet. Stop the services first. Idempotent.
    Teardown {
        #[arg(long, default_value = "KEYPAIR_FAUCET")]
        faucet_keypair_env: String,
        /// Also sweep the operator wallets (`--operator-env`) into the faucet.
        #[arg(long)]
        operators: bool,
        #[arg(long = "operator-env", default_values_t = OPERATOR_ENVS.map(String::from))]
        operator_envs: Vec<String>,
    },
    /// Rent still locked on chain (pools, tick arrays, markets, vaults,
    /// mints, programs) and what the participant / operator wallets hold.
    Leftovers {
        #[arg(long, default_value = "KEYPAIR_FAUCET")]
        faucet_keypair_env: String,
        #[arg(long = "operator-env", default_values_t = OPERATOR_ENVS.map(String::from))]
        operator_envs: Vec<String>,
    },
}

/// Operator keypairs the bootstrap funds (each is skipped when unset).
const OPERATOR_ENVS: [&str; 4] = [
    "KEYPAIR_PROGRAM_AUTHORITY",
    "KEYPAIR_TOKEN_MINT_AUTHORITY",
    "KEYPAIR_TOKEN_FREEZE_AUTHORITY",
    "KEYPAIR_TOKEN_METADATA_AUTHORITY",
];

/// The operator keypairs that are set, labelled by variable.
fn operator_keys(envs: &[String]) -> Vec<(String, solana_keypair::Keypair)> {
    envs.iter()
        .filter_map(|env| Some((env.clone(), keypair_from_environment(env).ok()?)))
        .collect()
}

/// role, wallet, SOL target, token targets, funding mode
type FundingTarget<'s> = (String, Pubkey, Decimal, Vec<(&'s Token, Decimal)>, Funding);

#[derive(Debug, Deserialize)]
struct AppConfig {
    rpc: RpcConfig,
    scenario: ScenarioConfig,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    // Logs go to stderr: stdout carries the JSON report.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    // Parse first: `--help` and argument errors need neither RPC nor secrets.
    let cli = Cli::parse();
    let cfg: AppConfig = mtm_common::config::load().map_err(|error| anyhow::anyhow!("{error}"))?;
    let chain = ChainClient::new(&cfg.rpc.http_url, &cfg.rpc.commitment)?;
    let rpc = rpc::JsonRpc::new(&cfg.rpc.http_url, &cfg.rpc.commitment)?;
    let scenario = Scenario::resolve(cfg.scenario, &chain).await?;
    let report = match cli.command {
        Command::Bootstrap {
            faucet_keypair_env,
            mint_authority_keypair_env,
            admin_keypair_env,
            no_fund,
        } => {
            bootstrap(
                &chain,
                &scenario,
                &faucet_keypair_env,
                &mint_authority_keypair_env,
                &admin_keypair_env,
                no_fund,
            )
            .await?
        }
        Command::Status => status(&chain, &scenario).await?,
        Command::WaitPrices { timeout_s } => wait_prices(&scenario, timeout_s).await?,
        Command::Snapshot { out } => {
            let snapshot = pnl::snapshot(&chain, &rpc, &scenario).await?;
            // Compact: at tens of thousands of participants a pretty
            // snapshot is several MB of whitespace.
            std::fs::write(&out, serde_json::to_vec(&snapshot)?)?;
            json!({ "out": out, "slot": snapshot.slot, "participants": snapshot.participants.len() })
        }
        Command::Report {
            start,
            end,
            prometheus,
            compute_sample_every,
        } => {
            let read = |path: &PathBuf| -> Result<pnl::Snapshot> {
                Ok(serde_json::from_slice(
                    &std::fs::read(path).with_context(|| format!("reading {}", path.display()))?,
                )?)
            };
            pnl::report(
                &rpc,
                &read(&start)?,
                &read(&end)?,
                prometheus.as_deref(),
                &pnl::VenuePrograms::of(&scenario),
                compute_sample_every,
            )
            .await?
        }
        Command::Teardown {
            faucet_keypair_env,
            operators,
            operator_envs,
        } => {
            let faucet = keypair_from_environment(&faucet_keypair_env)?.pubkey();
            let operators = if operators {
                operator_keys(&operator_envs)
                    .into_iter()
                    .filter(|(_, key)| key.pubkey() != faucet)
                    .collect()
            } else {
                Vec::new()
            };
            teardown::teardown(&chain, &scenario, &faucet, operators).await?
        }
        Command::Leftovers {
            faucet_keypair_env,
            operator_envs,
        } => {
            let mut keys = vec![(
                faucet_keypair_env.clone(),
                keypair_from_environment(&faucet_keypair_env)?.pubkey(),
            )];
            keys.extend(
                operator_keys(&operator_envs)
                    .into_iter()
                    .map(|(env, key)| (env, key.pubkey())),
            );
            teardown::leftovers(&chain, &scenario, &keys).await?
        }
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

async fn bootstrap(
    chain: &ChainClient,
    scenario: &Scenario,
    faucet_env: &str,
    mint_authority_env: &str,
    admin_env: &str,
    no_fund: bool,
) -> Result<Value> {
    let faucet = signer_from_environment(faucet_env)?;
    let mint_authority = signer_from_environment(mint_authority_env)?;
    let admin = signer_from_environment(admin_env)?;
    let confirmed = CommitmentConfig::confirmed();
    let programs = scenario.programs;

    // 1. venue configs
    let orca_config_signer =
        keypair_from_environment(&scenario.config.programs.orca_whirlpools_config)
            .context("the Orca config variable must hold the config keypair to create it")?;
    let orca_config = orca_whirlpool_orchestrator::OrcaWhirlpoolAdminOrchestrator::new(
        chain.clone(),
        programs.orca_whirlpool,
        Arc::clone(&admin),
        400_000,
        0,
        confirmed,
    )?
    .ensure_config(
        &orca_config_signer,
        orca_whirlpool_orchestrator::EnsureConfigParams {
            default_protocol_fee_rate: 300,
            tick_spacing: programs.orca_tick_spacing,
            default_fee_rate: 3_000,
        },
    )
    .await
    .context("ensuring the Orca WhirlpoolsConfig")?;
    let raydium_config = raydium_clmm_orchestrator::RaydiumClmmAdminOrchestrator::new(
        chain.clone(),
        programs.raydium_clmm,
        Arc::clone(&admin),
        400_000,
        0,
        confirmed,
    )?
    .ensure_amm_config(raydium_clmm_orchestrator::EnsureAmmConfigParams {
        index: scenario.config.programs.raydium_amm_config_index,
        tick_spacing: 10,
        trade_fee_rate: 500,
        protocol_fee_rate: 120_000,
        fund_fee_rate: 0,
    })
    .await
    .context("ensuring the Raydium AMM config")?;

    // 2. pools at oracle prices
    let oracle = OracleClient::new(&scenario.config.oracle_url);
    let prices = PriceBook::fetch(&oracle, scenario.config.max_price_age_ms).await?;
    let mut pools = Vec::new();
    for pool in &scenario.config.pools {
        let base = scenario.token(&pool.base)?;
        let quote = scenario.token(&pool.quote)?;
        let existing = scenario
            .pool_address(chain, pool.venue, &pool.base, &pool.quote)
            .await?;
        let exists = match existing {
            Some(address) => chain
                .rpc()
                .get_multiple_accounts(&[address])
                .await?
                .into_iter()
                .next()
                .flatten()
                .is_some(),
            None => false,
        };
        // Significant figures, not decimal places: a fixed 6 dp is 2-5% off
        // for a ~2e-5 pair and rounds anything below 5e-7 to zero.
        let price = round_sf(prices.pair(base, quote)?, 8)?;
        let address = if exists {
            existing
        } else {
            tracing::info!(venue = pool.venue.as_str(), base = %pool.base, quote = %pool.quote, %price, "creating pool at oracle price");
            Some(match pool.venue {
                Venue::OrcaWhirlpool => {
                    orca_whirlpool_orchestrator::OrcaWhirlpoolOrchestrator::new(
                        chain.clone(),
                        programs.orca_whirlpool,
                        Arc::clone(&faucet),
                        Arc::clone(&mint_authority),
                        Default::default(),
                    )?
                    .create_pool_flow(orca_whirlpool_orchestrator::PoolCreationParams {
                        whirlpools_config: programs.orca_whirlpools_config,
                        tick_spacing: programs.orca_tick_spacing,
                        mint_a: base.mint,
                        mint_b: quote.mint,
                        initial_price: price,
                        seed_lower_price: round_sf(price / Decimal::from(3), 8)?,
                        seed_upper_price: round_sf(price * Decimal::from(3), 8)?,
                    })
                    .await?
                    .create_pool
                    .accounts
                    .pool
                }
                Venue::RaydiumClmm => {
                    raydium_clmm_orchestrator::RaydiumClmmOrchestrator::new(
                        chain.clone(),
                        programs.raydium_clmm,
                        Arc::clone(&faucet),
                        Arc::clone(&mint_authority),
                        Default::default(),
                    )?
                    .create_pool_flow(raydium_clmm_orchestrator::PoolCreationParams {
                        amm_config: programs.raydium_amm_config,
                        mint_a: base.mint,
                        mint_b: quote.mint,
                        initial_price: price,
                        open_time: None,
                    })
                    .await?
                    .create_pool
                    .accounts
                    .pool
                }
                Venue::Manifest => {
                    manifest_dex_orchestrator::ManifestOrchestrator::new(
                        chain.clone(),
                        programs.manifest,
                        Arc::clone(&faucet),
                        Arc::clone(&mint_authority),
                        Default::default(),
                    )?
                    .create_market_flow(manifest_dex_orchestrator::MarketCreationParams {
                        base_mint: base.mint,
                        quote_mint: quote.mint,
                        allow_duplicate: false,
                    })
                    .await?
                    .market
                }
            })
        };
        pools.push(json!({
            "venue": pool.venue.as_str(), "base": pool.base, "quote": pool.quote,
            "address": address.map(|a| a.to_string()), "created": !exists,
            "oraclePrice": price.to_string(),
        }));
    }

    // 3. wallets: one funding transaction per wallet, sent concurrently
    let mut targets: Vec<FundingTarget<'_>> = Vec::new();
    if !no_fund {
        let resolve = |inventory: &std::collections::BTreeMap<String, Decimal>| {
            inventory
                .iter()
                .map(|(name, amount)| Ok((scenario.token(name)?, *amount)))
                .collect::<Result<Vec<_>>>()
        };
        for maker in &scenario.config.makers {
            targets.push((
                format!("maker/{}", maker.name),
                scenario.maker_wallet(&maker.name).pubkey(),
                maker.sol,
                resolve(&maker.inventory)?,
                Funding::Endowment,
            ));
        }
        for bot in scenario.config.arb.iter().flat_map(|arb| &arb.bots) {
            targets.push((
                format!("arb/{}", bot.name),
                scenario.arb_wallet(&bot.name).pubkey(),
                bot.sol,
                resolve(&bot.inventory)?,
                Funding::Endowment,
            ));
        }
        if let Some(flow) = &scenario.config.flow {
            let tokens = resolve(&flow.inventory)?;
            for index in 0..flow.takers {
                targets.push((
                    format!("taker/{index}"),
                    scenario.taker_wallet(index).pubkey(),
                    flow.sol,
                    tokens.clone(),
                    Funding::TopUp,
                ));
            }
        }
    }
    let concurrency: usize = std::env::var("SCENARIO_FUNDING_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(32);
    let wallets: Vec<Value> = futures::stream::iter(targets.iter().map(
        |(role, wallet, sol, tokens, funding)| {
            let (faucet, mint_authority) = (&faucet, &mint_authority);
            async move {
                let outcome = top_up(
                    chain,
                    faucet.as_ref(),
                    mint_authority.as_ref(),
                    wallet,
                    *sol,
                    tokens,
                    *funding,
                )
                .await?;
                Ok::<_, anyhow::Error>(json!({ "role": role, "wallet": wallet.to_string(), "lamportsSent": outcome.lamports_sent, "minted": outcome.minted }))
            }
        },
    ))
    .buffer_unordered(concurrency)
    .collect::<Vec<_>>()
    .await
    .into_iter()
    .collect::<Result<_>>()?;
    Ok(json!({
        "orcaWhirlpoolsConfig": { "address": orca_config.whirlpools_config.to_string(), "created": orca_config.created_config },
        "raydiumAmmConfig": { "address": raydium_config.amm_config.to_string(), "created": raydium_config.created },
        "pools": pools,
        "wallets": wallets,
    }))
}

async fn wait_prices(scenario: &Scenario, timeout_s: u64) -> Result<Value> {
    let oracle = OracleClient::new(&scenario.config.oracle_url);
    let started = std::time::Instant::now();
    loop {
        let missing: Vec<String> =
            match PriceBook::fetch(&oracle, scenario.config.max_price_age_ms).await {
                Ok(book) => scenario
                    .tokens
                    .values()
                    .filter(|token| book.usd(token).is_err())
                    .map(|token| token.name.clone())
                    .collect(),
                Err(_) => scenario.tokens.keys().cloned().collect(),
            };
        if missing.is_empty() {
            return Ok(
                json!({ "tokens": scenario.tokens.len(), "waitedMs": started.elapsed().as_millis() as u64 }),
            );
        }
        if started.elapsed().as_secs() >= timeout_s {
            anyhow::bail!(
                "no fresh oracle price after {timeout_s} s for: {}",
                missing.join(", ")
            );
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

async fn status(chain: &ChainClient, scenario: &Scenario) -> Result<Value> {
    let mut pools = Vec::new();
    for pool in &scenario.config.pools {
        let address = scenario
            .pool_address(chain, pool.venue, &pool.base, &pool.quote)
            .await?;
        let live = match address {
            Some(address) => chain.rpc().get_multiple_accounts(&[address]).await?[0].is_some(),
            None => false,
        };
        pools.push(json!({ "venue": pool.venue.as_str(), "base": pool.base, "quote": pool.quote, "address": address.map(|a| a.to_string()), "live": live }));
    }
    let roles: Vec<(String, solana_pubkey::Pubkey)> = scenario
        .participants()
        .into_iter()
        .map(|p| (p.role, p.wallet))
        .collect();
    // Batched reads: one round trip per 100 accounts instead of one per
    // (wallet, token).
    let tokens: Vec<&Token> = scenario.tokens.values().collect();
    let pairs: Vec<(solana_pubkey::Pubkey, &Token)> = roles
        .iter()
        .flat_map(|(_, wallet)| tokens.iter().map(|token| (*wallet, *token)))
        .collect();
    let owners: Vec<solana_pubkey::Pubkey> = roles.iter().map(|(_, wallet)| *wallet).collect();
    let (atoms, lamports) = balances(chain, &pairs, &owners).await?;
    let mut wallets = Vec::new();
    for (index, (role, wallet)) in roles.into_iter().enumerate() {
        let mut held = serde_json::Map::new();
        for (token, atoms) in tokens
            .iter()
            .zip(&atoms[index * tokens.len()..(index + 1) * tokens.len()])
        {
            held.insert(token.name.clone(), json!(to_ui(token, *atoms).to_string()));
        }
        held.insert(
            "SOL".into(),
            json!(
                (Decimal::from(lamports[index]) / Decimal::from(LAMPORTS_PER_SOL))
                    .normalize()
                    .to_string()
            ),
        );
        wallets.push(json!({ "role": role, "wallet": wallet.to_string(), "balances": held }));
    }
    Ok(json!({ "pools": pools, "wallets": wallets }))
}

fn round_sf(value: Decimal, digits: u32) -> Result<Decimal> {
    value
        .round_sf(digits)
        .map(|rounded| rounded.normalize())
        .filter(|rounded| !rounded.is_zero())
        .with_context(|| format!("price {value} does not round to a usable value"))
}
