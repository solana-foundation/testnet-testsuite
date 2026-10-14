//! Shared scenario plumbing for the bootstrap CLI, the market makers, and the
//! uninformed-flow service: config, address resolution, deterministic
//! wallets, oracle pricing, venue price math, and inventory top-ups.

pub mod config;
pub mod funding;
pub mod pricing;
pub mod venue;

use std::collections::BTreeMap;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
pub use config::*;
use mtm_chain::ChainClient;
use mtm_dex_harness::spl;
use sha2::{Digest, Sha256};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;

/// A token with its on-chain facts.
#[derive(Debug, Clone)]
pub struct Token {
    pub name: String,
    pub mint: Pubkey,
    pub decimals: u8,
    pub token_program: Pubkey,
    pub oracle_symbol: String,
}

#[derive(Debug, Clone, Copy)]
pub struct Programs {
    pub raydium_clmm: Pubkey,
    pub raydium_amm_config: Pubkey,
    pub orca_whirlpool: Pubkey,
    pub orca_whirlpools_config: Pubkey,
    pub orca_tick_spacing: u16,
    pub manifest: Pubkey,
}

impl Programs {
    pub fn of(&self, venue: Venue) -> Pubkey {
        match venue {
            Venue::RaydiumClmm => self.raydium_clmm,
            Venue::OrcaWhirlpool => self.orca_whirlpool,
            Venue::Manifest => self.manifest,
        }
    }
}

/// One scenario wallet and what it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Participant {
    /// `maker/<name>`, `arb/<bot>` or `taker/<index>` (also the wallet label).
    pub role: String,
    pub kind: ParticipantKind,
    pub wallet: Pubkey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ParticipantKind {
    ClobMaker,
    LiquidityProvider,
    OracleArbitrageur,
    CycleArbitrageur,
    Taker,
}

impl ParticipantKind {
    /// Human label used in snapshots and reports.
    pub fn label(self) -> &'static str {
        match self {
            Self::ClobMaker => "market maker (clob)",
            Self::LiquidityProvider => "liquidity provider",
            Self::OracleArbitrageur => "oracle arbitrageur",
            Self::CycleArbitrageur => "cross-venue arbitrage",
            Self::Taker => "uninformed taker",
        }
    }
}

/// Config with every name resolved to an address.
#[derive(Debug, Clone)]
pub struct Scenario {
    pub config: ScenarioConfig,
    pub programs: Programs,
    pub tokens: BTreeMap<String, Token>,
    seed: String,
}

impl Scenario {
    /// Resolve env-named addresses and read mint decimals from chain.
    pub async fn resolve(config: ScenarioConfig, chain: &ChainClient) -> Result<Self> {
        let seed = match std::env::var(&config.seed_env) {
            Ok(seed) if !seed.is_empty() => seed,
            _ => config.dev_seed.clone().with_context(|| {
                format!(
                    "{} is unset and the profile has no dev_seed",
                    config.seed_env
                )
            })?,
        };
        let p = &config.programs;
        let raydium_clmm = pubkey_from_env(&p.raydium_clmm)?;
        let programs = Programs {
            raydium_clmm,
            raydium_amm_config: Pubkey::find_program_address(
                &[b"amm_config", &p.raydium_amm_config_index.to_be_bytes()],
                &raydium_clmm,
            )
            .0,
            orca_whirlpool: pubkey_from_env(&p.orca_whirlpool)?,
            orca_whirlpools_config: pubkey_from_env(&p.orca_whirlpools_config)?,
            orca_tick_spacing: p.orca_tick_spacing,
            manifest: pubkey_from_env(&p.manifest)?,
        };
        let mints: Vec<Pubkey> = config
            .tokens
            .iter()
            .map(|token| pubkey_from_env(&token.mint_env))
            .collect::<Result<_>>()?;
        let accounts = chain
            .rpc()
            .get_multiple_accounts(&mints)
            .await
            .context("reading scenario mints")?;
        let mut tokens = BTreeMap::new();
        for ((token, mint), account) in config.tokens.iter().zip(&mints).zip(accounts) {
            let account =
                account.with_context(|| format!("mint {mint} ({}) not found", token.name))?;
            let summary = spl::decode_mint(account.owner, &account.data)
                .with_context(|| format!("{mint} ({}) is not a token mint", token.name))?;
            tokens.insert(
                token.name.clone(),
                Token {
                    name: token.name.clone(),
                    mint: *mint,
                    decimals: summary.decimals,
                    token_program: summary.token_program,
                    oracle_symbol: token.oracle_symbol.clone(),
                },
            );
        }
        let scenario = Self {
            config,
            programs,
            tokens,
            seed,
        };
        scenario.validate()?;
        Ok(scenario)
    }

