//! Check a rendered overlay against the types the services deserialize it
//! into (`mtm_scenario::config`), including their own range checks, so a
//! spec that renders is one flow, mm and arb will start with.

use anyhow::{Context, Result};
use mtm_scenario::config::{ArbBotConfig, FlowConfig, MakerConfig, PoolConfig, TokenConfig};
use serde::de::DeserializeOwned;

fn section<T: DeserializeOwned>(scenario: &toml::Table, key: &str) -> Result<Option<T>> {
    scenario
        .get(key)
        .map(|value| {
            value
                .clone()
                .try_into()
                .with_context(|| format!("scenario.{key}"))
        })
        .transpose()
}

pub fn overlay(text: &str) -> Result<()> {
    let root: toml::Table = text.parse().context("overlay is not valid TOML")?;
    let scenario = root
        .get("scenario")
        .and_then(|v| v.as_table())
        .context("overlay has no [scenario]")?;
    let pools: Vec<PoolConfig> = section(scenario, "pools")?.context("no scenario.pools")?;
    let makers: Vec<MakerConfig> = section(scenario, "makers")?.unwrap_or_default();
    for maker in &makers {
        maker.validate()?;
    }
    let flow: FlowConfig = section(scenario, "flow")?.context("no scenario.flow")?;
    flow.validate()?;
    let tokens: Option<Vec<TokenConfig>> = section(scenario, "tokens")?;
    if let Some(arb) = scenario.get("arb").and_then(|v| v.as_table()) {
        let bots: Vec<ArbBotConfig> = section(arb, "bots")?.unwrap_or_default();
        mtm_scenario::config::ArbConfig {
            bots,
            poll_ms: 5_000,
            debounce_ms: 250,
        }
        .validate()?;
    }
    if let Some(tokens) = tokens {
        let known = |name: &str| tokens.iter().any(|t| t.name == name);
        for pool in &pools {
            anyhow::ensure!(
                known(&pool.base) && known(&pool.quote),
                "pool {}/{} uses an unlisted token",
                pool.base,
                pool.quote
            );
        }
    }
    let names: std::collections::HashSet<&str> = makers.iter().map(|m| m.name.as_str()).collect();
    anyhow::ensure!(
        names.len() == makers.len(),
        "two makers share a name (it derives the wallet)"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::{presets, render};

    #[test]
    fn every_preset_passes_the_services_checks() {
        let prices = crate::render::tests::prices();
        for (name, spec) in presets() {
            let out = render::overlay(name, &spec, &prices).expect("render");
            super::overlay(&out.toml).unwrap_or_else(|e| panic!("{name}: {e:#}"));
        }
    }
}
