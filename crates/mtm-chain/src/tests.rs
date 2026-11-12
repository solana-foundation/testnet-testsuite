//! The submission state machine against a scripted RPC ([`MockRpc`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::future::join_all;
use serde_json::{Value, json};
use solana_commitment_config::CommitmentConfig;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_rpc_client_api::client_error::Result as ClientResult;
use solana_rpc_client_api::request::{RpcError, RpcResponseErrorData};
use solana_signer::Signer;
use solana_transaction::Transaction;
use solana_transaction::versioned::VersionedTransaction;

use crate::mock::{self, MockRpc};
use crate::{ChainClient, ChainOptions, SubmissionConfig, SubmissionError, VersionedSendConfig};

const CUSTOM_ERROR: &str = r#"{"InstructionError":[0,{"Custom":1}]}"#;

fn custom_error() -> Value {
    serde_json::from_str(CUSTOM_ERROR).expect("error json")
}

fn client(mock: &MockRpc) -> ChainClient {
    ChainClient::from_sender(
        mock.clone(),
        CommitmentConfig::confirmed(),
        ChainOptions::default(),
    )
}

fn noop(payer: &Keypair) -> Instruction {
    Instruction {
        program_id: payer.pubkey(),
        accounts: vec![],
        data: vec![],
    }
}

fn signed(payer: &Keypair) -> VersionedTransaction {
    let mut transaction = Transaction::new_with_payer(&[noop(payer)], Some(&payer.pubkey()));
    let blockhash = transaction.message.recent_blockhash;
    transaction.sign(&[payer], blockhash);
    VersionedTransaction::from(transaction)
}

/// A healthy node: blockhash valid until height 1000, height 10, the
/// simulation uses 40k CU, and every status is `status`.
fn node(status: Value) -> impl Fn(&str, &Value) -> ClientResult<Value> + Send + Sync + 'static {
    move |method, params| match method {
        "getLatestBlockhash" => Ok(mock::latest_blockhash(1000)),
        "simulateTransaction" => Ok(mock::simulation(40_000, None)),
        "sendTransaction" => Ok(mock::sent_signature(params)),
        "getSignatureStatuses" => Ok(mock::statuses(params, &status)),
        "getBlockHeight" => Ok(json!(10)),
        other => panic!("unexpected {other}"),
    }
}

#[tokio::test(start_paused = true)]
async fn submit_confirms() {
    let mock = MockRpc::new(node(mock::status(7, "confirmed", None)));
    let payer = Keypair::new();
    let outcome = client(&mock)
        .submit(vec![noop(&payer)], &payer, &[], SubmissionConfig::default())
        .await
        .expect("confirmed");
    assert_eq!(outcome.confirmation_slot, 7);
    assert_eq!(outcome.simulated_compute_units, 40_000);
    assert_eq!(mock.count("sendTransaction"), 1);
}

