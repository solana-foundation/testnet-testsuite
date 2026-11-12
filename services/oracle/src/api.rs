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
        .route("/ready", get(ready))
        .route("/v1/prices", get(all_prices))
        .route("/v1/price", get(one_price))
        .route("/v1/instruments", get(instruments))
        .route("/v1/ws", get(ws_handler))
        .with_state(state)
}

/// Liveness: the process is up and serving.
async fn health() -> &'static str {
    "ok"
}

/// Readiness: every registered instrument was published recently (503 with
/// the offending instruments otherwise).
async fn ready(State(state): State<Arc<AppState>>) -> (StatusCode, String) {
    let not_ready = state.not_ready(mtm_common::time::now_us()).await;
    if not_ready.is_empty() {
        return (StatusCode::OK, "ready".to_string());
    }
    let reasons: Vec<String> = not_ready.iter().map(ToString::to_string).collect();
    (
        StatusCode::SERVICE_UNAVAILABLE,
        format!("not ready: {}", reasons.join(", ")),
    )
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use oracle_client::{InstrumentInfo, Price, PriceData, PriceSource};

    use super::*;
    use crate::state::ReadyLimits;

    fn info(symbol: &str) -> InstrumentInfo {
        InstrumentInfo {
            symbol: Symbol::new(symbol),
            mint: None,
            base: "peg".to_string(),
            transforms: Vec::new(),
        }
    }

    fn point(symbol: &str, received_at_us: i64) -> PricePoint {
        PricePoint {
            symbol: Symbol::new(symbol),
            data: PriceData::Aggregate {
                price: Price::new(100_000_000, -8),
                conf: None,
                ema: None,
            },
            publish_time_us: None,
            received_at_us,
            slot: None,
            source: PriceSource::Derived,
        }
    }

    /// Serves the router on a loopback port; returns its base URL.
    async fn serve(state: Arc<AppState>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move { axum::serve(listener, router(state)).await });
        format!("http://{addr}")
    }

    async fn get(url: String) -> (u16, String) {
        let res = reqwest::get(url).await.expect("request");
        let status = res.status().as_u16();
        (status, res.text().await.expect("body"))
    }

    #[tokio::test]
    async fn ready_requires_a_recent_point_for_every_instrument() {
        let mut limits = ReadyLimits::uniform(Duration::from_secs(15));
        limits
            .per_symbol
            .insert(Symbol::new("SLOW/USD"), Duration::from_secs(600));
        let state = AppState::new(vec![info("A/USD"), info("SLOW/USD")], limits);
        let base = serve(state.clone()).await;

        // Liveness holds regardless.
        assert_eq!(get(format!("{base}/health")).await, (200, "ok".to_string()));

        let (status, body) = get(format!("{base}/ready")).await;
        assert_eq!(status, 503);
        assert!(body.contains("A/USD: no price"), "{body}");
        assert!(body.contains("SLOW/USD: no price"), "{body}");

        let now = mtm_common::time::now_us();
        // A/USD is 60 s old (past 15 s); SLOW/USD is 60 s old within its 600 s.
        state.update(point("A/USD", now - 60_000_000)).await;
        state.update(point("SLOW/USD", now - 60_000_000)).await;
        let (status, body) = get(format!("{base}/ready")).await;
        assert_eq!(status, 503);
        assert!(body.contains("A/USD: 60"), "{body}");
        assert!(!body.contains("SLOW/USD"), "{body}");

        state
            .update(point("A/USD", mtm_common::time::now_us()))
            .await;
        assert_eq!(
            get(format!("{base}/ready")).await,
            (200, "ready".to_string())
        );
    }
}
