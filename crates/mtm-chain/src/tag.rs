//! Who sent a transaction, for the `chain_tx_*` metrics.
//!
//! Venue clients call [`ChainClient::submit`](crate::ChainClient::submit)
//! deep inside their own code, so the tag travels as a task-local rather than
//! a parameter: a service wraps an action's future in [`scope`] (or narrows
//! the action with [`action`]) and every transaction sent from inside it is
//! labelled. Untagged sends are labelled `unknown`. The scope does not cross
//! `tokio::spawn`; tag the spawned future instead.
//!
//! Labels stay low-cardinality: `actor` names a configured participant
//! (maker, bot) or a role (`taker`), never a per-wallet index.

use std::future::Future;
use std::time::Duration;

use metrics::{counter, histogram};

use crate::SubmissionError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxTag {
    /// A configured participant (`clob-jup-tight`, `arb-fast`) or a role
    /// (`taker`, `scenario`).
    pub actor: String,
    /// What the transaction does (`requote`, `rebalance`, `hedge`, `swap`,
    /// `arb`, `fund`, ...).
    pub action: &'static str,
    /// The venue it touches, or `router` / `multi` when it spans venues.
    pub venue: &'static str,
}

impl TxTag {
    pub fn new(actor: impl Into<String>, action: &'static str, venue: &'static str) -> Self {
        Self {
            actor: actor.into(),
            action,
            venue,
        }
    }
}

tokio::task_local! {
    static TAG: TxTag;
}

/// Run `future` with every transaction it sends labelled `tag`.
pub async fn scope<F: Future>(tag: TxTag, future: F) -> F::Output {
    TAG.scope(tag, future).await
}

/// Run `future` under the current tag with its action replaced (no-op
/// labels `unknown` when there is no current tag).
pub async fn action<F: Future>(action: &'static str, future: F) -> F::Output {
    let mut tag = current();
    tag.action = action;
    TAG.scope(tag, future).await
}

/// The current tag, or an `unknown` one outside any scope.
pub fn current() -> TxTag {
    TAG.try_with(Clone::clone)
        .unwrap_or_else(|_| TxTag::new("unknown", "unknown", "unknown"))
}

/// Which `ChainClient` entry point sent it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Path {
    Submit,
    Versioned,
}

impl Path {
    fn as_str(self) -> &'static str {
        match self {
            Path::Submit => "submit",
            Path::Versioned => "versioned",
        }
    }
}

pub(crate) fn record_submitted(path: Path) {
    let tag = current();
    counter!("chain_tx_submitted_total",
        "actor" => tag.actor, "action" => tag.action, "venue" => tag.venue, "path" => path.as_str())
    .increment(1);
}

pub(crate) fn record_retry() {
    let tag = current();
    counter!("chain_tx_send_retries_total", "actor" => tag.actor, "action" => tag.action)
        .increment(1);
}

/// Count the outcome and, for anything that was sent, how long the call
/// took to settle (submit's includes its simulation).
pub(crate) fn record_outcome<T>(result: &Result<T, SubmissionError>, elapsed: Duration) {
    let tag = current();
    let (outcome, error) = classify(result);
    counter!("chain_tx_outcome_total",
        "actor" => tag.actor, "action" => tag.action, "venue" => tag.venue,
        "outcome" => outcome, "error" => error)
    .increment(1);
    if matches!(outcome, "landed" | "failed" | "expired" | "ambiguous") {
        histogram!("chain_tx_settle_seconds", "action" => tag.action, "outcome" => outcome)
            .record(elapsed.as_secs_f64());
    }
}

/// `(outcome, error kind)`. Outcomes: `landed`, `failed` (on chain),
/// `simulation_failed`, `rejected` (by the RPC), `expired`, `ambiguous`,
/// `client` (no blockhash, signing). The error kind is the transaction
/// error's variant with a custom program code kept (bounded by the
/// programs' error enums), instruction indexes dropped.
pub(crate) fn classify<T>(result: &Result<T, SubmissionError>) -> (&'static str, String) {
    match result {
        Ok(_) => ("landed", String::new()),
        Err(SubmissionError::Execution { error, .. }) => ("failed", error_kind(error)),
        Err(SubmissionError::Simulation { error, .. }) => ("simulation_failed", error_kind(error)),
        Err(SubmissionError::Submission(_)) => ("rejected", "rpc".to_owned()),
        Err(SubmissionError::SimulationRpc(_)) => ("client", "simulation_rpc".to_owned()),
        Err(SubmissionError::Blockhash(_)) => ("client", "blockhash".to_owned()),
        Err(SubmissionError::Signing(_)) => ("client", "signing".to_owned()),
        Err(SubmissionError::Expired { .. }) => ("expired", String::new()),
        Err(SubmissionError::AmbiguousConfirmation { .. }) => ("ambiguous", String::new()),
    }
}

/// `InstructionError(2, Custom(6001))` → `InstructionError:Custom(6001)`;
/// `InsufficientFundsForRent { account_index: 3 }` → `InsufficientFundsForRent`;
/// `BlockhashNotFound` stays.
pub fn error_kind(debug: &str) -> String {
    let debug = debug.trim();
    if let Some(inner) = debug
        .strip_prefix("InstructionError(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        // `<index>, <InstructionError>`
        let error = inner.split_once(", ").map_or(inner, |(_, error)| error);
        let error = error.split([' ', '{']).next().unwrap_or(error);
        return format!("InstructionError:{error}");
    }
    debug
        .split(['(', ' ', '{'])
        .next()
        .unwrap_or(debug)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_kinds_drop_indexes_and_keep_codes() {
        assert_eq!(
            error_kind("InstructionError(2, Custom(6001))"),
            "InstructionError:Custom(6001)"
        );
        assert_eq!(
            error_kind("InstructionError(0, ComputationalBudgetExceeded)"),
            "InstructionError:ComputationalBudgetExceeded"
        );
        assert_eq!(
            error_kind("InstructionError(1, BorshIoError(\"x\"))"),
            "InstructionError:BorshIoError(\"x\")"
        );
        assert_eq!(
            error_kind("InsufficientFundsForRent { account_index: 3 }"),
            "InsufficientFundsForRent"
        );
        assert_eq!(error_kind("BlockhashNotFound"), "BlockhashNotFound");
    }

    #[tokio::test]
    async fn scopes_nest_and_default_to_unknown() {
        assert_eq!(current().actor, "unknown");
        scope(TxTag::new("maker-a", "requote", "manifest"), async {
            assert_eq!(current().action, "requote");
            action("hedge", async {
                let tag = current();
                assert_eq!((tag.actor.as_str(), tag.action), ("maker-a", "hedge"));
            })
            .await;
            assert_eq!(current().action, "requote");
        })
        .await;
    }

    #[test]
    fn outcomes_classify() {
        let landed: Result<(), SubmissionError> = Ok(());
        assert_eq!(classify(&landed).0, "landed");
        let failed: Result<(), SubmissionError> = Err(SubmissionError::Execution {
            signature: Default::default(),
            slot: 1,
            error: "InstructionError(3, Custom(1))".to_owned(),
        });
        assert_eq!(
            classify(&failed),
            ("failed", "InstructionError:Custom(1)".to_owned())
        );
    }
}
