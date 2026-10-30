//! Raw JSON-RPC with timing and error metrics per method. The URL carries
//! the provider's API key, so it is never logged: only [`host`] is.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use metrics::{counter, histogram};
use serde_json::{Value, json};

const ATTEMPTS: u32 = 4;

#[derive(Clone)]
pub struct JsonRpc {
    client: reqwest::Client,
    url: String,
    budget: Arc<Budget>,
}

/// A request budget shared by every task using the client: calls (retries
/// included) are spaced `interval` apart (zero: unlimited), and a 429's
/// `Retry-After` pauses all of them rather than each retrying on its own.
struct Budget {
    interval: Duration,
    next: tokio::sync::Mutex<tokio::time::Instant>,
}

impl Budget {
    async fn take(&self) {
        let at = {
            let mut next = self.next.lock().await;
            let at = (*next).max(tokio::time::Instant::now());
            *next = at + self.interval;
            at
        };
        tokio::time::sleep_until(at).await;
    }

    async fn pause(&self, pause: Duration) {
        let mut next = self.next.lock().await;
        *next = (*next).max(tokio::time::Instant::now() + pause);
    }
}

enum Failure {
    /// HTTP 429, with the server's `Retry-After` when it sent one.
    RateLimited(Option<Duration>),
    Other(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for Failure {
    fn from(error: E) -> Self {
        Failure::Other(error.into())
    }
}

/// A JSON-RPC error object.
#[derive(Debug, thiserror::Error)]
#[error("RPC error {code}: {message}")]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl JsonRpc {
    /// `max_rps`: at most this many requests per second (None: unlimited).
    pub fn new(url: impl Into<String>, max_rps: Option<f64>) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()?,
            url: url.into(),
            budget: Arc::new(Budget {
                interval: max_rps
                    .filter(|r| *r > 0.0)
                    .map_or(Duration::ZERO, |rps| Duration::from_secs_f64(1.0 / rps)),
                next: tokio::sync::Mutex::new(tokio::time::Instant::now()),
            }),
        })
    }

    /// The call's `result`; an error object comes back as an [`RpcError`]
    /// (downcast to inspect the code) and is not retried. Transport errors,
    /// 429 (after its `Retry-After`, for every caller) and 5xx are retried
    /// with backoff.
    pub async fn call(&self, method: &'static str, params: Value) -> Result<Value> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let mut attempt = 0;
        loop {
            attempt += 1;
            self.budget.take().await;
            let started = Instant::now();
            let outcome = async {
                let response = self.client.post(&self.url).json(&body).send().await?;
                let status = response.status();
                if status.as_u16() == 429 {
                    let retry_after = response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .map(Duration::from_secs);
                    return Err(Failure::RateLimited(retry_after));
                }
                if status.is_server_error() {
                    return Err(Failure::Other(anyhow!("HTTP {status}")));
                }
                Ok::<_, Failure>(response.error_for_status()?.json::<Value>().await?)
            }
            .await;
            histogram!("chain_rpc_seconds", "method" => method)
                .record(started.elapsed().as_secs_f64());
            match outcome {
                Ok(mut response) => {
                    if let Some(error) = response.get("error") {
                        counter!("chain_rpc_errors_total", "method" => method, "kind" => "rpc")
                            .increment(1);
                        return Err(RpcError {
                            code: error["code"].as_i64().unwrap_or_default(),
                            message: error["message"].as_str().unwrap_or_default().to_owned(),
                        }
                        .into());
                    }
                    return Ok(response["result"].take());
                }
                Err(Failure::RateLimited(retry_after)) => {
                    counter!("chain_rpc_errors_total", "method" => method, "kind" => "rate_limited")
                        .increment(1);
                    let pause = retry_after.unwrap_or(Duration::from_secs(1));
                    self.budget.pause(pause).await;
                    if attempt >= ATTEMPTS {
                        return Err(anyhow!("HTTP 429 Too Many Requests").context(method));
                    }
                    tracing::debug!(
                        method,
                        attempt,
                        ?pause,
                        "rate limited; pausing every caller"
                    );
                }
                Err(Failure::Other(error)) => {
                    // Strip the URL (it carries the key) from reqwest errors.
                    let error = anyhow!("{}", redact(&format!("{error:#}"), &self.url));
                    counter!("chain_rpc_errors_total", "method" => method, "kind" => "transport")
                        .increment(1);
                    if attempt >= ATTEMPTS {
                        return Err(error.context(method));
                    }
                    tracing::debug!(method, attempt, error = %error, "RPC retry");
                    tokio::time::sleep(Duration::from_millis(250 * 2_u64.pow(attempt - 1))).await;
                }
            }
        }
    }
}

/// `https://key@host:port/path?api-key=...` → `host:port`.
pub fn host(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?']).next().unwrap_or(rest);
    authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host)
        .to_owned()
}

/// Replace every occurrence of `url` (and its query string) with its host.
pub fn redact(text: &str, url: &str) -> String {
    let mut out = text.replace(url, &host(url));
    if let Some((_, query)) = url.split_once('?') {
        out = out.replace(query, "<redacted>");
    }
    out
}

/// `getBlock` error codes that mean the slot has no block: skipped, or
/// missing from the ledger / long-term storage.
pub fn is_skipped(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<RpcError>()
        .is_some_and(|e| matches!(e.code, -32007 | -32009))
}

/// `getBlock` errors that may succeed later (block not yet available).
pub fn is_not_yet(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<RpcError>()
        .is_some_and(|e| matches!(e.code, -32004 | -32014))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn budget_spaces_requests() {
        let budget = Budget {
            interval: Duration::from_millis(250),
            next: tokio::sync::Mutex::new(tokio::time::Instant::now()),
        };
        let start = tokio::time::Instant::now();
        for _ in 0..5 {
            budget.take().await;
        }
        // Four gaps of 250 ms after the first immediate call.
        assert_eq!(start.elapsed(), Duration::from_secs(1));
        // A Retry-After holds back the next caller too.
        budget.pause(Duration::from_secs(10)).await;
        let paused = tokio::time::Instant::now();
        budget.take().await;
        assert_eq!(paused.elapsed(), Duration::from_secs(10));
    }

    #[test]
    fn host_hides_keys() {
        assert_eq!(
            host("https://testnet.helius-rpc.com/?api-key=secret"),
            "testnet.helius-rpc.com"
        );
        assert_eq!(host("http://127.0.0.1:8899"), "127.0.0.1:8899");
        assert_eq!(host("https://user:pw@rpc.example/v1/key"), "rpc.example");
        let url = "https://testnet.helius-rpc.com/?api-key=secret";
        let text = format!("error sending request for url ({url})");
        assert!(!redact(&text, url).contains("secret"));
    }
}
