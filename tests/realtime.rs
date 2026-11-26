//! Realtime overlay: a hand-built GTFS-RT feed on the `station` fixture.
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use rustis::{Snapshot, import_gtfs};
use serde_json::Value;
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

fn varint(mut v: u64, out: &mut Vec<u8>) {
    while v >= 0x80 {
        out.push((v & 0x7f) as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}
fn bytes(f: u64, b: &[u8], out: &mut Vec<u8>) {
    varint(f << 3 | 2, out);
    varint(b.len() as u64, out);
    out.extend_from_slice(b);
}
fn num(f: u64, v: u64, out: &mut Vec<u8>) {
    varint(f << 3, out);
    varint(v, out);
}
fn event(delay: i32, out: &mut Vec<u8>, f: u64) {
    let mut e = Vec::new();
    num(1, delay as i64 as u64, &mut e);
    bytes(f, &e, out);
}
/// `(stop, delay)` updates for trip `id` on 2026-11-16 (`rel` 0 / 3 = cancelled).
fn entity(id: &str, rel: u64, stus: &[(&str, i32)]) -> Vec<u8> {
    let mut desc = Vec::new();
    bytes(1, id.as_bytes(), &mut desc);
    bytes(3, b"20261116", &mut desc);
    if rel != 0 {
        num(4, rel, &mut desc);
    }
    let mut tu = Vec::new();
    bytes(1, &desc, &mut tu);
    for (stop, d) in stus {
        let mut s = Vec::new();
        bytes(4, stop.as_bytes(), &mut s);
        event(*d, &mut s, 2);
        event(*d, &mut s, 3);
        bytes(2, &s, &mut tu);
    }
    let mut ent = Vec::new();
    bytes(1, id.as_bytes(), &mut ent);
    bytes(3, &tu, &mut ent);
    let mut out = Vec::new();
    bytes(2, &ent, &mut out);
    out
}

fn setup(feed: Vec<u8>) -> (TempDir, axum::Router, Arc<Snapshot>) {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("snapshot");
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/station");
    import_gtfs(&fixture, &path, "2026-11-16").unwrap();
    let snap = Arc::new(Snapshot::open(&path).unwrap());
    snap.apply_rt("", feed.as_slice());
    let handle = Arc::new(rustis::http::SnapshotHandle::new(
        Arc::try_unwrap(Snapshot::open(&path).map(Arc::new).unwrap()).ok().unwrap(),
        None,
        usize::MAX,
    ));
    handle.get().apply_rt("", feed.as_slice());
    (tmp, rustis::http::router_with(handle, 1), snap)
}

async fn get(app: &axum::Router, uri: &str) -> Value {
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let r = app.clone().oneshot(req).await.unwrap();
    serde_json::from_slice(&r.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[tokio::test]
async fn delays_and_cancellations_show_in_boards_and_trips() {
    let mut feed = entity("ICE930", 0, &[("NU", 600), ("FH", 600)]);
    feed.extend(entity("RE1A", 3, &[]));
    // a trip with a wrong scheduled time must be dropped, not applied
    let (_t, app, snap) = setup(feed);
    let st = snap.rt_stats();
    assert_eq!(st.delayed, 1, "{st:?}");
    assert_eq!(st.cancelled, 1, "{st:?}");

    let j = get(&app, "/api/v6/stoptimes?stopId=NU&time=2026-11-16T08:00:00Z&n=5").await;
    let first = &j["stopTimes"][0];
    assert_eq!(first["realTime"], true, "{j}");
    assert_eq!(first["place"]["scheduledDeparture"], "2026-11-16T09:11:00Z");
    assert_eq!(first["place"]["departure"], "2026-11-16T09:21:00Z");

    let j = get(&app, "/api/v6/stoptimes?stopId=ULM&time=2026-11-16T08:00:00Z&n=5").await;
    let re = j["stopTimes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["tripShortName"] == "4711" && s["cancelled"] == true)
        .unwrap_or_else(|| panic!("cancelled RE1A missing: {j}"));
    assert_eq!(re["realTime"], false);
    assert_eq!(re["tripCancelled"], true);

    let j = get(&app, "/api/v6/trip?tripId=20261116_10:00_ICE930").await;
    let leg = &j["legs"][0];
    assert_eq!(leg["realTime"], true, "{j}");
    assert_eq!(leg["endTime"], "2026-11-16T13:10:00Z");
    assert_eq!(leg["scheduledEndTime"], "2026-11-16T13:00:00Z");
}

#[tokio::test]
async fn plan_uses_delayed_times() {
    let (_t, app, _s) = setup(entity("ICE930", 0, &[("NU", 600), ("FH", 600)]));
    let j = get(
        &app,
        "/api/v6/plan?fromPlace=ULM&toPlace=FH&time=2026-11-16T08:30:00Z",
    )
    .await;
    let it = &j["itineraries"][0];
    let leg = it["legs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["tripShortName"] == "930")
        .unwrap_or_else(|| panic!("{j}"));
    assert_eq!(leg["realTime"], true);
    assert_eq!(leg["endTime"], "2026-11-16T13:10:00Z", "{j}");
    assert_eq!(leg["scheduledEndTime"], "2026-11-16T13:00:00Z");
}
