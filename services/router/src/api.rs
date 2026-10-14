use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use router_client::{
    Execution, InstructionsResponse, QuoteRequest, QuoteResponse, WireAccountMeta, WireInstruction,
};
use solana_pubkey::Pubkey;
use std::str::FromStr;
use tracing::warn;

use crate::engine::{self, Book, EngineError, PlannedRoute, Request};
use crate::{AppState, assemble};

pub fn router(state: Arc<AppState>) -> Router {
    let timeout = std::time::Duration::from_millis(state.cfg.request_timeout_ms);
    let concurrency = state.cfg.max_concurrent_requests.max(1);
    Router::new()
        .route("/health", get(health))
        .route("/v1/quote", post(quote))
        .route("/v1/instructions", post(instructions))
        .route("/v1/pools", get(pools))
        .with_state(state)
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            timeout,
        ))
        .layer(tower::limit::ConcurrencyLimitLayer::new(concurrency))
}

async fn health() -> &'static str {
    "ok"
}

async fn pools(State(state): State<Arc<AppState>>) -> Json<Vec<serde_json::Value>> {
    Json(
        state
            .registry
            .all()
            .await
            .iter()
            .map(|entry| {
                serde_json::json!({
                    "venue": entry.kind.as_str(),
                    "pool": entry.pool.to_string(),
                    "program_id": entry.program_id.to_string(),
                    "mint_a": entry.mints.0.to_string(),
                    "mint_b": entry.mints.1.to_string(),
                    "tier": entry.tier,
                })
            })
            .collect(),
    )
}

fn engine_status(error: EngineError) -> (StatusCode, String) {
    match error {
        EngineError::BadAddress(_) | EngineError::Unsupported(_) => {
            (StatusCode::BAD_REQUEST, error.to_string())
        }
        EngineError::NoRoute => (StatusCode::NOT_FOUND, error.to_string()),
        EngineError::Rpc(_) => (StatusCode::BAD_GATEWAY, error.to_string()),
    }
}

fn validate(req: &QuoteRequest) -> Result<(), (StatusCode, String)> {
    if req.slippage_bps >= 10_000 {
        return Err((
            StatusCode::BAD_REQUEST,
            "slippage_bps must be < 10000".into(),
        ));
    }
    if req.amount == 0 {
        return Err((StatusCode::BAD_REQUEST, "amount must be > 0".into()));
    }
    Ok(())
}

fn parse_user(user: &str) -> Result<Pubkey, (StatusCode, String)> {
    Pubkey::from_str(user)
        .map_err(|_| (StatusCode::BAD_REQUEST, format!("bad user address: {user}")))
}

async fn planned(
    state: &AppState,
    req: &QuoteRequest,
) -> Result<
    (
        Request,
        PlannedRoute<crate::Snapshot>,
        Book<crate::Snapshot>,
        u64,
    ),
    (StatusCode, String),
> {
    plan_request(state, req).await
}

fn execution_label(execution: Execution) -> &'static str {
    match execution {
        Execution::Direct => "Direct",
        Execution::Jupiter => "Jupiter",
    }
}

/// Time a whole request (planning, fitting, assembly, and the blockhash
/// fetch) and count its errors by status.
async fn observed<T>(
    endpoint: &'static str,
    execution: Execution,
    work: impl std::future::Future<Output = Result<T, (StatusCode, String)>>,
) -> Result<T, (StatusCode, String)> {
    let started = std::time::Instant::now();
    let result = work.await;
    let execution = execution_label(execution);
    metrics::histogram!("router_quote_seconds", "endpoint" => endpoint, "execution" => execution)
        .record(started.elapsed().as_secs_f64());
    if let Err((status, _)) = &result {
        metrics::counter!(
            "router_quote_errors_total",
            "endpoint" => endpoint,
            "execution" => execution,
            "status" => status.as_u16().to_string()
        )
        .increment(1);
    }
    result
}

async fn plan_request(
    state: &AppState,
    req: &QuoteRequest,
) -> Result<
    (
        Request,
        PlannedRoute<crate::Snapshot>,
        Book<crate::Snapshot>,
        u64,
    ),
    (StatusCode, String),
> {
    validate(req)?;
    let mut request = Request::parse(req).map_err(engine_status)?;
    request.jupiter_overhead_cu = state.jupiter_overhead_cu;
    let (book, state_slot) = crate::load_book(state, &request)
        .await
        .map_err(engine_status)?;
    let planned =
        engine::plan(&request, &book, &state.bridge_mints, state_slot).map_err(engine_status)?;
    metrics::counter!("router_quotes_total", "execution" => execution_label(request.execution))
        .increment(1);
    Ok((request, planned, book, state_slot))
}

