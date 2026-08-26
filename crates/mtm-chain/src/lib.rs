//! Off-chain Solana toolkit shared by every service that touches the chain:
//! RPC clients, keypair loading, and (soon) a tx send pipeline with priority
//! fees + retries and account subscription streams (ws now, geyser later).

pub mod keys;

use std::sync::Arc;

use anyhow::Context;
use solana_commitment_config::CommitmentConfig;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;

#[derive(Clone)]
pub struct ChainClient {
    rpc: Arc<RpcClient>,
}

impl ChainClient {
    pub fn new(http_url: impl Into<String>, commitment: &str) -> anyhow::Result<Self> {
        let commitment = parse_commitment(commitment)?;
        Ok(Self {
            rpc: Arc::new(RpcClient::new_with_commitment(http_url.into(), commitment)),
        })
    }

    pub fn rpc(&self) -> &RpcClient {
        &self.rpc
    }

    /// Cheap connectivity check; returns the current slot.
    pub async fn current_slot(&self) -> anyhow::Result<u64> {
        self.rpc.get_slot().await.context("get_slot failed")
    }

    // TODO: send_and_confirm with compute budget / priority fee handling and
    // a retry policy tuned for testnet flakiness.
    // TODO: account subscription trait with pubsub-ws and yellowstone-geyser backends.
}

fn parse_commitment(s: &str) -> anyhow::Result<CommitmentConfig> {
    match s {
        "processed" => Ok(CommitmentConfig::processed()),
        "confirmed" => Ok(CommitmentConfig::confirmed()),
        "finalized" => Ok(CommitmentConfig::finalized()),
        other => anyhow::bail!("unknown commitment level: {other}"),
    }
}
