//! The submission state machine against a scripted RPC ([`MockRpc`]).

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
use crate::{ChainClient, SubmissionConfig, SubmissionError, VersionedSendConfig};

const CUSTOM_ERROR: &str = r#"{"InstructionError":[0,{"Custom":1}]}"#;

fn custom_error() -> Value {
    serde_json::from_str(CUSTOM_ERROR).expect("error json")
}

fn client(mock: &MockRpc) -> ChainClient {
    ChainClient::from_sender(mock.clone(), CommitmentConfig::confirmed())
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
        "isBlockhashValid" => Ok(mock::with_context(json!(true))),
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
