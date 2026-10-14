//! Raw JSON-RPC for the calls the typed client does not cover
//! (`getBlock` with account-only details, `getTokenAccountsByOwner` parsed):
//! one HTTP client, the client's commitment, error objects surfaced, and
//! jittered retries on transport errors, rate limits and server errors.

use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

/// Attempts per call before a transport or server error is fatal.
const ATTEMPTS: u32 = 5;

pub struct JsonRpc {
    client: reqwest::Client,
    url: String,
    /// The client's commitment (`processed`, `confirmed` or `finalized`).
    pub commitment: String,
}

/// A JSON-RPC error object.
#[derive(Debug, thiserror::Error)]
#[error("RPC error {code}: {message}")]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl JsonRpc {
    pub fn new(url: impl Into<String>, commitment: impl Into<String>) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()?,
            url: url.into(),
            commitment: commitment.into(),
        })
    }

    /// `getBlock` and friends reject `processed`; use `confirmed` there.
    pub fn block_commitment(&self) -> &str {
        if self.commitment == "processed" {
            "confirmed"
        } else {
            &self.commitment
        }
    }

    /// The call's `result`. An error object comes back as an [`RpcError`]
    /// inside the `anyhow::Error` (downcast to inspect its code).
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let mut attempt = 0;
        loop {
            attempt += 1;
            let outcome = async {
                let response = self.client.post(&self.url).json(&body).send().await?;
                let status = response.status();
                if status.as_u16() == 429 || status.is_server_error() {
                    return Err::<Value, _>(anyhow!("HTTP {status}"));
                }
                Ok(response.error_for_status()?.json::<Value>().await?)
            }
            .await;
            match outcome {
                Ok(mut response) => {
                    if let Some(error) = response.get("error") {
                        return Err(RpcError {
                            code: error["code"].as_i64().unwrap_or_default(),
                            message: error["message"].as_str().unwrap_or_default().to_owned(),
                        }
                        .into());
                    }
                    return Ok(response["result"].take());
                }
                Err(error) if attempt < ATTEMPTS => {
                    tracing::debug!(method, attempt, error = %format!("{error:#}"), "RPC retry");
                    tokio::time::sleep(backoff(attempt)).await;
                }
                Err(error) => {
                    return Err(error.context(format!("{method} failed {ATTEMPTS} times")));
                }
            }
        }
    }
}

/// 200 ms doubling per attempt, plus up to as much again of jitter.
pub fn backoff(attempt: u32) -> Duration {
    let base = 200_u64 << attempt.min(6);
    Duration::from_millis(base + rand::random_range(0..base))
}
