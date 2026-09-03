//! On-chain pusher: lands Hermes binary updates on testnet via the Pyth
//! receiver program.
//!
//! TODO (next milestone):
//!   - build post_update transactions from Hermes `binary.data` blobs
//!     (pyth-solana-receiver-sdk or hand-rolled instructions)
//!   - send with priority fees + a retry policy tuned for testnet flakiness
//!   - meter tx volume/land-rate — this is the testnet traffic generator

use tracing::warn;

use crate::config::{PusherConfig, RpcConfig};

pub async fn run(_cfg: PusherConfig, _rpc: RpcConfig) {
    warn!("pusher enabled but not implemented yet — no transactions will be sent");
}
