//! Feed-level import behaviour: ZIP layouts, GTFS-Flex, frequencies, the full
//! stop catalogue and several feeds in one snapshot.
use rustis::{
    BuildOptions, Endpoint, FeedSpec, PlanRequest, PlanResponse, Snapshot, build_snapshot,
    import_gtfs_with,
};
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny")
}

fn copy_fixture(dir: &Path, name: &str) -> PathBuf {
    let out = dir.join(name);
    std::fs::create_dir_all(&out).unwrap();
    for e in std::fs::read_dir(fixture()).unwrap() {
        let e = e.unwrap();
        std::fs::copy(e.path(), out.join(e.file_name())).unwrap();
    }
    out
}

fn req(from: &str, to: &str, dep: u32) -> PlanRequest {
    PlanRequest {
        date: "2026-11-16".into(),
        departure: dep,
        from: vec![Endpoint {
            stop_id: from.into(),
            walk_seconds: 0,
        }],
        to: vec![Endpoint {
            stop_id: to.into(),
            walk_seconds: 0,
        }],
        max_transfers: 3,
        max_results: 10,
        max_travel_seconds: 86_400,
        alternatives_within_seconds: 3600,
    }
}

fn trips(r: &PlanResponse) -> Vec<Vec<String>> {
    r.itineraries
        .iter()
        .map(|i| i.legs.iter().filter_map(|l| l.trip_id.clone()).collect())
        .collect()
}

/// The fixture zipped inside a sub-folder, plus shapes.txt and a trip whose
/// service never runs.
fn zipped(dir: &Path) -> PathBuf {
    let path = dir.join("feed.zip");
    let mut z = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    let opts = zip::write::SimpleFileOptions::default();
    for e in std::fs::read_dir(fixture()).unwrap() {
        let e = e.unwrap();
        let name = e.file_name().into_string().unwrap();
        let mut body = std::fs::read_to_string(e.path()).unwrap();
        match name.as_str() {
            "calendar.txt" => body += "never,0,0,0,0,0,0,0,20261101,20261231\n",
            "trips.txt" => body += "R1,never,GHOST,Nowhere\n",
            "stop_times.txt" => {
                body += "GHOST,08:00:00,08:00:00,A,1,0,0\nGHOST,08:01:00,08:01:00,C,2,0,0\n"
            }
            _ => {}
        }
        z.start_file(format!("gtfs/{name}"), opts).unwrap();
        z.write_all(body.as_bytes()).unwrap();
    }
    z.start_file("gtfs/shapes.txt", opts).unwrap();
    z.write_all(b"shape_id,shape_pt_lat,shape_pt_lon,shape_pt_sequence\nS,50,8,1\n")
        .unwrap();
    z.finish().unwrap();
    path
}

fn all_results(s: &Snapshot) -> Vec<String> {
    let mut out = Vec::new();
    for (from, to) in [("A", "C"), ("A", "B"), ("D", "C")] {
        for dep in [0, 8 * 3600, 24 * 3600, 26 * 3600] {
            let r = s.plan(&req(from, to, dep)).unwrap();
            out.push(serde_json::to_string(&r.itineraries).unwrap());
        }
    }
    out
}

#[test]
fn zip_with_subfolder_and_directory_give_identical_results() {
    let tmp = TempDir::new().unwrap();
    let o = BuildOptions::for_date("2026-11-16");
    import_gtfs_with(&fixture(), &tmp.path().join("a"), &o).unwrap();
    let m = import_gtfs_with(&zipped(tmp.path()), &tmp.path().join("b"), &o).unwrap();
    assert_eq!(
        m.feeds[0].skipped.get("trips_not_running_in_horizon"),
        Some(&1)
    );
    let a = Snapshot::open(&tmp.path().join("a")).unwrap();
    let b = Snapshot::open(&tmp.path().join("b")).unwrap();
    let (ra, rb) = (all_results(&a), all_results(&b));
    assert_eq!(ra, rb);
    assert!(rb.iter().all(|r| !r.contains("GHOST")));
}

