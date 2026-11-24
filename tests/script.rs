//! MOTIS dataset scripts (`script:`) run at import (nigiri's Lua API).
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use rustis::build::BuildOptions;
use rustis::{Snapshot, import_gtfs_with};
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

const SCRIPT: &str = r#"
local util = require 'scripts.util'

function process_route(route)
  if route:get_short_name() == 'S' then
    return false
  end
  if route:get_short_name() == '1' then
    route:set_route_type(101)
    if route:get_color() == 0 then
      route:set_color(0xf7931d)
    end
  end
end

function process_trip(trip)
  if trip:get_id() == 'T2' then
    error('broken rule')
  end
  if trip:get_route():get_agency():get_name() == 'Rustis Test'
     and trip:get_route():get_route_type() == 101 then
    trip:set_short_name('ICE ' .. util.strip_zeros(trip:get_short_name()))
    trip:set_display_name(trip:get_short_name())
  end
end
"#;

fn feed_with_script(tmp: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny");
    let feed = tmp.join("feed");
    std::fs::create_dir_all(&feed).unwrap();
    for e in std::fs::read_dir(&fixture).unwrap() {
        let p = e.unwrap().path();
        std::fs::copy(&p, feed.join(p.file_name().unwrap())).unwrap();
    }
    std::fs::write(
        feed.join("trips.txt"),
        "route_id,service_id,trip_id,trip_headsign,trip_short_name\n\
         R1,daily,T1,Interchange,000119\n\
         R2,daily,T2,Destination,77\n\
         RS,daily,SLOW,Destination,\n\
         RN,daily,NIGHT,Destination,\n\
         RP,daily,NOPICK,Destination,\n\
         RP,daily,NODROP,Destination,\n",
    )
    .unwrap();
    // config dir layout as in a MOTIS setup: <dir>/scripts/<name>.lua
    let scripts = tmp.join("cfg/scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::write(scripts.join("feed.lua"), SCRIPT).unwrap();
    std::fs::write(
        scripts.join("util.lua"),
        "return { strip_zeros = function(s) return string.format('%d', tonumber(s)) end }",
    )
    .unwrap();
    (feed, scripts.join("feed.lua"))
}

async fn plan(snap: Snapshot, uri: &str) -> Value {
    let app = rustis::http::router(Arc::new(snap), 1);
    let r = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    serde_json::from_slice(&r.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[tokio::test]
async fn script_sets_train_number_mode_colour_and_drops_routes() {
    let tmp = TempDir::new().unwrap();
    let (feed, script) = feed_with_script(tmp.path());
    let mut opts = BuildOptions::for_date("2026-11-16");
    opts.scripts.insert(String::new(), script);
    let dest = tmp.path().join("snap");
    let m = import_gtfs_with(&feed, &dest, &opts).unwrap();
    let skipped = &m.feeds[0].skipped;
    assert_eq!(
        skipped.get("script_dropped_routes"),
        Some(&1),
        "{skipped:?}"
    );
    assert_eq!(skipped.get("script_errors"), Some(&1), "{skipped:?}");

    let json = plan(
        Snapshot::open(&dest).unwrap(),
        "/api/v6/plan?fromPlace=A&toPlace=C&time=2026-11-16T07:00:00Z&numItineraries=5",
    )
    .await;
    let its = json["itineraries"].as_array().unwrap();
    assert!(!its.is_empty(), "{json}");
    let rides: Vec<&Value> = its
        .iter()
        .flat_map(|i| i["legs"].as_array().unwrap())
        .filter(|l| l["mode"] != "WALK")
        .collect();
    // the dropped route S never appears
    assert!(rides.iter().all(|l| l["routeShortName"] != "S"), "{json}");
    let ice = rides.iter().find(|l| l["routeShortName"] == "1").unwrap();
    assert_eq!(ice["displayName"], "ICE 119");
    assert_eq!(ice["tripShortName"], "ICE 119");
    assert_eq!(ice["mode"], "HIGHSPEED_RAIL");
    assert_eq!(ice["routeColor"], "F7931D");
    // the trip whose call failed is imported unchanged (display = line)
    let bus = rides.iter().find(|l| l["routeShortName"] == "2").unwrap();
    assert_eq!(bus["displayName"], "2");
    assert_eq!(bus["tripShortName"], "77");
}

#[test]
fn without_script_display_name_is_the_line() {
    let tmp = TempDir::new().unwrap();
    let (feed, _) = feed_with_script(tmp.path());
    let dest = tmp.path().join("snap");
    import_gtfs_with(&feed, &dest, &BuildOptions::for_date("2026-11-16")).unwrap();
    let snap = Snapshot::open(&dest).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let json = rt.block_on(plan(
        snap,
        "/api/v6/plan?fromPlace=A&toPlace=C&time=2026-11-16T07:00:00Z",
    ));
    let first = &json["itineraries"][0]["legs"][0];
    assert_eq!(first["displayName"], "1");
    assert_eq!(first["tripShortName"], "000119");
    assert_eq!(first["mode"], "BUS");
}

#[test]
fn missing_script_is_reported_and_feed_imported() {
    let tmp = TempDir::new().unwrap();
    let (feed, _) = feed_with_script(tmp.path());
    let mut opts = BuildOptions::for_date("2026-11-16");
    opts.scripts
        .insert(String::new(), tmp.path().join("cfg/scripts/missing.lua"));
    let m = import_gtfs_with(&feed, &tmp.path().join("snap"), &opts).unwrap();
    assert!(m.feeds[0].imported);
    assert_eq!(m.feeds[0].skipped.get("script_load_failed"), Some(&1));
}

#[test]
fn motis_config_scripts_resolve_against_the_config_dir() {
    let cfg = "timetable:\n  datasets:\n    de-DELFI:\n      path: gtfs/de_DELFI.gtfs.zip\n      script: scripts/de-DELFI.lua\n    de-VBB:\n      path: gtfs/de_VBB.gtfs.zip\n";
    let m = rustis::inventory::motis_scripts(cfg, Path::new("/data"));
    assert_eq!(m.len(), 1);
    assert_eq!(m["de-DELFI"], Path::new("/data/scripts/de-DELFI.lua"));
}
