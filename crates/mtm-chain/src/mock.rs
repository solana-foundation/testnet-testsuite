//! A scripted [`RpcSender`] for the submission state machine tests: every
//! request is recorded and answered by a closure over (method, params).

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};
use solana_rpc_client::rpc_sender::{RpcSender, RpcTransportStats};
use solana_rpc_client_api::client_error::{Error as ClientError, Result as ClientResult};
use solana_rpc_client_api::request::{RpcError, RpcRequest};
use solana_signature::Signature;

/// The all-zero hash, base58.
pub const BLOCKHASH: &str = "11111111111111111111111111111111";

type Handler = dyn Fn(&str, &Value) -> ClientResult<Value> + Send + Sync;
type Delays = dyn Fn(&str) -> Option<Duration> + Send + Sync;

/// Clones share the call log and counters.
#[derive(Clone)]
pub struct MockRpc {
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    handler: Arc<Handler>,
    /// Every answer takes this long...
    delay: Duration,
    /// ...unless this says otherwise for a request (by method).
    delays: Option<Arc<Delays>>,
    in_flight: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl MockRpc {
    /// A mock answering every request with `handler(method, params)`.
    pub fn new(
        handler: impl Fn(&str, &Value) -> ClientResult<Value> + Send + Sync + 'static,
    ) -> Self {
        Self {
            calls: Arc::default(),
            handler: Arc::new(handler),
            delay: Duration::ZERO,
            delays: None,
            in_flight: Arc::default(),
            peak: Arc::default(),
        }
    }

    /// Every answer takes `delay`.
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// Per-request delays: `delays(method)` is called once per request,
    /// in order (`None`: the [`Self::with_delay`] default). An hour stands
    /// for a request that never returns within the test.
    pub fn with_delays(
        mut self,
        delays: impl Fn(&str) -> Option<Duration> + Send + Sync + 'static,
    ) -> Self {
        self.delays = Some(Arc::new(delays));
        self
    }

    /// Most requests that were ever in flight at once.
    pub fn peak_in_flight(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }

    /// Requests made so far for `method`, with their params.
    pub fn calls(&self, method: &str) -> Vec<Value> {
        self.calls
            .lock()
            .expect("calls lock")
            .iter()
            .filter(|(name, _)| name == method)
            .map(|(_, params)| params.clone())
            .collect()
    }

    /// Requests made so far for `method`.
    pub fn count(&self, method: &str) -> usize {
        self.calls(method).len()
    }

    /// Every method requested so far, in order.
    pub fn methods(&self) -> Vec<String> {
        self.calls
            .lock()
            .expect("calls lock")
            .iter()
            .map(|(name, _)| name.clone())
            .collect()
    }
}

impl RpcSender for MockRpc {
    fn send<'life0, 'async_trait>(
        &'life0 self,
        request: RpcRequest,
        params: Value,
    ) -> Pin<Box<dyn Future<Output = ClientResult<Value>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        let method = request.to_string();
        self.calls
            .lock()
            .expect("calls lock")
            .push((method.clone(), params.clone()));
        let result = (self.handler)(&method, &params);
        let delay = self
            .delays
            .as_ref()
            .and_then(|delays| delays(&method))
            .unwrap_or(self.delay);
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        Box::pin(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            result
        })
    }

    fn get_transport_stats(&self) -> RpcTransportStats {
        RpcTransportStats::default()
    }

    fn url(&self) -> String {
        "mock".to_owned()
    }
}

/// `{context, value}`, as RPC methods with a context return it.
pub fn with_context(value: Value) -> Value {
    json!({ "context": { "slot": 1 }, "value": value })
}

/// A `getLatestBlockhash` answer ([`BLOCKHASH`]).
pub fn latest_blockhash(last_valid_block_height: u64) -> Value {
    with_context(json!({
        "blockhash": BLOCKHASH,
        "lastValidBlockHeight": last_valid_block_height,
    }))
}

/// A `simulateTransaction` answer.
pub fn simulation(units_consumed: u64, err: Option<Value>) -> Value {
    with_context(json!({
        "err": err,
        "logs": [],
        "accounts": null,
        "unitsConsumed": units_consumed,
    }))
}

/// One entry of a `getSignatureStatuses` value.
pub fn status(slot: u64, confirmation_status: &str, err: Option<Value>) -> Value {
    json!({
        "slot": slot,
        "confirmations": if confirmation_status == "finalized" { Value::Null } else { json!(0) },
        "status": match &err { Some(err) => json!({ "Err": err }), None => json!({ "Ok": null }) },
        "err": err,
        "confirmationStatus": confirmation_status,
    })
}

/// A `getSignatureStatuses` answer giving every requested signature `entry`.
pub fn statuses(params: &Value, entry: &Value) -> Value {
    let requested = params[0].as_array().map_or(0, Vec::len);
    with_context(Value::Array(vec![entry.clone(); requested]))
}

/// The first signature of a base64 wire transaction (`sendTransaction`'s
/// first param), as the node would echo it.
pub fn sent_signature(params: &Value) -> Value {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(params[0].as_str().expect("encoded transaction"))
        .expect("base64 transaction");
    // One-byte shortvec signature count, then the signatures.
    let signature = Signature::try_from(&bytes[1..65]).expect("signature bytes");
    json!(signature.to_string())
}

/// A JSON-RPC error response, as `HttpSender` reports one.
pub fn rpc_error(code: i64, message: &str) -> ClientError {
    RpcError::RpcResponseError {
        code,
        message: message.to_owned(),
        data: solana_rpc_client_api::request::RpcResponseErrorData::Empty,
    }
    .into()
}

/// A transport-level failure whose request may have reached the node.
pub fn io_error() -> ClientError {
    std::io::Error::new(std::io::ErrorKind::ConnectionReset, "connection reset").into()
}