    fn validate(&self) -> Result<()> {
        for pool in &self.config.pools {
            self.token(&pool.base)?;
            self.token(&pool.quote)?;
        }
        let mut names = std::collections::HashSet::new();
        for maker in &self.config.makers {
            // The name derives the wallet and the state file: two makers with
            // one name would race on both.
            if !names.insert(maker.name.as_str()) {
                bail!("maker name {} is used twice", maker.name);
            }
            maker.validate()?;
            self.token(&maker.base)?;
            self.token(&maker.quote)?;
            for name in maker.inventory.keys() {
                self.token(name)?;
            }
            let compatible = matches!(
                (&maker.strategy, maker.venue),
                (Strategy::ClobLadder { .. }, Venue::Manifest)
                    | (
                        Strategy::ClmmRange { .. },
                        Venue::RaydiumClmm | Venue::OrcaWhirlpool
                    )
                    | (Strategy::PoolArb { .. }, _)
            );
            if !compatible {
                bail!(
                    "maker {}: strategy does not fit venue {}",
                    maker.name,
                    maker.venue.as_str()
                );
            }
        }
        if let Some(arb) = &self.config.arb {
            arb.validate()?;
            for bot in &arb.bots {
                for name in bot.start_tokens.iter().chain(bot.inventory.keys()) {
                    self.token(name)?;
                }
            }
        }
        if let Some(flow) = &self.config.flow {
            flow.validate()?;
            for (a, b) in &flow.pairs {
                self.token(a)?;
                self.token(b)?;
            }
            for name in flow.inventory.keys() {
                self.token(name)?;
            }
        }
        Ok(())
    }

    pub fn token(&self, name: &str) -> Result<&Token> {
        self.tokens
            .get(name)
            .with_context(|| format!("unknown scenario token {name}"))
    }

    pub fn token_by_mint(&self, mint: &Pubkey) -> Option<&Token> {
        self.tokens.values().find(|token| token.mint == *mint)
    }

    /// Deterministic wallet for a role, e.g. `maker/clob-jup` or `taker/0`.
    pub fn wallet(&self, label: &str) -> Keypair {
        derive_keypair(&self.seed, label)
    }

    pub fn maker_wallet(&self, maker: &str) -> Keypair {
        self.wallet(&format!("maker/{maker}"))
    }

    pub fn arb_wallet(&self, bot: &str) -> Keypair {
        self.wallet(&format!("arb/{bot}"))
    }

    pub fn taker_wallet(&self, index: u32) -> Keypair {
        self.wallet(&format!("taker/{index}"))
    }

    /// Every scenario wallet: makers, then arb bots, then takers. The one
    /// enumeration snapshots, status and teardown share, so a new role
    /// cannot be missed by one of them.
    pub fn participants(&self) -> Vec<Participant> {
        let mut out: Vec<Participant> = self
            .config
            .makers
            .iter()
            .map(|maker| Participant {
                role: format!("maker/{}", maker.name),
                kind: match maker.strategy {
                    Strategy::ClobLadder { .. } => ParticipantKind::ClobMaker,
                    Strategy::ClmmRange { .. } => ParticipantKind::LiquidityProvider,
                    Strategy::PoolArb { .. } => ParticipantKind::OracleArbitrageur,
                },
                wallet: self.maker_wallet(&maker.name).pubkey(),
            })
            .collect();
        if let Some(arb) = &self.config.arb {
            out.extend(arb.bots.iter().map(|bot| Participant {
                role: format!("arb/{}", bot.name),
                kind: ParticipantKind::CycleArbitrageur,
                wallet: self.arb_wallet(&bot.name).pubkey(),
            }));
        }
        if let Some(flow) = &self.config.flow {
            out.extend((0..flow.takers).map(|index| Participant {
                role: format!("taker/{index}"),
                kind: ParticipantKind::Taker,
                wallet: self.taker_wallet(index).pubkey(),
            }));
        }
        out
    }

