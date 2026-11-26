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
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use tokio::sync::Semaphore;

/// The served snapshot; can be switched while serving. Requests keep the
/// `Arc` they started with, so an old snapshot is unmapped once its last
/// request finished.
pub struct SnapshotHandle {
    current: RwLock<Arc<Snapshot>>,
    /// Path watched for a new snapshot (e.g. `ROOT/current`, a symlink).
    watch: Option<PathBuf>,
    max_labels: usize,
    status: RwLock<ReloadStatus>,
}

#[derive(Clone, Default, serde::Serialize)]
struct ReloadStatus {
    /// Resolved directory of the served snapshot.
    snapshot: PathBuf,
    loaded_at: String,
    reloads: u64,
    last_error: Option<String>,
}

impl SnapshotHandle {
    pub fn new(snapshot: Snapshot, watch: Option<PathBuf>, max_labels: usize) -> Self {
        let mut snapshot = snapshot;
        snapshot.set_max_labels(max_labels);
        let snapshot_dir = snapshot.dir().to_owned();
        Self {
            current: RwLock::new(Arc::new(snapshot)),
            watch,
            max_labels,
            status: RwLock::new(ReloadStatus {
                snapshot: std::fs::canonicalize(snapshot_dir.as_path())
                    .unwrap_or(snapshot_dir.clone()),
                loaded_at: jiff::Timestamp::now().to_string(),
                ..Default::default()
            }),
        }
    }

    pub fn get(&self) -> Arc<Snapshot> {
        self.current.read().unwrap().clone()
    }

    /// Opens the snapshot the watched path resolves to if it differs from
    /// the served one. On failure the served snapshot stays (error recorded).
    pub fn reload_if_changed(&self) -> anyhow::Result<bool> {
        let Some(w) = &self.watch else {
            return Ok(false);
        };
        let r = (|| {
            let target = std::fs::canonicalize(w)?;
            if target == self.status.read().unwrap().snapshot {
                return Ok(false);
            }
            let mut s = Snapshot::open(&target)?;
            s.set_max_labels(self.max_labels);
            *self.current.write().unwrap() = Arc::new(s);
            eprintln!("reloaded snapshot {}", target.display());
            self.status.write().unwrap().snapshot = target;
            Ok(true)
        })();
        let mut st = self.status.write().unwrap();
        match &r {
            Ok(true) => {
                st.loaded_at = jiff::Timestamp::now().to_string();
                st.reloads += 1;
                st.last_error = None;
            }
            Ok(false) => {}
            Err(e) => {
                let msg = format!("{e:#}");
                if st.last_error.as_deref() != Some(msg.as_str()) {
                    eprintln!("snapshot reload failed, keeping the served one: {msg}");
                }
                st.last_error = Some(msg);
            }
        }
        r
    }
}

#[derive(Clone)]
struct AppState {
    handle: Arc<SnapshotHandle>,
    permits: Arc<Semaphore>,
}

/// Builds the router. `max_concurrency` bounds simultaneous plan/bbox queries;
/// excess requests get `503` instead of queueing unboundedly.
pub fn router(snapshot: Arc<Snapshot>, max_concurrency: usize) -> Router {
    let snapshot =
        Arc::try_unwrap(snapshot).unwrap_or_else(|_| panic!("router needs the only reference"));
    let labels = snapshot.max_labels();
    router_with(
        Arc::new(SnapshotHandle::new(snapshot, None, labels)),
        max_concurrency,
    )
}

