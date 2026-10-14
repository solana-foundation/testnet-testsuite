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

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
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
    },
}

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
    let rpc_url = cfg.rpc.http_url.clone();
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
        Command::Snapshot { out } => {
            let snapshot = pnl::snapshot(&chain, &scenario).await?;
            std::fs::write(&out, serde_json::to_vec_pretty(&snapshot)?)?;
            json!({ "out": out, "slot": snapshot.slot, "participants": snapshot.participants.len() })
        }
        Command::Report {
            start,
            end,
            prometheus,
        } => {
            let read = |path: &PathBuf| -> Result<pnl::Snapshot> {
                Ok(serde_json::from_slice(
                    &std::fs::read(path).with_context(|| format!("reading {}", path.display()))?,
                )?)
            };
            pnl::report(
                &rpc_url,
                &read(&start)?,
                &read(&end)?,
                prometheus.as_deref(),
            )
            .await?
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

    // 3. wallets
    let mut wallets = Vec::new();
    if !no_fund {
        for maker in &scenario.config.makers {
            let wallet = scenario.maker_wallet(&maker.name).pubkey();
            let tokens = maker
                .inventory
                .iter()
                .map(|(name, amount)| Ok((scenario.token(name)?, *amount)))
                .collect::<Result<Vec<_>>>()?;
            let outcome = top_up(
                chain,
                faucet.as_ref(),
                mint_authority.as_ref(),
                &wallet,
                maker.sol,
                &tokens,
                Funding::Endowment,
            )
            .await?;
            wallets.push(json!({ "role": format!("maker/{}", maker.name), "wallet": wallet.to_string(), "lamportsSent": outcome.lamports_sent, "minted": outcome.minted }));
        }
        for bot in scenario.config.arb.iter().flat_map(|arb| &arb.bots) {
            let wallet = scenario.arb_wallet(&bot.name).pubkey();
            let tokens = bot
                .inventory
                .iter()
                .map(|(name, amount)| Ok((scenario.token(name)?, *amount)))
                .collect::<Result<Vec<_>>>()?;
            let outcome = top_up(
                chain,
                faucet.as_ref(),
                mint_authority.as_ref(),
                &wallet,
                bot.sol,
                &tokens,
                Funding::Endowment,
            )
            .await?;
            wallets.push(json!({ "role": format!("arb/{}", bot.name), "wallet": wallet.to_string(), "lamportsSent": outcome.lamports_sent, "minted": outcome.minted }));
        }
        if let Some(flow) = &scenario.config.flow {
            let tokens = flow
                .inventory
                .iter()
                .map(|(name, amount)| Ok((scenario.token(name)?, *amount)))
                .collect::<Result<Vec<_>>>()?;
            for index in 0..flow.takers {
                let wallet = scenario.taker_wallet(index).pubkey();
                let outcome = top_up(
                    chain,
                    faucet.as_ref(),
                    mint_authority.as_ref(),
                    &wallet,
                    flow.sol,
                    &tokens,
                    Funding::TopUp,
                )
                .await?;
                wallets.push(json!({ "role": format!("taker/{index}"), "wallet": wallet.to_string(), "lamportsSent": outcome.lamports_sent, "minted": outcome.minted }));
            }
        }
    }
    Ok(json!({
        "orcaWhirlpoolsConfig": { "address": orca_config.whirlpools_config.to_string(), "created": orca_config.created_config },
        "raydiumAmmConfig": { "address": raydium_config.amm_config.to_string(), "created": raydium_config.created },
        "pools": pools,
        "wallets": wallets,
    }))
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
    let mut roles: Vec<(String, solana_pubkey::Pubkey)> = scenario
        .config
        .makers
        .iter()
        .map(|maker| {
            (
                format!("maker/{}", maker.name),
                scenario.maker_wallet(&maker.name).pubkey(),
            )
        })
        .collect();
    roles.extend(
        scenario
            .config
            .arb
            .iter()
            .flat_map(|arb| &arb.bots)
            .map(|bot| {
                (
                    format!("arb/{}", bot.name),
                    scenario.arb_wallet(&bot.name).pubkey(),
                )
            }),
    );
    if let Some(flow) = &scenario.config.flow {
        roles.extend((0..flow.takers).map(|index| {
            (
                format!("taker/{index}"),
                scenario.taker_wallet(index).pubkey(),
            )
        }));
    }
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
