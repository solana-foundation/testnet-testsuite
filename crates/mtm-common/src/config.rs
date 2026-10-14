//! Layered config loading shared by every binary:
//! `config/default.toml` <- `config/{MTM_PROFILE}.toml` <- `MTM_*` env vars.
//!
//! Each service defines its own top-level config struct (usually `{ rpc, <service> }`)
//! and calls [`load`]. Env overrides use `__` as the section separator, e.g.
//! `MTM_RPC__HTTP_URL=https://... MTM_PROFILE=testnet cargo run -p oracle`.

use std::path::{Path, PathBuf};

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
    let profile = std::env::var("MTM_PROFILE").ok();
    figment(&config_root(), profile.as_deref())?
        .extract()
        .map_err(Box::new)
}

/// `MTM_PROFILE=a,b` layers b.toml over a.toml (e.g. a scale overlay). A
/// profile that was named but has no file is an error: a typo would
/// otherwise run on the defaults (localhost RPC, base load). Unset, the
/// profile is `local`, and a missing local.toml is fine.
fn figment(root: &Path, profile: Option<&str>) -> Result<Figment, Box<figment::Error>> {
    let mut figment = Figment::new().merge(Toml::file(root.join("default.toml")));
    for name in profile
        .unwrap_or("local")
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        let path = root.join(format!("{name}.toml"));
        if profile.is_some() && !path.is_file() {
            return Err(Box::new(figment::Error::from(format!(
                "MTM_PROFILE names `{name}`, but {} does not exist",
                path.display()
            ))));
        }
        figment = figment.merge(Toml::file(path));
    }
    // The loader's own variables are not config.
    Ok(figment.merge(
        Env::prefixed("MTM_")
            .ignore(&["PROFILE", "CONFIG_DIR"])
            .split("__"),
    ))
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

    #[test]
    fn named_profiles_must_exist_and_layer_in_order() {
        let root = std::env::temp_dir().join(format!("mtm-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("temp dir");
        let write = |name: &str, url: &str| {
            std::fs::write(
                root.join(format!("{name}.toml")),
                format!("[rpc]\nhttp_url = \"{url}\"\nws_url = \"ws://x\"\n"),
            )
            .expect("write profile");
        };
        write("default", "http://default");
        write("base", "http://base");
        write("overlay", "http://overlay");
        let url = |profile: Option<&str>| -> Result<String, Box<figment::Error>> {
            let cfg: TestConfig = figment(&root, profile)?.extract().map_err(Box::new)?;
            Ok(cfg.rpc.http_url)
        };
        assert_eq!(url(Some("base,overlay")).expect("layers"), "http://overlay");
        assert_eq!(url(Some("overlay,base")).expect("layers"), "http://base");
        // unset: `local`, whose file may be missing
        assert_eq!(url(None).expect("defaults"), "http://default");
        let typo = url(Some("base,overlya")).expect_err("missing profile");
        assert!(typo.to_string().contains("overlya"), "{typo}");
        std::fs::remove_dir_all(&root).ok();
    }
}
