//! `scenario overlay` and `scenario estimate`: render scale overlays from a
//! spec (crates/scenario-spec) and estimate the load they offer. Both run
//! offline: no config profile, RPC or secrets.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use scenario_spec::estimate::{Calibration, Chain, estimate, takers_for};
use scenario_spec::{Spec, preset, presets, render};

#[derive(Debug, Args)]
pub struct Source {
    /// A named scale (x10 … x2000, assets).
    #[arg(long, conflicts_with_all = ["spec", "overlay"])]
    preset: Option<String>,
    /// A spec as JSON (the dashboard's Compose panel writes these).
    #[arg(long, conflicts_with = "overlay")]
    spec: Option<PathBuf>,
    /// An overlay generated from a spec (its second line holds the spec).
    #[arg(long)]
    overlay: Option<PathBuf>,
}

impl Source {
    fn load(&self) -> Result<Option<(String, Spec)>> {
        if let Some(name) = &self.preset {
            let spec = preset(name).with_context(|| format!("no preset {name}"))?;
            return Ok(Some((name.clone(), spec)));
        }
        if let Some(path) = &self.spec {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            let spec: Spec = serde_json::from_str(&text)
                .with_context(|| format!("parsing {}", path.display()))?;
            let spec = spec.normalized();
            return Ok(Some((spec.custom_name(), spec)));
        }
        if let Some(path) = &self.overlay {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            let spec = render::spec_of(&text)
                .with_context(|| format!("{} holds no spec", path.display()))?;
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.strip_prefix("scale-"))
                .unwrap_or("overlay")
                .to_owned();
            return Ok(Some((name, spec)));
        }
        Ok(None)
    }
}

