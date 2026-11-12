//! Service config, loaded via mtm-common's layered loader:
//! `config/default.toml` <- `config/{MTM_PROFILE}.toml` <- `MTM_*` env vars.

use std::net::SocketAddr;
use std::path::PathBuf;

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
    /// `/ready` answers 503 while any registered instrument's latest point
    /// was published longer ago than this. Instruments fed by a slower
    /// cadence (CoinGecko polls, slow GBM clocks) get twice that cadence.
    #[serde(default = "default_ready_max_age_ms")]
    pub ready_max_age_ms: u64,
    /// Where synthetic generator state (GBM price + RNG position, rebased
    /// basket factors) is saved and restored across restarts. Unset: every
    /// start re-anchors at the configured `initial` / `rebase`.
    #[serde(default)]
    pub state_path: Option<PathBuf>,
}

fn default_ready_max_age_ms() -> u64 {
    15_000
}

impl OracleConfig {
    /// Startup checks serde can't express. Every interval must be positive
    /// (`tokio::time::interval(0)` panics inside the task that builds it).
    pub fn validate(&self) -> anyhow::Result<()> {
        let b = &self.binance;
        for (key, value) in [
            (
                "oracle.hermes.poll_interval_ms",
                self.hermes.poll_interval_ms,
            ),
            (
                "oracle.coingecko.poll_interval_ms",
                self.coingecko.poll_interval_ms,
            ),
            ("oracle.binance.flush_interval_ms", b.flush_interval_ms),
            ("oracle.binance.idle_timeout_ms", b.idle_timeout_ms),
            ("oracle.binance.ping_interval_ms", b.ping_interval_ms),
            ("oracle.binance.confirm_interval_ms", b.confirm_interval_ms),
            (
                "oracle.binance.confirm_max_silence_ms",
                b.confirm_max_silence_ms,
            ),
            ("oracle.ready_max_age_ms", self.ready_max_age_ms),
        ] {
            if value == 0 {
                anyhow::bail!("{key} must be > 0");
            }
        }
        // A quiet but healthy connection proves itself once per ping round
        // trip: a confirm window no longer than the ping interval would stop
        // confirming between pongs.
        if b.confirm_max_silence_ms <= b.ping_interval_ms {
            anyhow::bail!(
                "oracle.binance.confirm_max_silence_ms ({}) must exceed ping_interval_ms ({})",
                b.confirm_max_silence_ms,
                b.ping_interval_ms
            );
        }
        Ok(())
    }
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
    /// REST host for the book snapshot that seeds each new connection
    /// (bookTicker streams only changes).
    pub rest_url: String,
    pub flush_interval_ms: u64,
    /// Reconnect when the connection has shown no sign of life (no frame,
    /// no pong to our pings) for this long: a stalled connection can stay
    /// open for minutes before TCP gives up.
    pub idle_timeout_ms: u64,
    /// Client ping cadence: proves a quiet connection is alive.
    pub ping_interval_ms: u64,
    /// While the connection is alive, re-publish each symbol's unchanged book
    /// this often: bookTicker only sends changes, so a thin book would
    /// otherwise age past consumers' staleness limits while still current.
    pub confirm_interval_ms: u64,
    /// Re-confirm only while the connection showed life within this window
    /// (about two ping round trips), far tighter than `idle_timeout_ms`: a
    /// stalled connection stops being vouched for within this bound, which
    /// is what consumers' staleness limits stack on top of.
    pub confirm_max_silence_ms: u64,
}

impl Default for BinanceConfig {
    fn default() -> Self {
        Self {
            ws_url: "wss://stream.binance.com:9443".to_string(),
            rest_url: "https://api.binance.com".to_string(),
            flush_interval_ms: 250,
            idle_timeout_ms: 15_000,
            ping_interval_ms: 5_000,
            confirm_interval_ms: 1_000,
            confirm_max_silence_ms: 10_000,
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
        assert_eq!(cfg.oracle.ready_max_age_ms, 15_000);
        cfg.oracle.validate().expect("defaults are valid");
    }

    fn minimal() -> OracleConfig {
        Figment::new()
            .merge(Toml::string(
                r#"
                listen_addr = "127.0.0.1:8080"
                underlyings = []
                [hermes]
                base_url = "https://hermes.pyth.network"
                poll_interval_ms = 1000
                "#,
            ))
            .extract()
            .expect("config should parse")
    }

    #[test]
    fn rejects_zero_intervals() {
        type Zero = fn(&mut OracleConfig);
        let zeroes: [(&str, Zero); 8] = [
            ("hermes.poll", |c| c.hermes.poll_interval_ms = 0),
            ("coingecko.poll", |c| c.coingecko.poll_interval_ms = 0),
            ("binance.flush", |c| c.binance.flush_interval_ms = 0),
            ("binance.idle", |c| c.binance.idle_timeout_ms = 0),
            ("binance.ping", |c| c.binance.ping_interval_ms = 0),
            ("binance.confirm", |c| c.binance.confirm_interval_ms = 0),
            ("binance.silence", |c| c.binance.confirm_max_silence_ms = 0),
            ("ready", |c| c.ready_max_age_ms = 0),
        ];
        for (name, zero) in zeroes {
            let mut cfg = minimal();
            zero(&mut cfg);
            let err = cfg.validate().expect_err(name).to_string();
            assert!(err.contains("must be > 0"), "{name}: {err}");
        }
    }

    #[test]
    fn rejects_a_confirm_window_inside_one_ping() {
        let mut cfg = minimal();
        cfg.binance.confirm_max_silence_ms = cfg.binance.ping_interval_ms;
        assert!(cfg.validate().is_err());
    }
}
