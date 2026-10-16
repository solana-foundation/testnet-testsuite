//! Feature gates as the cluster has them: every account owned by the
//! Feature program, activated (with its slot) or pending (activates at the
//! next epoch boundary). On-chain state only; descriptions come from the
//! Agave feature set this binary was built with.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use chain_watch_client::{Feature, kind};
use futures::{StreamExt, stream};
use metrics::gauge;
use mtm_common::time::now_us;
use serde_json::json;
use tracing::{info, warn};

use crate::state::Shared;
use crate::store::{self, Write};

pub const FEATURE_PROGRAM: &str = "Feature111111111111111111111111111111111111";

/// Poll at start, every `feature_poll_s`, and at each epoch boundary.
pub async fn poll(shared: Arc<Shared>) {
    let descriptions: HashMap<String, &'static str> = agave_feature_set::FEATURE_NAMES
        .iter()
        .map(|(id, description)| (id.to_string(), *description))
        .collect();
    loop {
        if let Err(error) = poll_once(&shared, &descriptions).await {
            warn!(error = %format!("{error:#}"), "feature poll failed");
        }
        // Activations land on the boundary's first slots: look again shortly after.
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(shared.cfg.feature_poll_s)) => {}
            () = shared.epoch_changed.notified() => {
                tokio::time::sleep(Duration::from_secs(20)).await;
            }
        }
    }
}

async fn poll_once(shared: &Shared, descriptions: &HashMap<String, &'static str>) -> Result<()> {
    let accounts = shared
        .rpc
        .call(
            "getProgramAccounts",
            json!([FEATURE_PROGRAM, { "encoding": "base64", "commitment": "confirmed" }]),
        )
        .await?;
    let mut onchain: Vec<(String, Option<u64>)> = Vec::new();
    for account in accounts.as_array().into_iter().flatten() {
        let Some(id) = account["pubkey"].as_str() else {
            continue;
        };
        let data = account["account"]["data"][0].as_str().unwrap_or_default();
        let data = base64::engine::general_purpose::STANDARD
            .decode(data)
            .with_context(|| format!("feature {id} data"))?;
        onchain.push((id.to_owned(), activated_at(&data)));
    }
    let now = now_us();
    let confirmed = shared.confirmed();
    let slot_seconds = shared.slot_seconds();
    // Block times only for activations not already stored with one.
    let stored: HashMap<String, Feature> = {
        let ids: Vec<String> = onchain.iter().map(|(id, _)| id.clone()).collect();
        shared
            .store
            .read(move |conn| {
                let mut out = HashMap::new();
                for id in ids {
                    if let Some(feature) = store::feature(conn, &id)? {
                        out.insert(id, feature);
                    }
                }
                Ok(out)
            })
            .await?
    };
    let unknown_times: Vec<(String, u64)> = onchain
        .iter()
        .filter_map(|(id, slot)| {
            let slot = (*slot)?;
            let known = stored.get(id).filter(|f| f.activated_slot == Some(slot));
            known.is_none().then(|| (id.clone(), slot))
        })
        .collect();
    let times = stream::iter(unknown_times)
        .map(|(id, slot)| async move {
            let time = block_time_us(shared, slot, confirmed, now, slot_seconds).await;
            (id, time)
        })
        .buffer_unordered(4)
        .collect::<HashMap<_, _>>()
        .await;

    let (mut active, mut pending) = (0, 0);
    for (id, slot) in &onchain {
        let previous = stored.get(id);
        let (activated_time_us, time_estimated) = match (slot, previous) {
            (Some(slot), Some(p)) if p.activated_slot == Some(*slot) => {
                (p.activated_time_us, p.time_estimated)
            }
            (Some(_), _) => times
                .get(id)
                .copied()
                .map_or((None, false), |(t, e)| (Some(t), e)),
            (None, _) => (None, false),
        };
        let feature = Feature {
            id: id.clone(),
            description: descriptions.get(id).map(|d| (*d).to_owned()),
            activated_slot: *slot,
            activated_time_us,
            time_estimated,
            first_seen_pending_us: match (slot, previous) {
                (None, None) => Some(now),
                _ => previous.and_then(|p| p.first_seen_pending_us),
            },
        };
        let label = |f: &Feature| f.description.as_deref().map_or(String::new(), short);
        match (slot, previous) {
            (None, None)
            | (
                None,
                Some(Feature {
                    activated_slot: Some(_),
                    ..
                }),
            ) => {
                info!(feature = %id, description = %label(&feature), "feature pending");
                let detail = json!({ "description": feature.description });
                shared
                    .store
                    .event(now, Some(confirmed), kind::FEATURE_PENDING, id, detail)
                    .await?;
            }
            (
                Some(slot),
                Some(Feature {
                    activated_slot: None,
                    ..
                }),
            ) => {
                info!(feature = %id, slot, description = %label(&feature), "feature activated");
                let detail = json!({
                    "description": feature.description,
                    "activated_slot": slot,
                    "seen_live": true,
                });
                let time = activated_time_us.unwrap_or(now);
                shared
                    .store
                    .event(time, Some(*slot), kind::FEATURE_ACTIVATED, id, detail)
                    .await?;
            }
            _ => {}
        }
        let labels = [("feature", id.clone()), ("description", label(&feature))];
        match slot {
            Some(slot) => {
                active += 1;
                gauge!("chain_feature_activation_slot", &labels).set(*slot as f64);
                gauge!("chain_feature_active", &labels).set(1.0);
            }
            None => {
                pending += 1;
                gauge!("chain_feature_active", &labels).set(0.0);
            }
        }
        shared.store.write(Write::Feature(feature));
    }
    gauge!("chain_features", "state" => "active").set(f64::from(active));
    gauge!("chain_features", "state" => "pending").set(f64::from(pending));
    Ok(())
}

/// Feature account data is a bincode `Option<u64>`: `[1, slot LE]` once
/// activated, `[0]` while pending.
pub fn activated_at(data: &[u8]) -> Option<u64> {
    match data {
        [1, rest @ ..] if rest.len() >= 8 => rest[..8].try_into().ok().map(u64::from_le_bytes),
        _ => None,
    }
}

/// The slot's block time, or an extrapolation from the current slot pace
/// when the RPC no longer has it (marked estimated).
async fn block_time_us(
    shared: &Shared,
    slot: u64,
    confirmed: u64,
    now_us: i64,
    slot_seconds: f64,
) -> (i64, bool) {
    // A public endpoint can't afford ~300 lookups: estimate instead.
    let exact = if shared.public_endpoint {
        None
    } else {
        shared.rpc.call("getBlockTime", json!([slot])).await.ok()
    };
    match exact.as_ref().and_then(serde_json::Value::as_i64) {
        Some(seconds) => (seconds * 1_000_000, false),
        None => {
            let behind = confirmed.saturating_sub(slot) as f64 * slot_seconds;
            (now_us - (behind * 1e6) as i64, true)
        }
    }
}

/// Descriptions run long; labels keep the first 100 characters.
fn short(description: &str) -> String {
    description.chars().take(100).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_feature_accounts() {
        let mut active = vec![1];
        active.extend_from_slice(&42_u64.to_le_bytes());
        assert_eq!(activated_at(&active), Some(42));
        assert_eq!(activated_at(&[0]), None);
        assert_eq!(activated_at(&[0; 9]), None);
        assert_eq!(activated_at(&[]), None);
    }

    #[test]
    fn knows_feature_descriptions() {
        assert!(!agave_feature_set::FEATURE_NAMES.is_empty());
    }
}
