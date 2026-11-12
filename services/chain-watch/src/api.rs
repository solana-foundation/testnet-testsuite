//! Read-only HTTP API over the run store (types in chain-watch-client).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chain_watch_client::Health;
use serde::Deserialize;

use crate::state::Shared;
use crate::store;

/// Widest `/slots` span, and most rows per reply: an hour at 200 ms slots.
const MAX_SLOT_SPAN: u64 = 18_000;

pub fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/features", get(features))
        .route("/events", get(events))
        .route("/slots", get(slots))
        .route("/txs", get(txs))
        .with_state(shared)
}

struct ApiError(anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", self.0)).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(error: E) -> Self {
        Self(error.into())
    }
}

async fn health(State(shared): State<Arc<Shared>>) -> Json<Health> {
    let confirmed = shared.confirmed();
    let cursor = shared.cursor.load(Ordering::Relaxed);
    Json(Health {
        run_id: shared.run_id.clone(),
        block_details: shared.block_details,
        rpc_host: shared.rpc_host.clone(),
        processed_slot: shared.processed.load(Ordering::Relaxed),
        confirmed_slot: confirmed,
        cursor_slot: cursor,
        lag_slots: confirmed.saturating_sub(cursor),
        our_wallets: shared.our_wallets.load(Ordering::Relaxed),
    })
}

async fn features(
    State(shared): State<Arc<Shared>>,
) -> Result<Json<Vec<chain_watch_client::Feature>>, ApiError> {
    Ok(Json(shared.store.read(store::features).await?))
}

#[derive(Deserialize)]
struct EventsQuery {
    #[serde(default)]
    after: i64,
    #[serde(default = "default_limit")]
    limit: u32,
}

fn default_limit() -> u32 {
    1_000
}

async fn events(
    State(shared): State<Arc<Shared>>,
    Query(query): Query<EventsQuery>,
) -> Result<Json<Vec<chain_watch_client::Event>>, ApiError> {
    let limit = query.limit.min(10_000);
    Ok(Json(
        shared
            .store
            .read(move |conn| store::events(conn, query.after, limit))
            .await?,
    ))
}

/// `from` + `to`: the rows in `from..=to`. `from` + `limit` (no `to`): the
/// first `limit` rows at or after `from`, whatever holes lie between, so a
/// follower can always advance past the last row it got.
#[derive(Deserialize)]
struct SlotsQuery {
    from: u64,
    to: Option<u64>,
    limit: Option<u32>,
}

async fn slots(
    State(shared): State<Arc<Shared>>,
    Query(query): Query<SlotsQuery>,
) -> Result<Response, ApiError> {
    let (from, (to, limit)) = match slot_bounds(&query) {
        Ok(bounds) => (query.from, bounds),
        Err(message) => return Ok((StatusCode::BAD_REQUEST, message).into_response()),
    };
    let rows = shared
        .store
        .read(move |conn| store::slots(conn, from, to, limit))
        .await?;
    Ok(Json(rows).into_response())
}

/// The `to` and row limit a `/slots` query reads with. `from..=to` holds
/// up to `MAX_SLOT_SPAN + 1` slots, all returned; a bare `limit` is capped
/// at `MAX_SLOT_SPAN`.
fn slot_bounds(query: &SlotsQuery) -> Result<(u64, u32), String> {
    match (query.to, query.limit) {
        (Some(to), _) if to < query.from || to - query.from > MAX_SLOT_SPAN => Err(format!(
            "from..=to must be ordered and span at most {MAX_SLOT_SPAN} slots"
        )),
        (Some(to), limit) => Ok((to, limit.unwrap_or(u32::MAX).min(MAX_SLOT_SPAN as u32 + 1))),
        (None, None) => Err("give `to` or `limit`".to_owned()),
        (None, Some(limit)) => Ok((u64::MAX, limit.min(MAX_SLOT_SPAN as u32))),
    }
}

#[derive(Deserialize)]
struct TxsQuery {
    slot: u64,
}

async fn txs(
    State(shared): State<Arc<Shared>>,
    Query(query): Query<TxsQuery>,
) -> Result<Json<Vec<chain_watch_client::OurTx>>, ApiError> {
    Ok(Json(
        shared
            .store
            .read(move |conn| store::txs(conn, query.slot))
            .await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(from: u64, to: Option<u64>, limit: Option<u32>) -> SlotsQuery {
        SlotsQuery { from, to, limit }
    }

    #[test]
    fn a_full_width_range_returns_every_slot() {
        // 0..=18_000 is 18_001 slots.
        let (to, limit) = slot_bounds(&query(0, Some(MAX_SLOT_SPAN), None)).expect("valid");
        assert_eq!((to, limit), (MAX_SLOT_SPAN, MAX_SLOT_SPAN as u32 + 1));
        assert!(slot_bounds(&query(0, Some(MAX_SLOT_SPAN + 1), None)).is_err());
        assert!(slot_bounds(&query(5, Some(4), None)).is_err());
        assert_eq!(
            slot_bounds(&query(0, Some(10), Some(3))).expect("valid"),
            (10, 3)
        );
        // `from` + `limit`: capped at the span.
        assert_eq!(
            slot_bounds(&query(7, None, Some(u32::MAX))).expect("valid"),
            (u64::MAX, MAX_SLOT_SPAN as u32)
        );
        assert!(slot_bounds(&query(7, None, None)).is_err());
    }
}
