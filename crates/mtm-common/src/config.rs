//! Layered config loading shared by every binary:
//! `config/default.toml` <- `config/{MTM_PROFILE}.toml` <- `MTM_*` env vars.
//!
//! Each service defines its own top-level config struct (usually `{ rpc, <service> }`)
//! and calls [`load`]. Env overrides use `__` as the section separator, e.g.
//! `MTM_RPC__HTTP_URL=https://... MTM_PROFILE=testnet cargo run -p oracle`.

use std::path::PathBuf;

use figment::Figment;
use figment::providers::{Env, Format, Toml};
use serde::Deserialize;

/// Cluster RPC endpoints, present in every service config.
#[derive(Debug, Clone, Deserialize)]
pub struct RpcConfig {
    pub http_url: String,
    pub ws_url: String,
    #[serde(default = "default_commitment")]
    pub commitment: String,
    /// RPC requests one service process keeps in flight at once, the rest
    /// queue. Per process: flow divides it over its shards, every other
    /// service has one client with the whole bound.
    #[serde(default = "default_max_in_flight")]
    pub max_in_flight: usize,
}

fn default_commitment() -> String {
    "confirmed".to_string()
}

/// `mtm_chain::DEFAULT_MAX_IN_FLIGHT`.
fn default_max_in_flight() -> usize {
    128
}

pub fn load<T: for<'de> Deserialize<'de>>() -> Result<T, Box<figment::Error>> {
    let profile = std::env::var("MTM_PROFILE").unwrap_or_else(|_| "local".to_string());
    let root = config_root();
    Figment::new()
        .merge(Toml::file(root.join("default.toml")))
        .merge(Toml::file(root.join(format!("{profile}.toml"))))
        .merge(Env::prefixed("MTM_").split("__"))
        .extract()
        .map_err(Box::new)
}

/// Config directory; override with MTM_CONFIG_DIR when running outside the repo root.
fn config_root() -> PathBuf {
    std::env::var("MTM_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("config"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize)]
    struct TestConfig {
        rpc: RpcConfig,
    }

    #[test]
    fn parses_rpc_section_with_default_commitment() {
        let cfg: TestConfig = Figment::new()
            .merge(Toml::string(
                r#"
                [rpc]
                http_url = "http://localhost:8899"
                ws_url = "ws://localhost:8900"
                "#,
            ))
            .extract()
            .expect("config should parse");
        assert_eq!(cfg.rpc.commitment, "confirmed");
    }

    fn profile_rpc(profile: &str) -> RpcConfig {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config");
        Figment::new()
            .merge(Toml::file(root.join("default.toml")))
            .merge(Toml::file(root.join(format!("{profile}.toml"))))
            .extract::<TestConfig>()
            .unwrap_or_else(|error| panic!("{profile}: {error}"))
            .rpc
    }

    #[test]
    fn shared_rpc_profiles_bound_requests_below_the_default() {
        // The public testnet endpoint allows ~40 connections per IP for
        // every service on the host together.
        for profile in ["testnet", "scenario-testnet"] {
            let rpc = profile_rpc(profile);
            assert!(rpc.http_url.contains("testnet"), "{profile}");
            assert!(
                (1..=10).contains(&rpc.max_in_flight),
                "{profile}: {}",
                rpc.max_in_flight
            );
        }
        assert_eq!(profile_rpc("local").max_in_flight, default_max_in_flight());
    }
}
