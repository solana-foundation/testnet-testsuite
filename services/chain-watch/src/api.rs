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

/// Widest `/slots` span: an hour at 200 ms slots.
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

#[derive(Deserialize)]
struct SlotsQuery {
    from: u64,
    to: u64,
}

async fn slots(
    State(shared): State<Arc<Shared>>,
    Query(query): Query<SlotsQuery>,
) -> Result<Response, ApiError> {
    if query.to < query.from || query.to - query.from > MAX_SLOT_SPAN {
        return Ok((
            StatusCode::BAD_REQUEST,
            format!("from..=to must be ordered and span at most {MAX_SLOT_SPAN} slots"),
        )
            .into_response());
    }
    let rows = shared
        .store
        .read(move |conn| store::slots(conn, query.from, query.to))
        .await?;
    Ok(Json(rows).into_response())
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