#[tokio::test(start_paused = true)]
async fn submit_reports_an_on_chain_failure() {
    let mock = MockRpc::new(node(mock::status(7, "confirmed", Some(custom_error()))));
    let payer = Keypair::new();
    let error = client(&mock)
        .submit(vec![noop(&payer)], &payer, &[], SubmissionConfig::default())
        .await
        .expect_err("failed on chain");
    assert!(
        matches!(error, SubmissionError::Execution { slot: 7, .. }),
        "{error:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn send_versioned_confirms() {
    let mock = MockRpc::new(node(mock::status(9, "confirmed", None)));
    let tx = signed(&Keypair::new());
    let outcome = client(&mock)
        .send_versioned(&tx, VersionedSendConfig::default())
        .await
        .expect("confirmed");
    assert_eq!(outcome.signature, tx.signatures[0]);
    assert_eq!(outcome.confirmation_slot, 9);
}

#[tokio::test(start_paused = true)]
async fn send_versioned_preflight_failure_is_a_simulation_error() {
    let mock = MockRpc::new(|method, _| match method {
        "sendTransaction" => Err(RpcError::RpcResponseError {
            code: -32002,
            message: "Transaction simulation failed".to_owned(),
            data: RpcResponseErrorData::SendTransactionPreflightFailure(
                serde_json::from_value(json!({
                    "err": custom_error(),
                    "logs": ["Program log: slippage"],
                    "accounts": null,
                    "unitsConsumed": 1_000,
                }))
                .expect("simulation result"),
            ),
        }
        .into()),
        other => panic!("unexpected {other}"),
    });
    let error = client(&mock)
        .send_versioned(&signed(&Keypair::new()), VersionedSendConfig::default())
        .await
        .expect_err("preflight failure");
    let SubmissionError::Simulation { logs, .. } = error else {
        panic!("{error:?}");
    };
    assert_eq!(logs, ["Program log: slippage"]);
}

#[tokio::test(start_paused = true)]
async fn send_versioned_rpc_rejection_is_not_polled() {
    let mock = MockRpc::new(|method, _| match method {
        "sendTransaction" => Err(mock::rpc_error(-32005, "Node is unhealthy")),
        other => panic!("unexpected {other}"),
    });
    let error = client(&mock)
        .send_versioned(&signed(&Keypair::new()), VersionedSendConfig::default())
        .await
        .expect_err("rejected");
    assert!(matches!(error, SubmissionError::Submission(_)), "{error:?}");
    assert_eq!(mock.count("sendTransaction"), 1);
    assert_eq!(mock.count("getSignatureStatuses"), 0);
}

#[tokio::test(start_paused = true)]
async fn send_versioned_lost_response_lets_the_status_decide() {
    let healthy = node(mock::status(9, "confirmed", None));
    let mock = MockRpc::new(move |method, params| match method {
        "sendTransaction" => Err(mock::io_error()),
        _ => healthy(method, params),
    });
    let outcome = client(&mock)
        .send_versioned(&signed(&Keypair::new()), VersionedSendConfig::default())
        .await
        .expect("landed although the response was lost");
    assert_eq!(outcome.confirmation_slot, 9);
}

/// A node whose `n`th status read (from 0) answers `status(n)` for every
/// signature, whose block height grows one per 400 ms from 10, and whose
/// blockhash is valid until `last_valid`.
fn scripted(
    last_valid: u64,
    status: impl Fn(usize) -> Value + Send + Sync + 'static,
) -> impl Fn(&str, &Value) -> ClientResult<Value> + Send + Sync + 'static {
    let reads = AtomicUsize::new(0);
    let start = tokio::time::Instant::now();
    move |method, params| match method {
        "getLatestBlockhash" => Ok(mock::latest_blockhash(last_valid)),
        "simulateTransaction" => Ok(mock::simulation(40_000, None)),
        "sendTransaction" => Ok(mock::sent_signature(params)),
        "getSignatureStatuses" => Ok(mock::statuses(
            params,
            &status(reads.fetch_add(1, Ordering::SeqCst)),
        )),
        "getBlockHeight" => Ok(json!(10 + start.elapsed().as_millis() / 400)),
        other => panic!("unexpected {other}"),
    }
}

fn after(reads: usize, status: Value) -> impl Fn(usize) -> Value + Send + Sync + 'static {
    move |read| {
        if read >= reads {
            status.clone()
        } else {
            Value::Null
        }
    }
}

#[tokio::test(start_paused = true)]
async fn concurrent_sends_share_batched_polls() {
    // Two rounds of reads: nothing, then confirmed. 500 signatures need
    // two status requests per round and one height read.
    let landed = mock::status(5, "confirmed", None);
    let mock = MockRpc::new(scripted(1000, move |read| {
        if read >= 2 {
            landed.clone()
        } else {
            Value::Null
        }
    }));
    let chain = client(&mock);
    let transactions: Vec<_> = (0..500).map(|_| signed(&Keypair::new())).collect();
    let results = join_all(
        transactions
            .iter()
            .map(|tx| chain.send_versioned(tx, VersionedSendConfig::default())),
    )
    .await;
    assert!(results.iter().all(Result::is_ok));
    assert_eq!(mock.count("sendTransaction"), 500);
    assert_eq!(mock.count("getSignatureStatuses"), 4);
    assert_eq!(mock.count("getBlockHeight"), 2);
    assert!(
        mock.calls("getSignatureStatuses")
            .iter()
            .all(|params| params[0].as_array().map_or(0, Vec::len) <= 256)
    );
}

#[tokio::test(start_paused = true)]
async fn requests_in_flight_are_bounded() {
    let mock = MockRpc::new(|_, _| Ok(json!(1))).with_delay(Duration::from_millis(10));
    let options = ChainOptions {
        max_in_flight: 4,
        ..ChainOptions::default()
    };
    let chain = ChainClient::from_sender(mock.clone(), CommitmentConfig::confirmed(), options);
    let view = chain.with_commitment(CommitmentConfig::processed());
    let reads = (0..20).map(|n| {
        let chain = if n % 2 == 0 { &chain } else { &view };
        chain.rpc().get_slot()
    });
    assert!(join_all(reads).await.iter().all(Result::is_ok));
    assert_eq!(mock.peak_in_flight(), 4);
}

