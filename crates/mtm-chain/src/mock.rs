//! A scripted [`RpcSender`] for the submission state machine tests: every
//! request is recorded and answered by a closure over (method, params).

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use base64::Engine;
use serde_json::{Value, json};
use solana_rpc_client::rpc_sender::{RpcSender, RpcTransportStats};
use solana_rpc_client_api::client_error::{Error as ClientError, Result as ClientResult};
use solana_rpc_client_api::request::{RpcError, RpcRequest};
use solana_signature::Signature;

/// The all-zero hash, base58.
pub const BLOCKHASH: &str = "11111111111111111111111111111111";

type Handler = dyn Fn(&str, &Value) -> ClientResult<Value> + Send + Sync;

#[derive(Clone)]
pub struct MockRpc {
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    handler: Arc<Handler>,
}

impl MockRpc {
    pub fn new(
        handler: impl Fn(&str, &Value) -> ClientResult<Value> + Send + Sync + 'static,
    ) -> Self {
        Self {
            calls: Arc::default(),
            handler: Arc::new(handler),
        }
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

    pub fn count(&self, method: &str) -> usize {
        self.calls(method).len()
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
        Box::pin(async move { result })
    }

    fn get_transport_stats(&self) -> RpcTransportStats {
        RpcTransportStats::default()
    }

    fn url(&self) -> String {
        "mock".to_owned()
    }
}

/// `{context, value}` as RPC methods with a context return it.
pub fn with_context(value: Value) -> Value {
    json!({ "context": { "slot": 1 }, "value": value })
}

pub fn latest_blockhash(last_valid_block_height: u64) -> Value {
    with_context(json!({
        "blockhash": BLOCKHASH,
        "lastValidBlockHeight": last_valid_block_height,
    }))
}

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
