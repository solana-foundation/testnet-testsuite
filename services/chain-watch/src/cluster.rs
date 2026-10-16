//! Who produces blocks: the leader schedule, each node's client and
//! version, and how stake splits across them.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chain_watch_client::kind;
use metrics::gauge;
use mtm_common::time::now_us;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::state::{Node, Shared};
use crate::store::Write;

/// `getSlotLeaders` serves at most 5000 per call.
const LEADER_CHUNK: u64 = 4_000;

/// Make sure the leaders of `from..=to` are known; drop old entries.
pub async fn ensure_leaders(shared: &Shared, from: u64, to: u64) -> Result<()> {
    let missing = {
        let leaders = shared
            .leaders
            .read()
            .map_err(|_| anyhow::anyhow!("leaders poisoned"))?;
        !leaders.contains_key(&from) || !leaders.contains_key(&to)
    };
    if !missing {
        return Ok(());
    }
    let result = shared
        .rpc
        .call("getSlotLeaders", json!([from, LEADER_CHUNK]))
        .await?;
    let mut leaders = shared
        .leaders
        .write()
        .map_err(|_| anyhow::anyhow!("leaders poisoned"))?;
    for (offset, leader) in result.as_array().into_iter().flatten().enumerate() {
        if let Some(leader) = leader.as_str() {
            leaders.insert(from + offset as u64, leader.to_owned());
        }
    }
    let keep = from.saturating_sub(1_000);
    *leaders = leaders.split_off(&keep);
    Ok(())
}

