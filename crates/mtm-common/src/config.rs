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
}

fn default_commitment() -> String {
    "confirmed".to_string()
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
}