#[tokio::test(start_paused = true)]
async fn submit_rebroadcasts_the_same_bytes_until_it_lands() {
    // The first copy is dropped: nothing lands until the third send.
    let sends = Arc::new(AtomicUsize::new(0));
    let landed = mock::status(8, "confirmed", None);
    let inner = scripted(1000, |_| Value::Null);
    let mock = MockRpc::new({
        let sends = Arc::clone(&sends);
        move |method, params| match method {
            "sendTransaction" => {
                sends.fetch_add(1, Ordering::SeqCst);
                inner(method, params)
            }
            "getSignatureStatuses" if sends.load(Ordering::SeqCst) >= 3 => {
                Ok(mock::statuses(params, &landed))
            }
            _ => inner(method, params),
        }
    });
    let payer = Keypair::new();
    let outcome = client(&mock)
        .submit(vec![noop(&payer)], &payer, &[], SubmissionConfig::default())
        .await
        .expect("landed after a rebroadcast");
    assert_eq!(outcome.confirmation_slot, 8);
    let sent = mock.calls("sendTransaction");
    assert!(sent.len() >= 3);
    assert!(sent.iter().all(|params| params[0] == sent[0][0]));
}

#[tokio::test(start_paused = true)]
async fn submit_expires_when_the_blockhash_does() {
    // Valid until height 20: four seconds of 400 ms blocks from 10.
    let mock = MockRpc::new(scripted(20, |_| Value::Null));
    let payer = Keypair::new();
    let started = tokio::time::Instant::now();
    let error = client(&mock)
        .submit(vec![noop(&payer)], &payer, &[], SubmissionConfig::default())
        .await
        .expect_err("never lands");
    assert!(
        matches!(error, SubmissionError::Expired { .. }),
        "{error:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(10));
    // Rebroadcast about every 2 s until then.
    assert!(mock.count("sendTransaction") >= 2);
}

#[tokio::test(start_paused = true)]
async fn a_landing_seen_in_the_expiry_tick_is_not_expired() {
    // The status read that follows the first height past expiry finds it.
    let landed = mock::status(30, "confirmed", None);
    let mock = MockRpc::new(scripted(20, move |read| {
        // Heights 11, 12, ...: height 21 is read in the 11th tick.
        if read >= 10 {
            landed.clone()
        } else {
            Value::Null
        }
    }));
    let outcome = client(&mock)
        .send_versioned(
            &signed(&Keypair::new()),
            VersionedSendConfig {
                last_valid_block_height: Some(20),
                ..VersionedSendConfig::default()
            },
        )
        .await
        .expect("landed just before expiry");
    assert_eq!(outcome.confirmation_slot, 30);
}

#[tokio::test(start_paused = true)]
async fn submit_tolerates_failed_status_reads() {
    let reads = AtomicUsize::new(0);
    let healthy = node(mock::status(7, "confirmed", None));
    let mock = MockRpc::new(move |method, params| match method {
        "getSignatureStatuses" if reads.fetch_add(1, Ordering::SeqCst) < 2 => Err(mock::io_error()),
        _ => healthy(method, params),
    });
    let payer = Keypair::new();
    let outcome = client(&mock)
        .submit(vec![noop(&payer)], &payer, &[], SubmissionConfig::default())
        .await
        .expect("two failed reads are not final");
    assert_eq!(outcome.confirmation_slot, 7);
}

#[tokio::test(start_paused = true)]
async fn persistent_status_failures_are_ambiguous() {
    let healthy = node(Value::Null);
    let mock = MockRpc::new(move |method, params| match method {
        "getSignatureStatuses" => Err(mock::io_error()),
        _ => healthy(method, params),
    });
    let started = tokio::time::Instant::now();
    let error = client(&mock)
        .send_versioned(&signed(&Keypair::new()), VersionedSendConfig::default())
        .await
        .expect_err("ambiguous");
    assert!(
        matches!(error, SubmissionError::AmbiguousConfirmation { .. }),
        "{error:?}"
    );
    // Five failures at least, spanning the 15 s window: 38 reads at 400 ms.
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_secs(15) && elapsed < Duration::from_secs(17),
        "{elapsed:?}"
    );
    assert!(mock.count("getSignatureStatuses") >= 5);
}