    /// Address of a configured pool. Manifest markets are keypair accounts,
    /// so they are looked up on chain (oldest first) rather than derived.
    pub async fn pool_address(
        &self,
        chain: &ChainClient,
        venue: Venue,
        base: &str,
        quote: &str,
    ) -> Result<Option<Pubkey>> {
        let (base, quote) = (self.token(base)?.mint, self.token(quote)?.mint);
        Ok(match venue {
            Venue::RaydiumClmm => Some(raydium_clmm_client::derive_pool_address(
                self.programs.raydium_clmm,
                self.programs.raydium_amm_config,
                base,
                quote,
            )?),
            Venue::OrcaWhirlpool => Some(orca_whirlpool_client::derive_pool_address(
                self.programs.orca_whirlpool,
                self.programs.orca_whirlpools_config,
                base,
                quote,
                self.programs.orca_tick_spacing,
            )?),
            Venue::Manifest => {
                manifest_dex_client::find_markets(chain, self.programs.manifest, base, quote)
                    .await?
                    .first()
                    .copied()
            }
        })
    }

    pub fn venue_kind(venue: Venue) -> mtm_venues::VenueKind {
        match venue {
            Venue::RaydiumClmm => mtm_venues::VenueKind::RaydiumClmm,
            Venue::OrcaWhirlpool => mtm_venues::VenueKind::OrcaWhirlpool,
            Venue::Manifest => mtm_venues::VenueKind::Manifest,
        }
    }
}

/// Ed25519 seed = `sha256(secret || "/" || label)`.
pub fn derive_keypair(secret: &str, label: &str) -> Keypair {
    let digest = Sha256::digest(format!("{secret}/{label}").as_bytes());
    let mut seed = [0_u8; 32];
    seed.copy_from_slice(&digest);
    Keypair::new_from_array(seed)
}

/// An env var holding either a base58 public key or a JSON keypair.
pub fn pubkey_from_env(name: &str) -> Result<Pubkey> {
    let value =
        std::env::var(name).with_context(|| format!("environment variable {name} is unset"))?;
    let value = value.trim();
    if let Ok(pubkey) = Pubkey::from_str(value) {
        return Ok(pubkey);
    }
    mtm_dex_harness::cli::keypair_from_encoded(value)
        .map(|keypair| keypair.pubkey())
        .with_context(|| format!("{name} is neither a public key nor a keypair"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wallets_are_deterministic_and_distinct() {
        let a = derive_keypair("seed", "maker/a").pubkey();
        assert_eq!(a, derive_keypair("seed", "maker/a").pubkey());
        assert_ne!(a, derive_keypair("seed", "maker/b").pubkey());
        assert_ne!(a, derive_keypair("other", "maker/a").pubkey());
    }

    #[test]
    fn profiles_parse() {
        use serde::Deserialize;
        #[derive(Deserialize)]
        struct Profile {
            scenario: ScenarioConfig,
        }
        for (name, dev_seed) in [("scenario-local", true), ("scenario-testnet", false)] {
            let path = format!("{}/../../config/{name}.toml", env!("CARGO_MANIFEST_DIR"));
            let text = std::fs::read_to_string(&path).expect("profile exists");
            let profile: Profile = toml::from_str(&text).expect("parses");
            assert_eq!(profile.scenario.makers.len(), 12, "{name}");
            assert!(profile.scenario.flow.is_some(), "{name}");
            assert_eq!(profile.scenario.dev_seed.is_some(), dev_seed, "{name}");
        }
    }
}
