//! MOTIS v6 compatible endpoints (`/api/v6/plan`, `/api/v6/map/stops`).
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustis::{Snapshot, import_gtfs};
use serde_json::Value;
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

async fn call(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn post(uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

/// 08:00 Europe/Berlin on 2026-11-16.
const T0: &str = "2026-11-16T07:00:00Z";

#[tokio::test]
async fn plan_between_stops_has_motis_shape() {
    let (_tmp, app) = app();
    let (status, json) = call(
        &app,
        get(&format!(
            "/api/v6/plan?fromPlace=A&toPlace=C&time={T0}&numItineraries=3"
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    for key in ["from", "to", "direct", "itineraries", "nextPageCursor"] {
        assert!(json.get(key).is_some(), "missing {key}");
    }
    let its = json["itineraries"].as_array().unwrap();
    assert!(!its.is_empty() && its.len() <= 3);
    // earliest arrival: bus 1 to B, walk to D, bus 2 to C at 08:25 local
    let best = its
        .iter()
        .min_by_key(|i| i["endTime"].as_str().unwrap().to_owned())
        .unwrap();
    assert_eq!(best["endTime"], "2026-11-16T07:25:00Z");
    assert_eq!(best["transfers"], 1);
    let legs = best["legs"].as_array().unwrap();
    let ride = &legs[0];
    assert_eq!(ride["mode"], "BUS");
    assert_eq!(ride["startTime"], "2026-11-16T07:00:00Z");
    assert_eq!(ride["routeShortName"], "1");
    assert_eq!(ride["displayName"], "1");
    assert_eq!(ride["headsign"], "Interchange");
    assert_eq!(ride["agencyName"], "Rustis Test");
    assert_eq!(ride["from"]["stopId"], "A");
    assert_eq!(ride["from"]["name"], "Origin");
    assert_eq!(ride["legGeometry"]["precision"], 6);
    assert!(!ride["legGeometry"]["points"].as_str().unwrap().is_empty());
    assert_eq!(ride["intermediateStops"].as_array().unwrap().len(), 0);
    assert_eq!(legs[1]["mode"], "WALK");
    assert_eq!(legs[1]["duration"], 120);
    assert_eq!(best["duration"], 25 * 60);
}

#[tokio::test]
async fn post_offsets_add_first_and_last_mile() {
    let (_tmp, app) = app();
    let (status, json) = call(
        &app,
        post(
            "/api/v6/plan?fromPlace=49.999,7.999&toPlace=50.021,8.021&time=2026-11-16T06:55:00Z&numItineraries=1",
            r#"{"fromOffsets":[{"stopId":"A","duration":60,"mode":"WALK"}],
                "toOffsets":[{"stopId":"C","duration":90,"mode":"WALK"}]}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let it = &json["itineraries"][0];
    let legs = it["legs"].as_array().unwrap();
    let (first, last) = (&legs[0], legs.last().unwrap());
    assert_eq!(first["mode"], "WALK");
    assert_eq!(first["duration"], 60);
    assert_eq!(first["from"]["name"], "START");
    assert_eq!(first["from"]["lat"], 49.999);
    assert_eq!(first["to"]["stopId"], "A");
    assert_eq!(last["mode"], "WALK");
    assert_eq!(last["duration"], 90);
    assert_eq!(last["to"]["name"], "END");
    // the first walk ends when bus 1 leaves at 08:00
    assert_eq!(it["startTime"], "2026-11-16T06:59:00Z");
}

#[tokio::test]
async fn coordinates_use_stops_within_radius() {
    let (_tmp, app) = app();
    let uri =
        format!("/api/v6/plan?fromPlace=50.0005,8.0&toPlace=50.0205,8.02&radius=200&time={T0}");
    let (status, json) = call(&app, get(&uri)).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let legs = json["itineraries"][0]["legs"].as_array().unwrap();
    assert_eq!(legs[0]["mode"], "WALK");
    assert_eq!(legs[0]["to"]["stopId"], "A");
    // MOTIS shape: never two WALK legs in a row, never a walk-only itinerary
    for it in json["itineraries"].as_array().unwrap() {
        let modes: Vec<&str> = it["legs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["mode"].as_str().unwrap())
            .collect();
        assert!(modes.iter().any(|m| *m != "WALK"), "{modes:?}");
        assert!(
            !modes.windows(2).any(|w| w == ["WALK", "WALK"]),
            "{modes:?}"
        );
    }
    // no stop within 10 m: empty answer, not an error
    let uri =
        format!("/api/v6/plan?fromPlace=50.0005,8.0&toPlace=50.0205,8.02&radius=10&time={T0}");
    let (status, json) = call(&app, get(&uri)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["itineraries"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn transit_modes_filter_and_unsupported_parameters() {
    let (_tmp, app) = app();
    let (_, json) = call(
        &app,
        get(&format!(
            "/api/v6/plan?fromPlace=A&toPlace=C&time={T0}&transitModes=TRAM,SUBWAY"
        )),
    )
    .await;
    assert_eq!(json["itineraries"].as_array().unwrap().len(), 0);
    let (_, json) = call(
        &app,
        get(&format!(
            "/api/v6/plan?fromPlace=A&toPlace=C&time={T0}&transitModes=BUS,TRAM"
        )),
    )
    .await;
    assert!(!json["itineraries"].as_array().unwrap().is_empty());
    for uri in [
        format!("/api/v6/plan?fromPlace=nope&toPlace=C&time={T0}"),
        "/api/v6/plan?fromPlace=A&toPlace=C&time=yesterday".to_owned(),
    ] {
        let (status, json) = call(&app, get(&uri)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert!(json["error"].as_str().is_some_and(|e| !e.is_empty()));
    }
}

#[tokio::test]
async fn map_stops_lists_served_stops_in_the_box() {
    let (_tmp, app) = app();
    let (status, json) = call(
        &app,
        get("/api/v6/map/stops?min=49.99,7.99&max=50.015,8.015"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let mut ids: Vec<&str> = json
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["stopId"].as_str().unwrap())
        .collect();
    ids.sort();
    assert_eq!(ids, ["A", "B", "D"]);
    let a = json
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["stopId"] == "A")
        .unwrap();
    assert_eq!(
        (a["name"].as_str(), a["lat"].as_f64(), a["lon"].as_f64()),
        (Some("Origin"), Some(50.0), Some(8.0))
    );
    let (status, _) = call(&app, get("/api/v6/map/stops?min=x&max=50,8")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn arrive_by_returns_latest_departures() {
    let (_tmp, app) = app();
    let plan = |t: &str, n: usize| {
        format!("/api/v6/plan?fromPlace=A&toPlace=C&time={t}&arriveBy=true&numItineraries={n}")
    };
    // arrive by 08:30: bus 1 + bus 2 (08:00 → 08:25); the slow bus is too late
    let (status, json) = call(&app, get(&plan("2026-11-16T07:30:00Z", 3))).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let its = json["itineraries"].as_array().unwrap();
    assert!(!its.is_empty());
    assert!(
        its.iter()
            .all(|i| i["endTime"].as_str().unwrap() <= "2026-11-16T07:30:00Z")
    );
    assert_eq!(its.last().unwrap()["startTime"], "2026-11-16T07:00:00Z");
    assert_eq!(its.last().unwrap()["endTime"], "2026-11-16T07:25:00Z");
    // arrive by 08:45: the slow bus leaves latest (08:05)
    let (_, json) = call(&app, get(&plan("2026-11-16T07:45:00Z", 1))).await;
    assert_eq!(
        json["itineraries"][0]["startTime"], "2026-11-16T07:05:00Z",
        "{json}"
    );
    assert_eq!(json["itineraries"][0]["endTime"], "2026-11-16T07:40:00Z");
    // arrive by 07:00: only the night bus of the previous service day
    let (_, json) = call(&app, get(&plan("2026-11-16T06:00:00Z", 1))).await;
    assert_eq!(
        json["itineraries"][0]["startTime"], "2026-11-16T00:10:00Z",
        "{json}"
    );
}