/// Router over a reloadable snapshot.
pub fn router_with(handle: Arc<SnapshotHandle>, max_concurrency: usize) -> Router {
    let state = AppState {
        handle,
        permits: Arc::new(Semaphore::new(max_concurrency.max(1))),
    };
    Router::new()
        .route("/health", get(health))
        .route("/v1/feeds", get(feeds))
        .route("/v1/debug/transfer", get(transfer))
        .route("/v1/stats", get(stats))
        .route("/v1/stops", get(stops))
        .route("/v1/plan", post(plan))
        .route("/api/v6/plan", get(motis_plan_get).post(motis_plan_post))
        .route("/api/v6/map/stops", get(motis_stops))
        .route("/api/v1/map/stops", get(motis_stops))
        .route("/api/v1/geocode", get(motis_geocode))
        .route("/api/v6/stoptimes", get(motis_stoptimes))
        .route("/api/v5/stoptimes", get(motis_stoptimes))
        .route("/api/v6/trip", get(motis_trip))
        .route("/api/v5/trip", get(motis_trip))
        .route("/api/v1/trip-by-name", get(motis_trip_by_name))
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
    let snap = s.handle.get();
    let reload = s.handle.status.read().unwrap().clone();
    Json(json!({
        "status": "ok",
        "snapshot": reload.snapshot.file_name().map(|f| f.to_string_lossy().into_owned()),
                "first_day": snap.first_day(),
        "horizon_days": snap.meta().horizon_days,
        "reload": reload,
        "realtime": snap.rt_stats(),
        "warnings": snap.warnings(),
        "feed_warnings": snap.feed_warnings(7),
    }))
    .into_response()
}

async fn feeds(State(s): State<AppState>) -> Response {
    Json(json!({ "feeds": s.handle.get().feeds() })).into_response()
}

#[derive(Deserialize)]
struct TransferQuery {
    from: String,
    to: String,
}

async fn transfer(State(s): State<AppState>, Query(q): Query<TransferQuery>) -> Response {
    match s.handle.get().transfer_info(&q.from, &q.to) {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn stats(State(s): State<AppState>) -> Response {
    Json(s.handle.get().stats()).into_response()
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
    let snap = s.handle.get();
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

type MotisQuery<T> = Result<Query<T>, axum::extract::rejection::QueryRejection>;

async fn motis_plan_get(
    State(s): State<AppState>,
    q: MotisQuery<crate::motis::PlanQuery>,
) -> Response {
    motis_plan_run(s, q, Bytes::new()).await
}

async fn motis_plan_post(
    State(s): State<AppState>,
    q: MotisQuery<crate::motis::PlanQuery>,
    body: Bytes,
) -> Response {
    motis_plan_run(s, q, body).await
}

async fn motis_plan_run(
    s: AppState,
    q: MotisQuery<crate::motis::PlanQuery>,
    body: Bytes,
) -> Response {
    let Query(q) = match q {
        Ok(q) => q,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
    };
    let body: crate::motis::PlanBody = if body.iter().all(u8::is_ascii_whitespace) {
        Default::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(b) => b,
            Err(e) => return error(StatusCode::BAD_REQUEST, format!("invalid JSON body: {e}")),
        }
    };
    match run_blocking(&s, move |snap| crate::motis::plan(snap, &q, &body)).await {
        Ok(v) => Json(v).into_response(),
        Err(r) => r,
    }
}

async fn motis_stops(
    State(s): State<AppState>,
    q: MotisQuery<crate::motis::StopsQuery>,
) -> Response {
    let Query(q) = match q {
        Ok(q) => q,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
    };
    match run_blocking(&s, move |snap| crate::motis::stops(snap, &q)).await {
        Ok(v) => Json(v).into_response(),
        Err(r) => r,
    }
}

macro_rules! motis_get {
    ($name:ident, $q:ty, $f:path) => {
        async fn $name(State(s): State<AppState>, q: MotisQuery<$q>) -> Response {
            let Query(q) = match q {
                Ok(q) => q,
                Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
            };
            match run_blocking(&s, move |snap| $f(snap, &q)).await {
                Ok(v) => Json(v).into_response(),
                Err(r) => r,
            }
        }
    };
}
motis_get!(
    motis_geocode,
    crate::motis::GeocodeQuery,
    crate::motis::geocode
);
motis_get!(
    motis_stoptimes,
    crate::motis::StopTimesQuery,
    crate::motis::stoptimes
);
motis_get!(motis_trip, crate::motis::TripQuery, crate::motis::trip);
motis_get!(
    motis_trip_by_name,
    crate::motis::TripByNameQuery,
    crate::motis::trip_by_name
);
