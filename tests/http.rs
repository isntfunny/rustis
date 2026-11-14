use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustis::{Snapshot, import_gtfs};
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

fn app() -> (TempDir, axum::Router) {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("snapshot");
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny");
    import_gtfs(&fixture, &path, "2026-11-16").unwrap();
    let app = rustis::http::router(Arc::new(Snapshot::open(&path).unwrap()), 1);
    (tmp, app)
}

async fn call(app: &axum::Router, req: Request<Body>) -> (StatusCode, serde_json::Value) {
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}

fn post(body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/plan")
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

#[tokio::test]
async fn plan_returns_itineraries_with_legs() {
    let (_tmp, app) = app();
    let (status, json) = call(
        &app,
        post(r#"{"date":"2026-11-16","departure":28800,"from":[{"stop_id":"A"}],"to":[{"stop_id":"C","walk_seconds":60}]}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["timezone"], "Europe/Berlin");
    let first = &json["itineraries"][0];
    assert_eq!(first["arrival"], 30300 + 60);
    assert_eq!(first["legs"][0]["kind"], "transit");
    assert_eq!(first["legs"][0]["trip_id"], "T1");
    assert_eq!(first["legs"][3]["kind"], "egress");
}

#[tokio::test]
async fn plan_validation_errors_are_400_with_message() {
    let (_tmp, app) = app();
    for body in [
        r#"{"date":"wrong","departure":28800,"from":[{"stop_id":"A"}],"to":[{"stop_id":"C"}]}"#,
        r#"{"date":"2026-11-16","departure":28800,"from":[{"stop_id":"nope"}],"to":[{"stop_id":"C"}]}"#,
        r#"{"date":"2026-11-16","departure":28800,"from":[{"stop_id":"A"}],"to":[{"stop_id":"C"}],"extra":1}"#,
        r#"{"date":"2026-11-16","#,
    ] {
        let (status, json) = call(&app, post(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(json["error"].as_str().is_some_and(|e| !e.is_empty()));
    }
}

#[tokio::test]
async fn health_stats_and_bbox_endpoints() {
    let (_tmp, app) = app();
    let (status, json) = call(&app, get("/health")).await;
    assert_eq!(
        (status, json["service_date"].as_str()),
        (StatusCode::OK, Some("2026-11-16"))
    );
    let (status, json) = call(&app, get("/v1/stats")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(json["mapped_bytes"].as_u64().unwrap() > 0);
    let (status, json) = call(
        &app,
        get("/v1/stops?min_lat=49.99&min_lon=7.99&max_lat=50.005&max_lon=8.005"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["stops"][0]["stop_id"], "A");
    for uri in [
        "/v1/stops?min_lat=NaN&min_lon=0&max_lat=90&max_lon=180",
        "/v1/stops?min_lat=1",
        "/v1/stops?min_lat=0&min_lon=0&max_lat=1&max_lon=1&limit=100000",
    ] {
        let (status, _) = call(&app, get(uri)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
    }
}
