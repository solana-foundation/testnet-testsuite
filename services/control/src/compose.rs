//! The Compose panel's backend: presets and chain profiles, estimates, and
//! rendering a spec into `config/scale-<name>.toml` for `up` to layer.

use std::path::Path;

use control_client::{
    ComposeInfo, ComposeRequest, ComposeResponse, EstimateRequest, EstimateResponse, NamedSpec,
};
use scenario_spec::estimate::{Calibration, Chain, estimate, takers_for};
use scenario_spec::{CATALOG, Spec, presets, render};

/// Chain profiles, the one matching `target` first.
pub fn chains(target: &str) -> Vec<Chain> {
    let mut chains = Chain::presets();
    if target == "devnet" {
        chains.insert(
            0,
            Chain {
                name: "devnet".into(),
                ..Chain::testnet()
            },
        );
    } else if let Some(i) = chains
        .iter()
        .position(|c| target == "testnet" && c.name == "testnet")
    {
        let chain = chains.remove(i);
        chains.insert(0, chain);
    }
    chains
}

/// Overlays in `dir` whose second line holds a spec.
pub fn overlays(dir: &Path) -> Vec<NamedSpec> {
    let mut found: Vec<NamedSpec> = crate::processes::scales(dir)
        .into_iter()
        .filter_map(|name| {
            let text = std::fs::read_to_string(dir.join(format!("scale-{name}.toml"))).ok()?;
            Some(NamedSpec {
                spec: render::spec_of(&text)?,
                name,
            })
        })
        .collect();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

pub fn info(dir: &Path, target: &str) -> ComposeInfo {
    ComposeInfo {
        presets: presets()
            .into_iter()
            .map(|(name, spec)| NamedSpec {
                name: name.to_owned(),
                spec,
            })
            .collect(),
        overlays: overlays(dir),
        catalog: CATALOG
            .iter()
            .skip(1)
            .map(|(t, _)| (*t).to_owned())
            .collect(),
        chains: chains(target),
        calibration: Calibration::default(),
    }
}

/// A preset's name for a spec equal to it, else `c-<hash>`.
pub fn name_for(spec: &Spec) -> String {
    presets()
        .into_iter()
        .find(|(_, preset)| preset == spec)
        .map(|(name, _)| name.to_owned())
        .unwrap_or_else(|| spec.custom_name())
}

pub fn estimate_for(request: EstimateRequest) -> EstimateResponse {
    let spec = request.spec.normalized();
    let cal = request.calibration.unwrap_or_default();
    let target_takers = request
        .target_tps
        .and_then(|tps| takers_for(&spec, &request.chain, &cal, tps));
    EstimateResponse {
        error: spec.check().err(),
        estimate: estimate(&spec, &request.chain, &cal),
        name: name_for(&spec),
        target_takers,
    }
}

/// Render and write the overlay; an existing one for the same spec is kept,
/// so a running scenario's overlay never changes under it.
pub async fn compose(
    http: &reqwest::Client,
    dir: &Path,
    request: ComposeRequest,
) -> Result<ComposeResponse, String> {
    let spec = request.spec.normalized();
    spec.check()?;
    let name = name_for(&spec);
    let path = dir.join(format!("scale-{name}.toml"));
    if path.is_file() {
        return Ok(ComposeResponse {
            name,
            created: false,
        });
    }
    let overrides = scenario_spec::prices::overrides_from_env().map_err(|e| format!("{e:#}"))?;
    let prices = scenario_spec::prices::fetch(http, &spec.all_tokens(), &overrides)
        .await
        .map_err(|e| format!("{e:#}"))?;
    let out = render::overlay(&name, &spec, &prices)?;
    scenario_spec::validate::overlay(&out.toml).map_err(|e| format!("rendered overlay: {e:#}"))?;
    // Write then rename, so a concurrent `up` never reads half a file.
    let partial = dir.join(format!(".scale-{name}.toml.partial"));
    std::fs::write(&partial, &out.toml)
        .map_err(|e| format!("writing {}: {e}", partial.display()))?;
    std::fs::rename(&partial, &path).map_err(|e| format!("writing {}: {e}", path.display()))?;
    tracing::info!(name, participants = out.participants, "composed overlay");
    Ok(ComposeResponse {
        name,
        created: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_keep_their_names() {
        let x10 = scenario_spec::preset("x10").expect("preset");
        assert_eq!(name_for(&x10), "x10");
        let mut custom = x10.clone();
        custom.takers.count = 141;
        assert!(name_for(&custom).starts_with("c-"));
    }

    #[test]
    fn the_target_chain_comes_first() {
        assert_eq!(chains("testnet")[0].name, "testnet");
        assert_eq!(chains("devnet")[0].name, "devnet");
        assert!(chains("local")[0].name.starts_with("local"));
    }

    #[test]
    fn committed_overlays_carry_their_spec() {
        let dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../config"));
        let found = overlays(dir);
        for (name, spec) in presets() {
            let overlay = found
                .iter()
                .find(|o| o.name == name)
                .unwrap_or_else(|| panic!("no overlay for {name}"));
            assert_eq!(overlay.spec, spec, "{name}");
        }
    }
}