/// Nodes, versions and stake every `cluster_poll_s`.
pub async fn poll_nodes(shared: Arc<Shared>) {
    let mut majority: Option<String> = None;
    // Label values seen before, so ones that vanish are zeroed, not frozen.
    let mut seen: BTreeSet<(&'static str, String)> = BTreeSet::new();
    loop {
        if let Err(error) = poll_once(&shared, &mut majority, &mut seen).await {
            warn!(error = %format!("{error:#}"), "cluster poll failed");
        }
        tokio::time::sleep(Duration::from_secs(shared.cfg.cluster_poll_s)).await;
    }
}

async fn poll_once(
    shared: &Shared,
    majority: &mut Option<String>,
    seen: &mut BTreeSet<(&'static str, String)>,
) -> Result<()> {
    let nodes = shared.rpc.call("getClusterNodes", json!([])).await?;
    let votes = shared
        .rpc
        .call("getVoteAccounts", json!([{ "commitment": "confirmed" }]))
        .await?;
    let nodes = parse_nodes(&nodes);
    let split = stake_split(&nodes, &votes);
    if let Ok(mut shared_nodes) = shared.nodes.write() {
        *shared_nodes = nodes;
    }

    let total = split.total.max(1) as f64;
    let mut now_seen = BTreeSet::new();
    for (client, stake) in &split.by_client {
        gauge!("chain_stake_ratio", "client" => client.clone()).set(*stake as f64 / total);
        now_seen.insert(("client", client.clone()));
    }
    for (version, stake) in &split.by_version {
        gauge!("chain_stake_by_version_ratio", "version" => version.clone())
            .set(*stake as f64 / total);
        now_seen.insert(("version", version.clone()));
    }
    for (client, count) in &split.nodes_by_client {
        gauge!("chain_nodes", "client" => client.clone()).set(*count as f64);
    }
    for (label, value) in seen.difference(&now_seen) {
        match *label {
            "client" => gauge!("chain_stake_ratio", "client" => value.clone()).set(0.0),
            _ => gauge!("chain_stake_by_version_ratio", "version" => value.clone()).set(0.0),
        }
    }
    *seen = now_seen;
    gauge!("chain_delinquent_stake_ratio").set(split.delinquent as f64 / total);

    let time_us = now_us();
    shared
        .store
        .write(Write::Versions(time_us, split.rows.clone()));

    // An event when the stake-majority version changes. The first poll only
    // records it: `replace` returns `None` then.
    let top = split
        .by_version
        .iter()
        .max_by_key(|(_, stake)| **stake)
        .map(|(version, _)| version.clone());
    if let Some(top) = top
        && majority.as_ref() != Some(&top)
        && let Some(previous) = majority.replace(top.clone())
    {
        info!(%previous, current = %top, "stake-majority version changed");
        let stake = split.by_version.get(&top).copied().unwrap_or_default();
        shared
            .store
            .event(
                time_us,
                Some(shared.confirmed()),
                kind::MAJORITY_VERSION,
                &top,
                json!({ "previous": previous, "current": top, "stake_ratio": stake as f64 / total }),
            )
            .await?;
    }
    Ok(())
}

fn parse_nodes(nodes: &Value) -> HashMap<String, Node> {
    nodes
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|node| {
            Some((
                node["pubkey"].as_str()?.to_owned(),
                Node {
                    client: node["clientId"].as_str().unwrap_or("unknown").to_owned(),
                    version: node["version"].as_str().unwrap_or("unknown").to_owned(),
                },
            ))
        })
        .collect()
}

#[derive(Debug, Default, PartialEq)]
struct StakeSplit {
    total: u64,
    delinquent: u64,
    by_client: BTreeMap<String, u64>,
    by_version: BTreeMap<String, u64>,
    nodes_by_client: BTreeMap<String, u64>,
    /// `(client, version, stake, nodes)`
    rows: Vec<(String, String, u64, u64)>,
}

/// Activated stake (current and delinquent) per client and version, keyed
/// by each vote account's node identity.
fn stake_split(nodes: &HashMap<String, Node>, votes: &Value) -> StakeSplit {
    let mut split = StakeSplit::default();
    let mut rows: BTreeMap<(String, String), (u64, u64)> = BTreeMap::new();
    for (list, delinquent) in [("current", false), ("delinquent", true)] {
        for vote in votes[list].as_array().into_iter().flatten() {
            let stake = vote["activatedStake"].as_u64().unwrap_or_default();
            let node = vote["nodePubkey"]
                .as_str()
                .and_then(|identity| nodes.get(identity));
            let (client, version) = node.map_or_else(
                || ("unknown".to_owned(), "unknown".to_owned()),
                |n| (n.client.clone(), n.version.clone()),
            );
            split.total += stake;
            if delinquent {
                split.delinquent += stake;
            }
            *split.by_client.entry(client.clone()).or_default() += stake;
            *split.by_version.entry(version.clone()).or_default() += stake;
            let row = rows.entry((client, version)).or_default();
            row.0 += stake;
            row.1 += 1;
        }
    }
    for node in nodes.values() {
        *split
            .nodes_by_client
            .entry(node.client.clone())
            .or_default() += 1;
    }
    split.rows = rows
        .into_iter()
        .map(|((client, version), (stake, count))| (client, version, stake, count))
        .collect();
    split
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_stake_by_client_and_version() {
        let nodes = parse_nodes(&json!([
            { "pubkey": "A", "clientId": "Agave", "version": "4.3.0" },
            { "pubkey": "F", "clientId": "Firedancer", "version": "0.708.20306" },
            { "pubkey": "G" }
        ]));
        let votes = json!({
            "current": [
                { "nodePubkey": "A", "activatedStake": 60 },
                { "nodePubkey": "F", "activatedStake": 30 }
            ],
            "delinquent": [{ "nodePubkey": "X", "activatedStake": 10 }]
        });
        let split = stake_split(&nodes, &votes);
        assert_eq!(split.total, 100);
        assert_eq!(split.delinquent, 10);
        assert_eq!(split.by_client["Agave"], 60);
        assert_eq!(split.by_client["Firedancer"], 30);
        assert_eq!(split.by_client["unknown"], 10);
        assert_eq!(split.by_version["4.3.0"], 60);
        assert_eq!(split.nodes_by_client["unknown"], 1);
        assert_eq!(split.rows.len(), 3);
    }
}