#[tokio::test(start_paused = true)]
async fn a_short_status_outage_is_tolerated() {
    // Three seconds of failed reads (an RPC restart) are not final.
    let start = tokio::time::Instant::now();
    let healthy = node(mock::status(7, "confirmed", None));
    let mock = MockRpc::new(move |method, params| match method {
        "getSignatureStatuses" if start.elapsed() < Duration::from_secs(3) => Err(mock::io_error()),
        _ => healthy(method, params),
    });
    let outcome = client(&mock)
        .send_versioned(&signed(&Keypair::new()), VersionedSendConfig::default())
        .await
        .expect("landed after the outage");
    assert_eq!(outcome.confirmation_slot, 7);
    assert!(mock.count("getSignatureStatuses") > 5);
}

#[tokio::test(start_paused = true)]
async fn a_rejection_after_a_lost_response_still_waits_for_the_status() {
    // Attempt 1 may have reached the node; attempt 2 is refused (e.g. as
    // already processed). The first copy lands.
    let sends = AtomicUsize::new(0);
    let healthy = node(mock::status(7, "confirmed", None));
    let mock = MockRpc::new(move |method, params| match method {
        "sendTransaction" => match sends.fetch_add(1, Ordering::SeqCst) {
            0 => Err(mock::io_error()),
            1 => Err(mock::rpc_error(-32005, "Node is behind")),
            _ => healthy(method, params),
        },
        _ => healthy(method, params),
    });
    let payer = Keypair::new();
    let outcome = client(&mock)
        .submit(vec![noop(&payer)], &payer, &[], SubmissionConfig::default())
        .await
        .expect("never `Submission` once a copy may be out");
    assert_eq!(outcome.confirmation_slot, 7);
}

#[tokio::test(start_paused = true)]
async fn unknown_validity_is_not_expired_early() {
    // No `last_valid_block_height` and no isBlockhashValid: a transaction
    // still pending after several polls lands at the seventh.
    let mock = MockRpc::new(scripted(1000, after(6, mock::status(9, "confirmed", None))));
    let outcome = client(&mock)
        .send_versioned(&signed(&Keypair::new()), VersionedSendConfig::default())
        .await
        .expect("landed");
    assert_eq!(outcome.confirmation_slot, 9);
    assert_eq!(mock.count("isBlockhashValid"), 0);
}