#[test]
fn flex_trips_are_skipped_flagged_and_the_rest_is_routed() {
    let tmp = TempDir::new().unwrap();
    let src = copy_fixture(tmp.path(), "flex");
    let st = std::fs::read_to_string(fixture().join("stop_times.txt")).unwrap();
    let mut body = String::from(
        "trip_id,arrival_time,departure_time,stop_id,stop_sequence,pickup_type,drop_off_type,location_group_id,start_pickup_drop_off_window,end_pickup_drop_off_window\n",
    );
    for l in st.lines().skip(1) {
        body += &format!("{l},,,\n");
    }
    body += "FLEX,08:00:00,08:00:00,A,1,0,0,,,\nFLEX,,,,2,0,0,zone1,08:00:00,09:00:00\nFLEX,08:05:00,08:05:00,C,3,0,0,,,\n";
    std::fs::write(src.join("stop_times.txt"), body).unwrap();
    let trips_txt =
        std::fs::read_to_string(fixture().join("trips.txt")).unwrap() + "RS,daily,FLEX,Flex\n";
    std::fs::write(src.join("trips.txt"), trips_txt).unwrap();
    let m = import_gtfs_with(
        &src,
        &tmp.path().join("s"),
        &BuildOptions::for_date("2026-11-16"),
    )
    .unwrap();
    assert_eq!(
        m.feeds[0].skipped.get("flex_or_unknown_stop_trips"),
        Some(&1)
    );
    let s = Snapshot::open(&tmp.path().join("s")).unwrap();
    let r = s.plan(&req("A", "C", 8 * 3600)).unwrap();
    let json = serde_json::to_string(&r.itineraries).unwrap();
    assert!(!json.contains("FLEX") && json.contains("T2"), "{json}");
    assert!(
        r.warnings.iter().any(|w| w.contains("GTFS-Flex")),
        "{:?}",
        r.warnings
    );
}

#[test]
fn frequency_trips_are_expanded_and_routed() {
    let tmp = TempDir::new().unwrap();
    let src = copy_fixture(tmp.path(), "freq");
    std::fs::write(
        src.join("frequencies.txt"),
        "trip_id,start_time,end_time,headway_secs\nSLOW,06:00:00,09:00:00,600\n",
    )
    .unwrap();
    let m = import_gtfs_with(
        &src,
        &tmp.path().join("s"),
        &BuildOptions::for_date("2026-11-16"),
    )
    .unwrap();
    assert_eq!(m.feeds[0].skipped.get("frequency_trips_expanded"), Some(&1));
    let s = Snapshot::open(&tmp.path().join("s")).unwrap();
    // Runs 06:00 … 08:50 every 10 min, 35 min ride: asked at 08:06:40 the
    // 08:10 run is the direct option.
    let r = s.plan(&req("A", "C", 8 * 3600 + 400)).unwrap();
    let direct = r
        .itineraries
        .iter()
        .find(|i| i.transfers == 0)
        .expect("direct frequency run");
    assert_eq!(
        (direct.departure, direct.arrival),
        (8 * 3600 + 600, 8 * 3600 + 600 + 2100)
    );
    assert!(trips(&r).contains(&vec!["SLOW".to_string()]));
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
}

#[test]
fn full_stop_catalogue_is_kept_including_unserved_stops() {
    let tmp = TempDir::new().unwrap();
    let src = copy_fixture(tmp.path(), "feed");
    let stops = std::fs::read_to_string(fixture().join("stops.txt")).unwrap()
        + "X,Underpass,50.0105,8.0105\nZ,Lonely,51.0,9.0\nP,Station,50.0,8.0\n";
    std::fs::write(src.join("stops.txt"), stops).unwrap();
    import_gtfs_with(
        &src,
        &tmp.path().join("s"),
        &BuildOptions::for_date("2026-11-16"),
    )
    .unwrap();
    let s = Snapshot::open(&tmp.path().join("s")).unwrap();
    assert_eq!(s.stats().stops, 7);
    let all = s
        .stops_in_bbox([50.9, 8.9], [51.1, 9.1], 10, false)
        .unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!((all[0].stop_id.as_str(), all[0].served), ("Z", false));
    assert!(
        s.stops_in_bbox([50.9, 8.9], [51.1, 9.1], 10, true)
            .unwrap()
            .is_empty()
    );
    let a = s
        .stops_in_bbox([49.99, 7.99], [50.001, 8.001], 10, true)
        .unwrap();
    assert_eq!(
        a.iter().map(|s| s.stop_id.as_str()).collect::<Vec<_>>(),
        ["A"]
    );
    // Routing to an unserved stop is a valid request without result.
    assert!(
        s.plan(&req("A", "Z", 8 * 3600))
            .unwrap()
            .itineraries
            .is_empty()
    );
}

