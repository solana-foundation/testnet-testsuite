//! Service config, loaded via mtm-common's layered loader:
//! `config/default.toml` <- `config/{MTM_PROFILE}.toml` <- `MTM_*` env vars.

use std::net::SocketAddr;

use mtm_common::Symbol;
pub use mtm_common::config::RpcConfig;
use serde::Deserialize;

use crate::pricing::spec::InstrumentConfig;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub rpc: RpcConfig,
    pub oracle: OracleConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OracleConfig {
    pub listen_addr: SocketAddr,
    pub metrics_addr: Option<SocketAddr>,
    pub hermes: HermesConfig,
    #[serde(default)]
    pub coingecko: CoinGeckoConfig,
    #[serde(default)]
    pub binance: BinanceConfig,
    pub underlyings: Vec<UnderlyingConfig>,
    #[serde(default)]
    pub instruments: Vec<InstrumentConfig>,
    #[serde(default)]
    pub pusher: PusherConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HermesConfig {
    pub base_url: String,
    pub poll_interval_ms: u64,
    #[serde(default)]
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UnderlyingConfig {
    pub symbol: Symbol,
    #[serde(default)]
    pub pyth_feed_id: Option<String>,
    #[serde(default)]
    pub coingecko_id: Option<String>,
    #[serde(default)]
    pub binance_symbol: Option<String>,
}

#[derive(Debug, Default)]
pub struct PartitionedFeeds {
    pub pyth: ProviderFeeds,
    pub coingecko: ProviderFeeds,
    pub binance: ProviderFeeds,
}

pub type ProviderFeeds = Vec<(Symbol, String)>;

pub fn partition_underlyings(underlyings: &[UnderlyingConfig]) -> anyhow::Result<PartitionedFeeds> {
    let mut out = PartitionedFeeds::default();
    for u in underlyings {
        match (&u.pyth_feed_id, &u.coingecko_id, &u.binance_symbol) {
            (Some(id), None, None) => out.pyth.push((u.symbol.clone(), id.clone())),
            (None, Some(id), None) => out.coingecko.push((u.symbol.clone(), id.clone())),
            (None, None, Some(id)) => out.binance.push((u.symbol.clone(), id.clone())),
            _ => anyhow::bail!(
                "underlying {}: set exactly one of pyth_feed_id | coingecko_id | binance_symbol",
                u.symbol
            ),
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CoinGeckoConfig {
    pub base_url: String,
    pub poll_interval_ms: u64,
    pub api_key: Option<String>,
}

impl Default for CoinGeckoConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.coingecko.com/api/v3".to_string(),
            poll_interval_ms: 300_000,
            api_key: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BinanceConfig {
    pub ws_url: String,
    pub flush_interval_ms: u64,
}

impl Default for BinanceConfig {
    fn default() -> Self {
        Self {
            ws_url: "wss://stream.binance.com:9443".to_string(),
            flush_interval_ms: 250,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PusherConfig {
    #[serde(default)]
    pub enabled: bool,
    pub keypair_path: Option<String>,
}

#[cfg(test)]
mod tests {
    use figment::Figment;
    use figment::providers::{Format, Toml};

    use super::*;

    #[test]
    fn parses_full_config_with_default_commitment() {
        let cfg: AppConfig = Figment::new()
            .merge(Toml::string(
                r#"
                [rpc]
                http_url = "http://localhost:8899"
                ws_url = "ws://localhost:8900"

                [oracle]
                listen_addr = "127.0.0.1:8080"

                [oracle.hermes]
                base_url = "https://hermes.pyth.network"
                poll_interval_ms = 1000

                [[oracle.underlyings]]
                symbol = "SOL/USD"
                pyth_feed_id = "ef0d"

                [[oracle.instruments]]
                symbol = "tSOL/USD"
                base = { kind = "underlying", feed = "SOL/USD" }

                [[oracle.instruments]]
                mint = "7pMcAg9x3GJqUxWZcntjZiy5UJPXfPZFoVwuCPCBpMcx"
                base = { kind = "gbm", initial = "0.02", daily_vol_bps = 1500, seed = 42 }
                "#,
            ))
            .extract()
            .expect("config should parse");
        assert_eq!(cfg.rpc.commitment, "confirmed");
        assert!(!cfg.oracle.pusher.enabled);
        assert_eq!(cfg.oracle.instruments.len(), 2);
        assert_eq!(
            cfg.oracle.instruments[1]
                .resolved_symbol()
                .expect("mint fallback")
                .as_str(),
            "7pMcAg9x3GJqUxWZcntjZiy5UJPXfPZFoVwuCPCBpMcx"
        );
    }
}