/// Plan and assemble for `user`, narrowing the route until it fits one v1
/// transaction (bytes, addresses, and instructions), then build it.
async fn fitted(
    state: &AppState,
    req: &QuoteRequest,
    user: &Pubkey,
) -> Result<(PlannedRoute<crate::Snapshot>, assemble::Assembled, String), (StatusCode, String)> {
    let (mut request, mut planned, book, state_slot) = planned(state, req).await?;
    loop {
        let assembled = assemble::route_instructions(
            state,
            user,
            request.execution,
            request.slippage_bps,
            &planned.segments,
        )
        .map_err(assembly_error)?;
        match assemble::check_fits(state, user, &assembled) {
            Ok(()) => {
                let tx = assemble::route_tx(state, user, &assembled)
                    .await
                    .map_err(assembly_error)?;
                return Ok((planned, assembled, tx));
            }
            Err(error) if assemble::is_oversized(&error) => {
                let widest = planned
                    .segments
                    .iter()
                    .map(|segment| segment.legs.len())
                    .max()
                    .unwrap_or(1);
                if widest > 1 {
                    // First narrow same-pair splits...
                    request.max_split_legs = widest.min(request.max_split_legs) - 1;
                } else {
                    // ...then drop the leg that touches the most accounts.
                    let Some(pool) = widest_leg(user, &planned) else {
                        return Err(assembly_error(error));
                    };
                    request.excluded.insert(pool);
                }
                planned = engine::plan(&request, &book, &state.bridge_mints, state_slot).map_err(
                    |plan_error| match plan_error {
                        EngineError::NoRoute => assembly_error(error),
                        other => engine_status(other),
                    },
                )?;
            }
            Err(error) => return Err(assembly_error(error)),
        }
    }
}

/// Pool of the single-leg segment whose swap instruction lists the most accounts.
fn widest_leg(user: &Pubkey, planned: &PlannedRoute<crate::Snapshot>) -> Option<Pubkey> {
    planned
        .segments
        .iter()
        .flat_map(|segment| &segment.legs)
        .filter_map(|leg| {
            let accounts = leg
                .source
                .swap_instructions(*user, &leg.quote)
                .ok()?
                .last()?
                .accounts
                .len();
            Some((accounts, leg.source.pool()))
        })
        .max_by_key(|(accounts, _)| *accounts)
        .map(|(_, pool)| pool)
}

async fn quote(
    State(state): State<Arc<AppState>>,
    Json(req): Json<QuoteRequest>,
) -> Result<Json<QuoteResponse>, (StatusCode, String)> {
    observed("quote", req.execution, quote_inner(&state, &req)).await
}

async fn quote_inner(
    state: &AppState,
    req: &QuoteRequest,
) -> Result<Json<QuoteResponse>, (StatusCode, String)> {
    let Some(user) = &req.user else {
        return Ok(Json(planned(state, req).await?.1.response));
    };
    let user = parse_user(user)?;
    let (planned, assembled, tx) = fitted(state, req, &user).await?;
    let mut response = planned.response;
    response.compute_unit_limit =
        Some(assemble::Budget::for_route(state, &assembled).compute_unit_limit);
    response.tx_base64 = Some(tx);
    Ok(Json(response))
}

async fn instructions(
    State(state): State<Arc<AppState>>,
    Json(req): Json<QuoteRequest>,
) -> Result<Json<InstructionsResponse>, (StatusCode, String)> {
    observed(
        "instructions",
        req.execution,
        instructions_inner(&state, &req),
    )
    .await
}

async fn instructions_inner(
    state: &AppState,
    req: &QuoteRequest,
) -> Result<Json<InstructionsResponse>, (StatusCode, String)> {
    let Some(user) = &req.user else {
        return Err((
            StatusCode::BAD_REQUEST,
            "/v1/instructions requires `user`".into(),
        ));
    };
    let user = parse_user(user)?;
    let (planned, assembled, _) = fitted(state, req, &user).await?;
    let mut quote = planned.response;
    quote.compute_unit_limit =
        Some(assemble::Budget::for_route(state, &assembled).compute_unit_limit);
    let wire = assembled
        .instructions
        .iter()
        .map(|ix| WireInstruction {
            program_id: ix.program_id.to_string(),
            accounts: ix
                .accounts
                .iter()
                .map(|meta| WireAccountMeta {
                    pubkey: meta.pubkey.to_string(),
                    is_signer: meta.is_signer,
                    is_writable: meta.is_writable,
                })
                .collect(),
            data_base64: base64::engine::general_purpose::STANDARD.encode(&ix.data),
        })
        .collect();
    Ok(Json(InstructionsResponse {
        quote,
        instructions: wire,
    }))
}

fn assembly_error(error: anyhow::Error) -> (StatusCode, String) {
    warn!(error = %error, "route assembly failed");
    let status = match error.downcast_ref::<assemble::AssemblyFault>() {
        Some(assemble::AssemblyFault::Rpc(_)) => StatusCode::BAD_GATEWAY,
        Some(assemble::AssemblyFault::Config(_)) => StatusCode::BAD_REQUEST,
        None => StatusCode::UNPROCESSABLE_ENTITY,
    };
    (status, format!("{error:#}"))
}
