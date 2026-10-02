use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};
use oracle_client::{PricePoint, Symbol};
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, warn};

use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/prices", get(all_prices))
        .route("/v1/price", get(one_price))
        .route("/v1/instruments", get(instruments))
        .route("/v1/ws", get(ws_handler))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

async fn all_prices(State(state): State<Arc<AppState>>) -> Json<Vec<PricePoint>> {
    Json(state.all().await)
}

/// Lookup by canonical symbol OR testnet mint address (exactly one).
#[derive(Debug, Deserialize)]
struct PriceQuery {
    symbol: Option<Symbol>,
    mint: Option<String>,
}

async fn one_price(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PriceQuery>,
) -> Result<Json<PricePoint>, StatusCode> {
    let symbol = match (query.symbol, query.mint) {
        (Some(symbol), None) => symbol,
        (None, Some(mint)) => state
            .symbol_for_mint(&mint)
            .cloned()
            .ok_or(StatusCode::NOT_FOUND)?,
        _ => return Err(StatusCode::BAD_REQUEST),
    };
    state
        .get(&symbol)
        .await
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

async fn instruments(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<oracle_client::InstrumentInfo>> {
    Json(state.instruments().to_vec())
}

async fn ws_handler(State(state): State<Arc<AppState>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| stream_prices(socket, state))
}

async fn stream_prices(mut socket: WebSocket, state: Arc<AppState>) {
    let mut rx = state.subscribe();
    loop {
        tokio::select! {
            update = rx.recv() => match update {
                Ok(point) => {
                    let Ok(json) = serde_json::to_string(&point) else { continue };
                    if socket.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(skipped)) => {
                    warn!(skipped, "ws client lagged, updates dropped");
                }
                Err(RecvError::Closed) => break,
            },
            // TODO: per-symbol subscription filtering; for now clients get everything
            incoming = socket.recv() => match incoming {
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
        }
    }
    debug!("ws client disconnected");
}
