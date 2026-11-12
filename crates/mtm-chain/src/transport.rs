//! Request bounds. A client has two permit pools: one its callers share and
//! a small one for its confirmer, so status polls never queue behind sends.
//!
//! Over HTTP a permit covers one HTTP exchange (request and body), taken
//! in a middleware under [`HttpSender`]: `HttpSender` sleeps between its
//! own HTTP 429 retries (500 ms, or `Retry-After`) with no permit held.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use reqwest_middleware::reqwest::{self, Request, Response};
use reqwest_middleware::{ClientBuilder, Middleware, Next};
use solana_rpc_client::http_sender::HttpSender;
use solana_rpc_client::rpc_sender::{RpcSender, RpcTransportStats};
use solana_rpc_client_api::client_error::Result as ClientResult;
use solana_rpc_client_api::request::RpcRequest;
use tokio::sync::Semaphore;

/// Requests the confirmer has in flight at once (outside
/// [`ChainOptions::max_in_flight`](crate::ChainOptions::max_in_flight)).
pub(crate) const CONFIRMER_MAX_IN_FLIGHT: usize = 4;

/// `HttpSender`'s own default.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A shared [`RpcSender`]; with `permits`, every request waits for one first
/// (senders whose permits are not taken per HTTP attempt underneath).
#[derive(Clone)]
pub(crate) struct Transport {
    inner: Arc<dyn RpcSender + Send + Sync>,
    permits: Option<Arc<Semaphore>>,
}

impl Transport {
    #[cfg(test)]
    pub fn limited(inner: Arc<dyn RpcSender + Send + Sync>, permits: usize) -> Self {
        Self {
            inner,
            permits: Some(Arc::new(Semaphore::new(permits.max(1)))),
        }
    }

    /// The callers' and the confirmer's senders over one HTTP connection
    /// pool, each bounded per HTTP attempt.
    pub fn http(
        url: String,
        request_timeout: Option<Duration>,
        max_in_flight: usize,
    ) -> anyhow::Result<(Self, Self)> {
        let timeout = request_timeout.unwrap_or(DEFAULT_REQUEST_TIMEOUT);
        let client = reqwest::Client::builder()
            .default_headers(HttpSender::default_headers())
            .timeout(timeout)
            .pool_idle_timeout(timeout)
            .build()?;
        let sender = |permits: usize| Self {
            inner: Arc::new(HttpSender::new_with_client_with_middleware(
                url.clone(),
                ClientBuilder::new(client.clone())
                    .with(PerAttempt {
                        permits: Arc::new(Semaphore::new(permits.max(1))),
                    })
                    .build(),
            )),
            permits: None,
        };
        Ok((sender(max_in_flight), sender(CONFIRMER_MAX_IN_FLIGHT)))
    }

    /// The same sender with its own pool of `permits` (for the confirmer).
    #[cfg(test)]
    pub fn sibling(&self, permits: usize) -> Self {
        Self::limited(Arc::clone(&self.inner), permits)
    }
}

impl RpcSender for Transport {
    // `RpcSender` is an `async_trait`; this is its expanded signature.
    fn send<'life0, 'async_trait>(
        &'life0 self,
        request: RpcRequest,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = ClientResult<serde_json::Value>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let _permit = match &self.permits {
                Some(permits) => Some(
                    permits
                        .acquire()
                        .await
                        .expect("the RPC semaphore is never closed"),
                ),
                None => None,
            };
            self.inner.send(request, params).await
        })
    }

    fn get_transport_stats(&self) -> RpcTransportStats {
        self.inner.get_transport_stats()
    }

    fn url(&self) -> String {
        self.inner.url()
    }
}

/// Holds a permit for one HTTP exchange, body included (it is read here, so
/// the connection is free when the permit goes).
struct PerAttempt {
    permits: Arc<Semaphore>,
}

impl Middleware for PerAttempt {
    // `Middleware` is an `async_trait`; this is its expanded signature.
    fn handle<'life0, 'life1, 'life2, 'async_trait>(
        &'life0 self,
        request: Request,
        extensions: &'life1 mut http::Extensions,
        next: Next<'life2>,
    ) -> Pin<Box<dyn Future<Output = reqwest_middleware::Result<Response>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let _permit = self
                .permits
                .acquire()
                .await
                .expect("the RPC semaphore is never closed");
            let response = next.run(request, extensions).await?;
            let mut buffered = http::Response::builder()
                .status(response.status())
                .version(response.version());
            if let Some(headers) = buffered.headers_mut() {
                *headers = response.headers().clone();
            }
            let url = response.url().clone();
            let body = response.bytes().await?;
            let buffered = reqwest::ResponseBuilderExt::url(buffered, url)
                .body(body)
                .map_err(|error| reqwest_middleware::Error::Middleware(error.into()))?;
            Ok(Response::from(buffered))
        })
    }
}
