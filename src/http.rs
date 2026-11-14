//! Loopback HTTP API. Routing runs on the blocking pool behind a semaphore so
//! concurrent RAPTOR scratch allocations stay bounded.
use crate::{Error, PlanRequest, Snapshot};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Semaphore;

#[derive(Clone)]
struct AppState {
    snapshot: Arc<Snapshot>,
    permits: Arc<Semaphore>,
}

/// Builds the router. `max_concurrency` bounds simultaneous plan/bbox queries;
/// excess requests get `503` instead of queueing unboundedly.
pub fn router(snapshot: Arc<Snapshot>, max_concurrency: usize) -> Router {
    let state = AppState {
        snapshot,
        permits: Arc::new(Semaphore::new(max_concurrency.max(1))),
    };
    Router::new()
        .route("/health", get(health))
        .route("/v1/stats", get(stats))
        .route("/v1/stops", get(stops))
        .route("/v1/plan", post(plan))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .with_state(state)
}

fn error(status: StatusCode, msg: impl std::fmt::Display) -> Response {
    (status, Json(json!({ "error": msg.to_string() }))).into_response()
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        match self {
            Error::Invalid(m) => error(StatusCode::BAD_REQUEST, m),
            Error::Internal(e) => error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
        }
    }
}

async fn health(State(s): State<AppState>) -> Response {
    Json(json!({ "status": "ok", "first_day": s.snapshot.first_day(), "horizon_days": s.snapshot.meta().horizon_days, "warnings": s.snapshot.warnings() })).into_response()
}

async fn stats(State(s): State<AppState>) -> Response {
    Json(s.snapshot.stats()).into_response()
}

#[derive(Deserialize)]
struct BboxQuery {
    min_lat: f64,
    min_lon: f64,
    max_lat: f64,
    max_lon: f64,
    #[serde(default = "default_limit")]
    limit: usize,
    /// Only stops served by at least one trip of the horizon.
    #[serde(default)]
    served_only: bool,
}
fn default_limit() -> usize {
    100
}

#[allow(clippy::result_large_err)]
async fn run_blocking<T: Send + 'static>(
    s: &AppState,
    f: impl FnOnce(&Snapshot) -> Result<T, Error> + Send + 'static,
) -> Result<T, Response> {
    let Ok(permit) = s.permits.clone().try_acquire_owned() else {
        return Err(error(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many concurrent queries",
        ));
    };
    let snap = s.snapshot.clone();
    let out = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        f(&snap)
    })
    .await
    .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    out.map_err(IntoResponse::into_response)
}

async fn stops(
    State(s): State<AppState>,
    q: Result<Query<BboxQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let Query(q) = match q {
        Ok(q) => q,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
    };
    match run_blocking(&s, move |snap| {
        snap.stops_in_bbox(
            [q.min_lat, q.min_lon],
            [q.max_lat, q.max_lon],
            q.limit,
            q.served_only,
        )
    })
    .await
    {
        Ok(stops) => Json(json!({ "stops": stops })).into_response(),
        Err(r) => r,
    }
}

async fn plan(State(s): State<AppState>, body: Bytes) -> Response {
    let req: PlanRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return error(
                StatusCode::BAD_REQUEST,
                format!("invalid JSON request: {e}"),
            );
        }
    };
    match run_blocking(&s, move |snap| snap.plan(&req)).await {
        Ok(resp) => Json(resp).into_response(),
        Err(r) => r,
    }
}
