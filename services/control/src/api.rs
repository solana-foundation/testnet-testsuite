//! HTTP + websocket API under `/api`, and the dashboard bundle at `/`.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use control_client::{JobRequest, Live};
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;
use tower_http::services::{ServeDir, ServeFile};

use crate::jobs::{self, Rejected};
use crate::{App, processes};

pub fn router(app: Arc<App>) -> Router {
    let ui = &app.cfg.ui_dir;
    let index = ui.join("index.html");
    let api = Router::new()
        .route("/api/status", get(status))
        .route("/api/jobs", get(list_jobs).post(start_job))
        .route("/api/jobs/{id}/cancel", post(cancel_job))
        .route("/api/jobs/{id}/output", get(job_output))
        .route("/api/logs/{name}", get(logs))
        .route("/api/events", get(events))
        .route("/api/chain/{*path}", get(chain_proxy))
        .route("/api/prom/{endpoint}", get(prom_proxy))
        .route("/api/live", get(live))
        .with_state(app.clone());
    if index.is_file() {
        api.fallback_service(ServeDir::new(ui).fallback(ServeFile::new(index)))
    } else {
        api.fallback(|| async {
            (
                StatusCode::NOT_FOUND,
                "dashboard not built: run `just dashboard-build` (the API is under /api)",
            )
        })
    }
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, message.into()).into_response()
}

async fn status(State(app): State<Arc<App>>) -> Json<control_client::Status> {
    Json(app.status().await)
}

async fn list_jobs(State(app): State<Arc<App>>) -> Response {
    match app.store.jobs(50) {
        Ok(jobs) => Json(jobs).into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

async fn start_job(State(app): State<Arc<App>>, Json(request): Json<JobRequest>) -> Response {
    match jobs::start(app, request).await {
        Ok(job) => (StatusCode::ACCEPTED, Json(job)).into_response(),
        Err(Rejected::Invalid(message)) => error(StatusCode::BAD_REQUEST, message),
        Err(Rejected::Busy(id)) => error(
            StatusCode::CONFLICT,
            format!("job {id} is still running; one job at a time"),
        ),
    }
}

async fn cancel_job(State(app): State<Arc<App>>, Path(id): Path<i64>) -> Response {
    match jobs::cancel(&app, id).await {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(message) => error(StatusCode::CONFLICT, message),
    }
}

async fn job_output(State(app): State<Arc<App>>, Path(id): Path<i64>) -> Response {
    let path = app
        .cfg
        .state_dir
        .join("control")
        .join("jobs")
        .join(format!("{id}.log"));
    match tokio::fs::read_to_string(&path).await {
        Ok(text) => text.into_response(),
        Err(_) => error(StatusCode::NOT_FOUND, format!("no output for job {id}")),
    }
}

#[derive(Deserialize)]
struct LogsQuery {
    #[serde(default = "default_lines")]
    lines: usize,
}

fn default_lines() -> usize {
    200
}

async fn logs(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Query(query): Query<LogsQuery>,
) -> Response {
    if !processes::SERVICES.contains(&name.as_str()) {
        return error(StatusCode::NOT_FOUND, format!("unknown service {name}"));
    }
    let state_dir = app.cfg.state_dir.clone();
    let lines = query.lines.min(5_000);
    match tokio::task::spawn_blocking(move || processes::tail(&state_dir, &name, lines)).await {
        Ok(Ok(text)) => text.into_response(),
        Ok(Err(e)) => error(StatusCode::NOT_FOUND, format!("{e:#}")),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
struct EventsQuery {
    #[serde(default)]
    since_us: i64,
}

async fn events(State(app): State<Arc<App>>, Query(query): Query<EventsQuery>) -> Response {
    match app.store.events(query.since_us) {
        Ok(events) => Json(events).into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// `GET /api/chain/<path>` → chain-watch `/<path>` (read-only API).
async fn chain_proxy(
    State(app): State<Arc<App>>,
    Path(path): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    if !["health", "features", "events", "slots", "txs"].contains(&path.as_str()) {
        return error(
            StatusCode::NOT_FOUND,
            format!("no chain-watch endpoint {path}"),
        );
    }
    proxy(&app, &app.cfg.chain_watch_url, &path, query).await
}

/// `GET /api/prom/query|query_range` → Prometheus `/api/v1/...`.
async fn prom_proxy(
    State(app): State<Arc<App>>,
    Path(endpoint): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    if !["query", "query_range"].contains(&endpoint.as_str()) {
        return error(
            StatusCode::NOT_FOUND,
            format!("no Prometheus endpoint {endpoint}"),
        );
    }
    proxy(
        &app,
        &app.cfg.prometheus_url,
        &format!("api/v1/{endpoint}"),
        query,
    )
    .await
}

async fn proxy(app: &App, base: &str, path: &str, query: Option<String>) -> Response {
    let mut url = format!("{}/{path}", base.trim_end_matches('/'));
    if let Some(query) = query {
        url = format!("{url}?{query}");
    }
    match app.http.get(&url).send().await {
        Ok(response) => {
            let status =
                StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/json")
                .to_owned();
            match response.bytes().await {
                Ok(bytes) => Response::builder()
                    .status(status)
                    .header(header::CONTENT_TYPE, content_type)
                    .body(Body::from(bytes))
                    .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response()),
                Err(e) => error(StatusCode::BAD_GATEWAY, e.to_string()),
            }
        }
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{base} unreachable: {e}")),
    }
}

async fn live(State(app): State<Arc<App>>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| stream(app, socket))
}

/// Snapshot first, then every broadcast; a client that falls behind gets a
/// fresh snapshot instead of a gap.
async fn stream(app: Arc<App>, mut socket: WebSocket) {
    let mut rx = app.hub.subscribe();
    let send = |message: &Live| serde_json::to_string(message).map(|t| Message::Text(t.into()));
    let Ok(first) = send(&app.hub.snapshot(app.status().await)) else {
        return;
    };
    if socket.send(first).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            message = rx.recv() => {
                let message = match message {
                    Ok(message) => message,
                    Err(RecvError::Lagged(_)) => app.hub.snapshot(app.status().await),
                    Err(RecvError::Closed) => return,
                };
                let Ok(text) = send(&message) else { continue };
                if socket.send(text).await.is_err() {
                    return;
                }
            }
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => {}
            },
        }
    }
}