#[derive(Debug, Args)]
pub struct OverlayArgs {
    #[command(flatten)]
    source: Source,
    /// Overlay name (`config/scale-<name>.toml`); defaults to the preset's,
    /// or `c-<hash>` for a spec.
    #[arg(long)]
    name: Option<String>,
    #[arg(long, default_value = "config")]
    config_dir: PathBuf,
    /// Token bootstrap config, written when a spec lists tokens beyond JUP and RAY.
    #[arg(long, default_value = "bootstrap/scenario/tokens-assets.json")]
    tokens_config: PathBuf,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ChainArg {
    Local,
    #[value(name = "local-400ms")]
    Local400ms,
    Testnet,
}

#[derive(Debug, Args)]
pub struct EstimateArgs {
    #[command(flatten)]
    source: Source,
    #[arg(long, value_enum, default_value_t = ChainArg::Local)]
    chain: ChainArg,
    /// Also size the takers for this many transactions per second.
    #[arg(long)]
    target_tps: Option<f64>,
    /// Print the estimate as JSON.
    #[arg(long)]
    json: bool,
}

/// Write overlays: the given source, or every preset when none is given.
pub async fn overlay(args: OverlayArgs) -> Result<()> {
    let specs = match args.source.load()? {
        Some((name, spec)) => vec![(args.name.clone().unwrap_or(name), spec)],
        None => presets()
            .into_iter()
            .map(|(n, s)| (n.to_owned(), s))
            .collect(),
    };
    let tokens: Vec<&str> = scenario_spec::CATALOG
        .iter()
        .map(|(t, _)| *t)
        .filter(|t| specs.iter().any(|(_, s)| s.all_tokens().contains(t)))
        .collect();
    let overrides = scenario_spec::prices::overrides_from_env()?;
    let client = reqwest::Client::new();
    let prices = scenario_spec::prices::fetch(&client, &tokens, &overrides).await?;
    for (name, spec) in &specs {
        write_overlay(&args.config_dir, name, spec, &prices)?;
        if spec.redefines_tokens() {
            std::fs::write(&args.tokens_config, render::tokens_config())
                .with_context(|| format!("writing {}", args.tokens_config.display()))?;
        }
    }
    Ok(())
}

pub fn write_overlay(
    dir: &Path,
    name: &str,
    spec: &Spec,
    prices: &render::Prices,
) -> Result<PathBuf> {
    let out = render::overlay(name, spec, prices).map_err(|e| anyhow::anyhow!("{name}: {e}"))?;
    scenario_spec::validate::overlay(&out.toml)
        .with_context(|| format!("{name}: the rendered overlay"))?;
    let path = dir.join(format!("scale-{name}.toml"));
    std::fs::write(&path, &out.toml).with_context(|| format!("writing {}", path.display()))?;
    let e = estimate(spec, &Chain::local(), &Calibration::default());
    eprintln!(
        "scale-{name}: {} takers + {} bots + {} makers = {} participants; {} assets, {} pools; ~{:.0} tx/s on {}",
        spec.takers.count,
        spec.arb.bots,
        e.makers,
        out.participants,
        spec.all_tokens().len(),
        e.pools,
        e.tps_capped,
        e.chain.name
    );
    Ok(path)
}

pub fn estimate_cmd(args: EstimateArgs) -> Result<()> {
    let Some((name, spec)) = args.source.load()? else {
        bail!("give --preset, --spec or --overlay");
    };
    let chain = match args.chain {
        ChainArg::Local => Chain::local(),
        ChainArg::Local400ms => Chain::local_400ms(),
        ChainArg::Testnet => Chain::testnet(),
    };
    let cal = Calibration::default();
    let e = estimate(&spec, &chain, &cal);
    let sized = args
        .target_tps
        .map(|tps| (tps, takers_for(&spec, &chain, &cal, tps)));
    if args.json {
        let mut value = serde_json::to_value(&e)?;
        if let Some((tps, takers)) = sized {
            value["target"] = serde_json::json!({"tps": tps, "takers": takers});
        }
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    println!("{name} on {}", chain.name);
    println!(
        "  participants  {} ({} takers, {} makers, {} bots) over {} pools; maker capital ×{}",
        e.participants, e.takers, e.makers, e.bots, e.pools, e.capital
    );
    println!(
        "  offered       {:.0} swaps/s (takers × trades/min); {:.2} s per trade",
        e.offered_swaps_per_s, e.trade_seconds
    );
    println!(
        "  expected      {:.0} tx/s = {:.0} takers + {:.1} makers + {:.2} arb{}",
        e.tps,
        e.taker_tx_per_s,
        e.maker_tx_per_s,
        e.arb_tx_per_s,
        if e.tps_capped < e.tps {
            format!(" → {:.0} tx/s at the limit", e.tps_capped)
        } else {
            String::new()
        }
    );
    println!(
        "  compute       {:.0}M CU/s, {:.1}M per block",
        e.cu_per_s / 1e6,
        e.cu_per_block / 1e6
    );
    for h in &e.headroom {
        println!(
            "  {:<13} {:>4.0}%  {}",
            format!("{:?}", h.limit).to_lowercase(),
            100.0 * h.utilisation,
            h.detail
        );
    }
    let venues: Vec<String> = e
        .venues
        .iter()
        .map(|v| format!("{} {:.0}%", v.venue.as_str(), 100.0 * v.share))
        .collect();
    println!("  venues        {}", venues.join(", "));
    println!(
        "  bootstrap     ~{:.0} transactions, ~{:.0} min",
        e.bootstrap.transactions,
        e.bootstrap.seconds / 60.0
    );
    println!(
        "  SOL           {:.1}/h fees, {:.1} rent, {:.0} wallet float",
        e.sol.fees_per_hour, e.sol.rent, e.sol.float
    );
    for w in &e.warnings {
        println!("  ! {w}");
    }
    if let Some((tps, takers)) = sized {
        match takers {
            Some(n) => {
                let mut sized = spec.clone();
                sized.takers.count = n;
                let at = estimate(&sized, &chain, &cal);
                println!(
                    "  {tps:.0} tx/s needs {n} takers at {} trades/min",
                    spec.takers.trades_per_min
                );
                if let Some(limit) = at.bottleneck {
                    println!(
                        "    but the {limit:?} limit caps that at ~{:.0} tx/s",
                        at.tps_capped
                    );
                }
            }
            None => println!("  {tps:.0} tx/s: makers and bots alone already send that"),
        }
    }
    Ok(())
}