#[tokio::test(start_paused = true)]
async fn unknown_validity_still_expires() {
    let mock = MockRpc::new(scripted(1000, |_| Value::Null));
    let error = client(&mock)
        .send_versioned(&signed(&Keypair::new()), VersionedSendConfig::default())
        .await
        .expect_err("never lands");
    assert!(
        matches!(error, SubmissionError::Expired { .. }),
        "{error:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_failure_below_the_commitment_is_not_final() {
    // Failed on a processed fork, then landed on the confirmed one.
    let failed = mock::status(5, "processed", Some(custom_error()));
    let landed = mock::status(6, "confirmed", None);
    let mock = MockRpc::new(scripted(1000, move |read| {
        if read == 0 {
            failed.clone()
        } else {
            landed.clone()
        }
    }));
    let outcome = client(&mock)
        .send_versioned(&signed(&Keypair::new()), VersionedSendConfig::default())
        .await
        .expect("the confirmed status decides");
    assert_eq!(outcome.confirmation_slot, 6);
}

#[tokio::test(start_paused = true)]
async fn execution_errors_wait_for_the_commitment() {
    let processed = mock::status(5, "processed", Some(custom_error()));
    let confirmed = mock::status(5, "confirmed", Some(custom_error()));
    let mock = MockRpc::new(scripted(1000, move |read| {
        if read < 2 {
            processed.clone()
        } else {
            confirmed.clone()
        }
    }));
    let error = client(&mock)
        .send_versioned(&signed(&Keypair::new()), VersionedSendConfig::default())
        .await
        .expect_err("failed");
    assert!(
        matches!(error, SubmissionError::Execution { slot: 5, .. }),
        "{error:?}"
    );
    assert_eq!(mock.count("getSignatureStatuses"), 3);
}

#[tokio::test(start_paused = true)]
async fn submit_uses_its_commitment_throughout() {
    let mock = MockRpc::new(node(mock::status(7, "finalized", None)));
    let payer = Keypair::new();
    let config = SubmissionConfig {
        commitment: CommitmentConfig::finalized(),
        ..SubmissionConfig::default()
    };
    client(&mock)
        .submit(vec![noop(&payer)], &payer, &[], config)
        .await
        .expect("finalized");
    for method in [
        "getLatestBlockhash",
        "simulateTransaction",
        "getBlockHeight",
    ] {
        let calls = mock.calls(method);
        assert!(!calls.is_empty(), "{method}");
        assert!(
            calls
                .iter()
                .all(|params| params.to_string().contains(r#""commitment":"finalized""#)),
            "{method}: {calls:?}"
        );
    }
    assert!(
        mock.calls("sendTransaction")
            .iter()
            .all(|params| params[1]["preflightCommitment"] == "finalized")
    );
}

#[tokio::test(start_paused = true)]
async fn the_hard_cap_ends_the_wait() {
    // Landed below the commitment and never confirmed: only the cap ends it.
    let mock = MockRpc::new(scripted(1000, |_| mock::status(5, "processed", None)));
    let error = client(&mock)
        .send_versioned(
            &signed(&Keypair::new()),
            VersionedSendConfig {
                confirmation_timeout: Duration::from_secs(5),
                ..VersionedSendConfig::default()
            },
        )
        .await
        .expect_err("capped");
    assert!(
        matches!(error, SubmissionError::AmbiguousConfirmation { .. }),
        "{error:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn send_versioned_resends_after_a_lost_response() {
    let sends = AtomicUsize::new(0);
    let healthy = node(mock::status(9, "confirmed", None));
    let mock = MockRpc::new(move |method, params| match method {
        "sendTransaction" if sends.fetch_add(1, Ordering::SeqCst) < 2 => Err(mock::io_error()),
        _ => healthy(method, params),
    });
    client(&mock)
        .send_versioned(&signed(&Keypair::new()), VersionedSendConfig::default())
        .await
        .expect("landed");
    let sent = mock.calls("sendTransaction");
    assert_eq!(sent.len(), 3);
    assert!(sent.iter().all(|params| params[0] == sent[0][0]));
}

/// How [`http_endpoint`] answers one JSON-RPC method: status line, extra
/// header lines, body.
type HttpAnswer = (&'static str, &'static str, String);

/// Every request answered with `status_line`, `Retry-After: 0` and no body.
fn every(status_line: &'static str) -> impl Fn(&str) -> HttpAnswer + Send + Sync + 'static {
    move |_| (status_line, "Retry-After: 0\r\n", String::new())
}

/// A JSON-RPC success answer.
fn http_ok(result: Value) -> HttpAnswer {
    (
        "200 OK",
        "Content-Type: application/json\r\n",
        json!({ "jsonrpc": "2.0", "id": 0, "result": result }).to_string(),
    )
}

/// A local HTTP endpoint answering each request with `answer(method)`;
/// returns its URL and the JSON-RPC methods it was asked.
async fn http_endpoint(
    answer: impl Fn(&str) -> HttpAnswer + Send + Sync + 'static,
) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("address"));
    let methods = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = Arc::clone(&methods);
    let answer = Arc::new(answer);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let seen = Arc::clone(&seen);
            let answer = Arc::clone(&answer);
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = [0_u8; 4096];
                loop {
                    // One request: headers, then Content-Length bytes.
                    let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else {
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                        }
                        continue;
                    };
                    let headers = String::from_utf8_lossy(&buffer[..end]).to_lowercase();
                    let length: usize = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .and_then(|value| value.trim().parse().ok())
                        .unwrap_or(0);
                    while buffer.len() < end + 4 + length {
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let body: Value = serde_json::from_slice(&buffer[end + 4..end + 4 + length])
                        .unwrap_or_default();
                    let method = body["method"].as_str().unwrap_or_default().to_owned();
                    let (status_line, headers, body) = answer(&method);
                    seen.lock().expect("methods lock").push(method);
                    buffer.drain(..end + 4 + length);
                    let response = format!(
                        "HTTP/1.1 {status_line}\r\n{headers}Content-Length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    if socket.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (url, methods)
}

async fn send_over_http(url: &str) -> SubmissionError {
    ChainClient::new(url, "confirmed")
        .expect("client")
        .send_versioned(&signed(&Keypair::new()), VersionedSendConfig::default())
        .await
        .expect_err("not taken")
}

#[tokio::test]
async fn http_429_on_send_is_rate_limited_and_not_polled() {
    let (url, methods) = http_endpoint(every("429 Too Many Requests")).await;
    let error = send_over_http(&url).await;
    assert!(
        matches!(error, SubmissionError::RateLimited(_)),
        "{error:?}"
    );
    let methods = methods.lock().expect("methods lock");
    assert!(
        methods.iter().all(|method| method == "sendTransaction"),
        "{methods:?}"
    );
}

#[tokio::test]
async fn http_5xx_on_send_is_unavailable_and_not_polled() {
    let (url, methods) = http_endpoint(every("503 Service Unavailable")).await;
    let error = send_over_http(&url).await;
    assert!(
        matches!(error, SubmissionError::Unavailable(_)),
        "{error:?}"
    );
    assert_eq!(*methods.lock().expect("methods lock"), ["sendTransaction"]);
}

#[tokio::test]
async fn a_gateway_timeout_on_send_is_resent_and_polled() {
    // The proxy gave up on a node that may have taken it: the status decides.
    let tx = signed(&Keypair::new());
    let (url, methods) = http_endpoint(|method| match method {
        "sendTransaction" => ("504 Gateway Timeout", "", String::new()),
        "getBlockHeight" => http_ok(json!(10)),
        "getSignatureStatuses" => http_ok(json!({
            "context": { "slot": 1 },
            "value": [mock::status(9, "confirmed", None)],
        })),
        other => panic!("unexpected {other}"),
    })
    .await;
    let outcome = ChainClient::new(url, "confirmed")
        .expect("client")
        .send_versioned(&tx, VersionedSendConfig::default())
        .await
        .expect("landed despite the 504s");
    assert_eq!(outcome.confirmation_slot, 9);
    let methods = methods.lock().expect("methods lock");
    let sends = methods.iter().filter(|m| *m == "sendTransaction").count();
    assert_eq!(sends, 3, "{methods:?}");
    assert!(methods.iter().any(|m| m == "getSignatureStatuses"));
}

#[tokio::test]
async fn a_rate_limited_request_does_not_hold_its_permit() {
    // One permit. The send's 429s make `HttpSender` sleep 500 ms between
    // retries; a read issued meanwhile goes through at once.
    let (url, methods) = http_endpoint(|method| match method {
        "sendTransaction" => ("429 Too Many Requests", "", String::new()),
        _ => http_ok(json!(42)),
    })
    .await;
    let chain = ChainClient::with_options(
        url,
        "confirmed",
        ChainOptions {
            max_in_flight: 1,
            ..ChainOptions::default()
        },
    )
    .expect("client");
    let sender = chain.clone();
    let send = tokio::spawn(async move {
        sender
            .send_versioned(&signed(&Keypair::new()), VersionedSendConfig::default())
            .await
    });
    while !methods
        .lock()
        .expect("methods lock")
        .iter()
        .any(|m| m == "sendTransaction")
    {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let started = std::time::Instant::now();
    assert_eq!(chain.rpc().get_slot().await.expect("slot"), 42);
    let waited = started.elapsed();
    assert!(waited < Duration::from_millis(300), "{waited:?}");
    send.abort();
}

#[tokio::test]
async fn a_refused_connection_is_unavailable() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("address"));
    drop(listener);
    let error = send_over_http(&url).await;
    assert!(
        matches!(error, SubmissionError::Unavailable(_)),
        "{error:?}"
    );
}

/// Whether a base64 wire transaction (first param) carries
/// `SetComputeUnitLimit(limit)`: data length 5, tag 2, the limit LE.
fn has_compute_unit_limit(params: &Value, limit: u32) -> bool {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(params[0].as_str().expect("encoded transaction"))
        .expect("base64");
    let mut data = vec![5, 2];
    data.extend_from_slice(&limit.to_le_bytes());
    bytes.windows(data.len()).any(|window| window == data)
}

#[tokio::test(start_paused = true)]
async fn submit_sizes_the_compute_unit_limit_from_the_simulation() {
    let mock = MockRpc::new(node(mock::status(7, "confirmed", None)));
    let payer = Keypair::new();
    let outcome = client(&mock)
        .submit(vec![noop(&payer)], &payer, &[], SubmissionConfig::default())
        .await
        .expect("confirmed");
    // Simulated at the 1.4M ceiling, sent at 40k used + 10k headroom.
    assert!(has_compute_unit_limit(
        &mock.calls("simulateTransaction")[0],
        1_400_000
    ));
    let sent = &mock.calls("sendTransaction")[0];
    assert!(has_compute_unit_limit(sent, 50_000));
    assert_eq!(
        mock::sent_signature(sent),
        json!(outcome.signature.to_string())
    );

    let exact = MockRpc::new(node(mock::status(7, "confirmed", None)));
    let config = SubmissionConfig {
        compute_unit_limit: 300_000,
        size_compute_units: false,
        ..SubmissionConfig::default()
    };
    client(&exact)
        .submit(vec![noop(&payer)], &payer, &[], config)
        .await
        .expect("confirmed");
    assert!(has_compute_unit_limit(
        &exact.calls("sendTransaction")[0],
        300_000
    ));
}

#[test]
fn sized_limits_keep_headroom_under_the_ceiling() {
    use crate::sized_compute_unit_limit;
    assert_eq!(sized_compute_unit_limit(450, 1_400_000), 10_450);
    assert_eq!(sized_compute_unit_limit(200_000, 1_400_000), 240_000);
    assert_eq!(sized_compute_unit_limit(1_300_000, 1_400_000), 1_400_000);
    assert_eq!(sized_compute_unit_limit(u64::MAX, 1_400_000), 1_400_000);
}

#[tokio::test(start_paused = true)]
async fn status_polls_do_not_queue_behind_callers() {
    // One permit, held by a caller read that never returns: the confirmer
    // still polls on its own permits.
    let mock = MockRpc::new(|method, params| match method {
        "getSlot" => Ok(json!(1)),
        _ => node(mock::status(9, "confirmed", None))(method, params),
    })
    .with_delays(|method| (method == "getSlot").then_some(Duration::from_secs(3600)));
    let options = ChainOptions {
        max_in_flight: 1,
        ..ChainOptions::default()
    };
    let chain = ChainClient::from_sender(mock.clone(), CommitmentConfig::confirmed(), options);
    let reader = chain.clone();
    let stuck = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        reader.rpc().get_slot().await
    });
    let started = tokio::time::Instant::now();
    let outcome = chain
        .send_versioned(&signed(&Keypair::new()), VersionedSendConfig::default())
        .await
        .expect("confirmed");
    assert_eq!(outcome.confirmation_slot, 9);
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(mock.count("getSlot"), 1, "the caller holds the permit");
    stuck.abort();
}

#[tokio::test(start_paused = true)]
async fn a_stuck_status_read_still_ends_at_the_cap() {
    let mock = MockRpc::new(node(Value::Null)).with_delays(|method| {
        (method == "getSignatureStatuses").then_some(Duration::from_secs(3600))
    });
    let started = tokio::time::Instant::now();
    let error = client(&mock)
        .send_versioned(
            &signed(&Keypair::new()),
            VersionedSendConfig {
                confirmation_timeout: Duration::from_secs(5),
                ..VersionedSendConfig::default()
            },
        )
        .await
        .expect_err("capped");
    assert!(
        matches!(error, SubmissionError::AmbiguousConfirmation { .. }),
        "{error:?}"
    );
    assert!(started.elapsed() <= Duration::from_secs(8));
}

#[tokio::test(start_paused = true)]
async fn a_hanging_rebroadcast_does_not_delay_the_verdict() {
    // The first send answers; the rebroadcast at 2 s never does. The
    // transaction is seen landed at about 2.4 s.
    let sends = AtomicUsize::new(0);
    let mock = MockRpc::new(scripted(1000, after(5, mock::status(8, "confirmed", None))))
        .with_delays(move |method| {
            (method == "sendTransaction" && sends.fetch_add(1, Ordering::SeqCst) > 0)
                .then_some(Duration::from_secs(3600))
        });
    let payer = Keypair::new();
    let started = tokio::time::Instant::now();
    let outcome = client(&mock)
        .submit(vec![noop(&payer)], &payer, &[], SubmissionConfig::default())
        .await
        .expect("landed");
    assert_eq!(outcome.confirmation_slot, 8);
    assert_eq!(mock.count("sendTransaction"), 2);
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[tokio::test(start_paused = true)]
async fn rebroadcasts_back_off() {
    // Valid until height 60 (about 20 s); every 2 s would be 11 sends.
    let mock = MockRpc::new(scripted(60, |_| Value::Null));
    let payer = Keypair::new();
    let error = client(&mock)
        .submit(vec![noop(&payer)], &payer, &[], SubmissionConfig::default())
        .await
        .expect_err("never lands");
    assert!(
        matches!(error, SubmissionError::Expired { .. }),
        "{error:?}"
    );
    // At 0, 2, 6 and 14 s, then every 8 s.
    assert_eq!(mock.count("sendTransaction"), 4);
}

#[tokio::test(start_paused = true)]
async fn one_tick_past_expiry_is_not_expired() {
    // Height 21 is first read in the 11th tick and nothing is found; the
    // 12th tick (a lagging status node caught up) finds it landed.
    let mock = MockRpc::new(scripted(20, after(11, mock::status(30, "confirmed", None))));
    let outcome = client(&mock)
        .send_versioned(
            &signed(&Keypair::new()),
            VersionedSendConfig {
                last_valid_block_height: Some(20),
                ..VersionedSendConfig::default()
            },
        )
        .await
        .expect("landed in the last valid blocks");
    assert_eq!(outcome.confirmation_slot, 30);
}

#[tokio::test(start_paused = true)]
async fn a_short_status_answer_is_a_failed_read() {
    // Already past expiry; the first three answers leave the signature
    // out, which must not read as "not found".
    let reads = AtomicUsize::new(0);
    let landed = mock::status(30, "confirmed", None);
    let mock = MockRpc::new(move |method, params| match method {
        "sendTransaction" => Ok(mock::sent_signature(params)),
        "getBlockHeight" => Ok(json!(25)),
        "getSignatureStatuses" => Ok(if reads.fetch_add(1, Ordering::SeqCst) < 3 {
            mock::with_context(json!([]))
        } else {
            mock::statuses(params, &landed)
        }),
        other => panic!("unexpected {other}"),
    });
    let outcome = client(&mock)
        .send_versioned(
            &signed(&Keypair::new()),
            VersionedSendConfig {
                last_valid_block_height: Some(20),
                ..VersionedSendConfig::default()
            },
        )
        .await
        .expect("landed");
    assert_eq!(outcome.confirmation_slot, 30);
}

#[tokio::test(start_paused = true)]
async fn a_caller_that_stops_waiting_is_dropped() {
    let mock = MockRpc::new(node(Value::Null));
    let chain = client(&mock);
    let tx = signed(&Keypair::new());
    let wait = chain.send_versioned(&tx, VersionedSendConfig::default());
    assert!(
        tokio::time::timeout(Duration::from_millis(1_000), wait)
            .await
            .is_err()
    );
    let reads = mock.count("getSignatureStatuses");
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(mock.count("getSignatureStatuses"), reads);
}

#[test]
fn the_confirmer_outlives_its_runtime() {
    let mock = MockRpc::new(node(mock::status(9, "confirmed", None)));
    let chain = client(&mock);
    let tx = signed(&Keypair::new());
    for _ in 0..2 {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .expect("runtime");
        let outcome = runtime
            .block_on(chain.send_versioned(&tx, VersionedSendConfig::default()))
            .expect("confirmed on each runtime");
        assert_eq!(outcome.confirmation_slot, 9);
    }
}

#[tokio::test(start_paused = true)]
async fn each_commitment_reads_its_height_once_per_tick() {
    let mock = MockRpc::new(node(mock::status(9, "finalized", None)));
    let chain = client(&mock);
    let finalized = chain.with_commitment(CommitmentConfig::finalized());
    let (a, b) = (signed(&Keypair::new()), signed(&Keypair::new()));
    let (x, y, z) = tokio::join!(
        chain.send_versioned(&a, VersionedSendConfig::default()),
        chain.send_versioned(&b, VersionedSendConfig::default()),
        finalized.send_versioned(&a, VersionedSendConfig::default()),
    );
    assert!(x.is_ok() && y.is_ok() && z.is_ok());
    let heights = mock.calls("getBlockHeight");
    assert_eq!(heights.len(), 2, "{heights:?}");
    for level in ["confirmed", "finalized"] {
        assert!(
            heights
                .iter()
                .any(|params| params.to_string().contains(level)),
            "{level}: {heights:?}"
        );
    }
    assert_eq!(mock.count("getSignatureStatuses"), 1);
}

#[tokio::test(start_paused = true)]
async fn heights_are_read_before_statuses() {
    let mock = MockRpc::new(scripted(1000, after(3, mock::status(9, "confirmed", None))));
    client(&mock)
        .send_versioned(&signed(&Keypair::new()), VersionedSendConfig::default())
        .await
        .expect("landed");
    let polls: Vec<String> = mock
        .methods()
        .into_iter()
        .filter(|method| method == "getBlockHeight" || method == "getSignatureStatuses")
        .collect();
    let ticks: Vec<&[String]> = polls.chunks(2).collect();
    assert_eq!(ticks.len(), 4, "{polls:?}");
    assert!(
        ticks
            .iter()
            .all(|tick| tick == &["getBlockHeight", "getSignatureStatuses"]),
        "{polls:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn send_versioned_rebroadcasts_until_the_verdict() {
    // Landed at the 6th read (2.4 s): sends at 0, 1 and 3 s... but 3 s is
    // after the verdict, so two.
    let mock = MockRpc::new(scripted(1000, after(5, mock::status(9, "confirmed", None))));
    let tx = signed(&Keypair::new());
    client(&mock)
        .send_versioned(
            &tx,
            VersionedSendConfig {
                rebroadcast_interval: Some(Duration::from_secs(1)),
                ..VersionedSendConfig::default()
            },
        )
        .await
        .expect("landed");
    let sent = mock.calls("sendTransaction");
    assert_eq!(sent.len(), 2);
    assert!(sent.iter().all(|params| params[0] == sent[0][0]));
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(mock.count("sendTransaction"), 2);
}
