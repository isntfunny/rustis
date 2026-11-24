//! Station search, departure boards, trips and train-number search
//! (`/api/v1/geocode`, `/api/v6/stoptimes`, `/api/v6/trip`,
//! `/api/v1/trip-by-name`).
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
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/station");
    import_gtfs(&fixture, &path, "2026-11-16").unwrap();
    let app = rustis::http::router(Arc::new(Snapshot::open(&path).unwrap()), 1);
    (tmp, app)
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn geocode_returns_one_entry_per_station() {
    let (_tmp, app) = app();
    for text in ["ulm hbf", "Ulm Hauptbahnhof", "ULM", "ulm h"] {
        let (status, json) = get(
            &app,
            &format!("/api/v1/geocode?text={}", text.replace(' ', "%20")),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        let m = json.as_array().unwrap();
        assert_eq!(m[0]["id"], "ULM", "{text}: {json}");
        assert_eq!(m[0]["name"], "Ulm Hbf");
        assert_eq!(m[0]["type"], "STOP");
        // platforms and the duplicate stop are part of the station
        assert_eq!(
            m.iter()
                .filter(|x| x["name"].as_str().unwrap().starts_with("Ulm"))
                .count(),
            1,
            "{text}: {json}"
        );
    }
    let (_, json) = get(&app, "/api/v1/geocode?text=Freital%20Hainsberg").await;
    assert_eq!(json[0]["id"], "FH");
    let (_, json) = get(&app, "/api/v1/geocode?text=hainsb").await;
    assert_eq!(json[0]["id"], "FH");
    let (_, json) = get(&app, "/api/v1/geocode?text=neu%20ulm").await;
    assert_eq!(json[0]["id"], "NU");
    let (_, json) = get(&app, "/api/v1/geocode?text=nowhere").await;
    assert_eq!(json.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn departure_board_covers_the_whole_station() {
    let (_tmp, app) = app();
    // 09:30 local
    let (status, json) = get(
        &app,
        "/api/v6/stoptimes?stopId=ULM&time=2026-11-16T08:30:00Z&n=3",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let st = json["stopTimes"].as_array().unwrap();
    assert_eq!(st.len(), 3, "{json}");
    assert_eq!(st[0]["tripShortName"], "930");
    assert_eq!(st[0]["displayName"], "ICE");
    assert_eq!(st[0]["headsign"], "Dresden Hbf");
    assert_eq!(st[0]["mode"], "REGIONAL_RAIL");
    assert_eq!(st[0]["place"]["departure"], "2026-11-16T09:00:00Z");
    assert_eq!(st[0]["place"]["track"], "1");
    assert!(st[0]["place"].get("arrival").is_none());
    assert_eq!(st[0]["tripTo"]["name"], "Freital-Hainsberg");
    assert_eq!(st[1]["displayName"], "RE1");
    assert_eq!(st[1]["place"]["track"], "2");
    assert_eq!(st[1]["headsign"], "Freital-Hainsberg");
    assert_eq!(st[1]["deutschlandticket"], false); // not a de- feed
    assert_eq!(st[2]["place"]["stopId"], "ULMX");
    assert_eq!(st[2]["place"]["departure"], "2026-11-16T09:30:00Z");
    let trip_id = st[0]["tripId"].as_str().unwrap();
    assert_eq!(trip_id, "20261116_10:00_ICE930");

    // later on: the next day's trips
    let (_, json) = get(
        &app,
        "/api/v6/stoptimes?stopId=ULM2&time=2026-11-16T10:00:00Z&n=2",
    )
    .await;
    let st = json["stopTimes"].as_array().unwrap();
    assert_eq!(st[0]["place"]["departure"], "2026-11-17T09:00:00Z");
    assert_eq!(st[0]["tripId"], "20261117_10:00_ICE930");

    // arrivals
    let (_, json) = get(
        &app,
        "/api/v6/stoptimes?stopId=FH&time=2026-11-16T12:00:00Z&n=2&arriveBy=true",
    )
    .await;
    let st = json["stopTimes"].as_array().unwrap();
    assert_eq!(st[0]["place"]["arrival"], "2026-11-16T13:00:00Z");
    assert_eq!(st[1]["place"]["arrival"], "2026-11-16T14:00:00Z");

    let (status, _) = get(&app, "/api/v6/stoptimes?stopId=nope").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn trip_lists_every_stop() {
    let (_tmp, app) = app();
    let (status, json) = get(&app, "/api/v6/trip?tripId=20261116_10:00_ICE930").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let leg = &json["legs"][0];
    assert_eq!(leg["from"]["stopId"], "ULM1");
    assert_eq!(leg["from"]["departure"], "2026-11-16T09:00:00Z");
    assert_eq!(leg["to"]["arrival"], "2026-11-16T13:00:00Z");
    let mid = leg["intermediateStops"].as_array().unwrap();
    assert_eq!(mid.len(), 1);
    assert_eq!(mid[0]["name"], "Neu-Ulm");
    assert_eq!(mid[0]["arrival"], "2026-11-16T09:10:00Z");
    assert_eq!(mid[0]["departure"], "2026-11-16T09:11:00Z");
    assert_eq!(json["duration"], 4 * 3600);
    // bare id + date
    let (status, json) = get(&app, "/api/v6/trip?tripId=ICE930&date=2026-11-18").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["legs"][0]["tripId"], "20261118_10:00_ICE930");
    let (status, _) = get(&app, "/api/v6/trip?tripId=20261116_11:00_ICE930").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn trips_by_train_name() {
    let (_tmp, app) = app();
    for name in ["ICE%20930", "ice930", "930"] {
        let (status, json) = get(
            &app,
            &format!("/api/v1/trip-by-name?name={name}&date=2026-11-16"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        let t = json["trips"].as_array().unwrap();
        assert_eq!(t.len(), 1, "{name}: {json}");
        assert_eq!(t[0]["tripId"], "20261116_10:00_ICE930");
        assert_eq!(t[0]["from"]["name"], "Ulm Hbf");
        assert_eq!(t[0]["to"]["arrival"], "2026-11-16T13:00:00Z");
    }
    let (_, json) = get(
        &app,
        "/api/v1/trip-by-name?name=ICE%20930&date=2026-11-16&stopId=NU",
    )
    .await;
    assert_eq!(
        json["trips"][0]["stop"]["departure"],
        "2026-11-16T09:11:00Z"
    );
    let (_, json) = get(
        &app,
        "/api/v1/trip-by-name?name=RE1%204711&date=2026-11-16&stopId=FH",
    )
    .await;
    assert_eq!(json["trips"][0]["stop"]["arrival"], "2026-11-16T14:00:00Z");
    let (_, json) = get(&app, "/api/v1/trip-by-name?name=ICE%20931&date=2026-11-16").await;
    assert_eq!(json["trips"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn plan_from_a_station_uses_all_its_stops() {
    let (_tmp, app) = app();
    // the parent station has no trips itself; the RE leaves from a platform
    let (status, json) = get(
        &app,
        "/api/v6/plan?fromPlace=ULM&toPlace=FH&time=2026-11-16T09:01:00Z",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let it = &json["itineraries"][0];
    assert_eq!(it["legs"][0]["displayName"], "RE1");
    assert_eq!(it["legs"][0]["tripId"], "20261116_10:05_RE1A");
}

#[tokio::test]
async fn implausible_transfers_are_dropped() {
    let (_tmp, app) = app();
    // 600 km in 3 minutes: a data error, not a footpath
    let (_, json) = get(&app, "/v1/debug/transfer?from=ULM1&to=FAR").await;
    assert!(json["footpath_secs"].is_null(), "{json}");
    let (_, json) = get(&app, "/v1/debug/transfer?from=ULM1&to=ULM2").await;
    assert_eq!(json["footpath_secs"], 120, "{json}");
}