#[test]
fn several_feeds_are_namespaced_and_linked_by_generated_footpaths() {
    let tmp = TempDir::new().unwrap();
    let a = copy_fixture(tmp.path(), "north");
    // Second feed: one trip from a stop ≈40 m from north:C onwards to E.
    let b = tmp.path().join("south");
    std::fs::create_dir_all(&b).unwrap();
    let w = |n: &str, body: &str| std::fs::write(b.join(n), body).unwrap();
    w(
        "agency.txt",
        "agency_id,agency_name,agency_url,agency_timezone\ns,South,https://s.invalid,Europe/Berlin\n",
    );
    w(
        "stops.txt",
        "stop_id,stop_name,stop_lat,stop_lon\nC,Destination south,50.0203,8.0203\nE,End,50.1,8.1\n",
    );
    w(
        "routes.txt",
        "route_id,agency_id,route_short_name,route_type\nR1,s,99,3\n",
    );
    w("trips.txt", "route_id,service_id,trip_id\nR1,d,T1\n");
    w(
        "calendar.txt",
        "service_id,monday,tuesday,wednesday,thursday,friday,saturday,sunday,start_date,end_date\nd,1,1,1,1,1,1,1,20261101,20261231\n",
    );
    w(
        "stop_times.txt",
        "trip_id,arrival_time,departure_time,stop_id,stop_sequence\nT1,08:30:00,08:30:00,C,1\nT1,09:00:00,09:00:00,E,2\n",
    );
    let mut o = BuildOptions::for_date("2026-11-16");
    o.link_radius_m = 100.0;
    let feeds = [
        FeedSpec {
            name: "north".into(),
            path: a,
        },
        FeedSpec {
            name: "south".into(),
            path: b,
        },
    ];
    let m = build_snapshot(&feeds, &tmp.path().join("s"), o).unwrap();
    assert!(m.feeds.iter().all(|f| f.imported));
    let s = Snapshot::open(&tmp.path().join("s")).unwrap();
    // Equal local ids in both feeds stay distinct.
    let r = s.plan(&req("north:A", "south:E", 8 * 3600)).unwrap();
    let it = &r.itineraries[0];
    assert_eq!(trips(&r)[0], ["north:T1", "north:T2", "south:T1"]);
    assert_eq!(it.arrival, 9 * 3600);
    let walk = it
        .legs
        .iter()
        .rfind(|l| l.kind == rustis::LegKind::Walk)
        .expect("cross-feed walk");
    assert_eq!(
        (
            walk.from.as_ref().unwrap().stop_id.as_str(),
            walk.to.as_ref().unwrap().stop_id.as_str()
        ),
        ("north:C", "south:C")
    );
    // A feed that is not GTFS is reported; the others are imported.
    let broken = tmp.path().join("broken");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(broken.join("trenitalia.xml"), "<netex/>").unwrap();
    let feeds = [
        FeedSpec {
            name: "north".into(),
            path: copy_fixture(tmp.path(), "north2"),
        },
        FeedSpec {
            name: "netex".into(),
            path: broken,
        },
    ];
    let m = build_snapshot(
        &feeds,
        &tmp.path().join("t"),
        BuildOptions::for_date("2026-11-16"),
    )
    .unwrap();
    assert!(m.feeds[0].imported && !m.feeds[1].imported);
    assert!(
        m.feeds[1]
            .error
            .as_deref()
            .unwrap()
            .contains("invalid NeTEx publication")
    );
    assert!(
        m.warnings.iter().any(|w| w.contains("not imported: netex")),
        "{:?}",
        m.warnings
    );
}
