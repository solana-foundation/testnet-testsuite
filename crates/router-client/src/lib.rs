//! Typed client for the mtm router service — owns the API wire types
//! (docs/router-architecture.md §1). Addresses travel as base58 strings so
//! this crate stays dependency-light for external consumers.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwapMode {
    ExactIn,
    ExactOut,
}

/// How the route executes on-chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Execution {
    /// Each leg is a top-level instruction of its venue program. Multi-hop
    /// legs chain conservatively: hop N+1 spends hop N's on-chain minimum.
    #[default]
    Direct,
    /// One `route_v2` instruction of the configured Jupiter fork, which
    /// forwards each hop's actual output into the next on-chain.
    Jupiter,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuoteRequest {
    pub input_mint: String,
    pub output_mint: String,
    /// atoms of input (ExactIn) or output (ExactOut)
    pub amount: u64,
    #[serde(default = "default_mode")]
    pub mode: SwapMode,
    #[serde(default = "default_slippage")]
    pub slippage_bps: u16,
    #[serde(default)]
    pub execution: Execution,
    /// when present, the response includes an unsigned transaction for this payer
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

fn default_mode() -> SwapMode {
    SwapMode::ExactIn
}
fn default_slippage() -> u16 {
    50
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteLeg {
    /// venue kind: "raydium-clmm", "orca-whirlpool", or "manifest"
    pub venue: String,
    pub pool: String,
    pub input_mint: String,
    pub output_mint: String,
    pub in_amount: u64,
    pub out_amount: u64,
    /// fees charged by the venue, in input atoms
    pub fee_amount: u64,
    pub price_impact_bps: u32,
    /// compute units this leg's swap is expected to consume
    pub cu_estimate: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuoteResponse {
    pub execution: Execution,
    pub in_amount: u64,
    pub out_amount: u64,
    /// slippage-adjusted floor enforced on-chain by the final leg
    pub min_out_amount: u64,
    pub price_impact_bps: u32,
    pub route: Vec<RouteLeg>,
    /// oldest state slot among quoted venues
    pub state_slot: u64,
    /// compute units the route is expected to consume: the legs' estimates
    /// plus aggregator overhead for `jupiter` execution (no margin)
    pub cu_estimate: u32,
    /// CU limit written into the returned transaction's v1 config, or
    /// recommended for /v1/instructions callers (estimate + the router's
    /// margin, capped at 1.4M); present when `user` was given
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compute_unit_limit: Option<u32>,
    /// unsigned v1 transaction, wincode wire format, base64 (present when
    /// `user` was given); the compute budget is in the message config
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx_base64: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireAccountMeta {
    pub pubkey: String,
    pub is_signer: bool,
    pub is_writable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireInstruction {
    pub program_id: String,
    pub accounts: Vec<WireAccountMeta>,
    pub data_base64: String,
}

/// Response of /v1/instructions: the quote plus raw instructions for callers
/// composing their own transaction. The compute budget is NOT included: put
/// the quote's `compute_unit_limit` (or `cu_estimate` plus your own margin)
/// in your transaction's v1 config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstructionsResponse {
    #[serde(flatten)]
    pub quote: QuoteResponse,
    pub instructions: Vec<WireInstruction>,
}

#[derive(Debug, thiserror::Error)]
pub enum RouterClientError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
}

#[derive(Debug, Clone)]
pub struct RouterClient {
    http: reqwest::Client,
    base_url: String,
}

impl RouterClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }

    pub async fn quote(&self, request: &QuoteRequest) -> Result<QuoteResponse, RouterClientError> {
        let resp = self
            .http
            .post(format!("{}/v1/quote", self.base_url))
            .json(request)
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.json().await?)
    }

    /// Raw instructions for composing into your own transaction. `user` is
    /// required on the request.
    pub async fn instructions(
        &self,
        request: &QuoteRequest,
    ) -> Result<InstructionsResponse, RouterClientError> {
        let resp = self
            .http
            .post(format!("{}/v1/instructions", self.base_url))
            .json(request)
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.json().await?)
    }
}
