use rustis::{Endpoint, LegKind, PlanRequest, PlanResponse, Snapshot, import_gtfs};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny")
}

/// Copies the fixture into `dir`, overriding/adding files; `""` removes a file.
fn feed(dir: &Path, overrides: &[(&str, &str)]) -> PathBuf {
    let out = dir.join("feed");
    std::fs::create_dir_all(&out).unwrap();
    for e in std::fs::read_dir(fixture()).unwrap() {
        let e = e.unwrap();
        std::fs::copy(e.path(), out.join(e.file_name())).unwrap();
    }
    for (name, body) in overrides {
        if body.is_empty() {
            std::fs::remove_file(out.join(name)).unwrap();
        } else {
            std::fs::write(out.join(name), body).unwrap();
        }
    }
    out
}

fn request() -> PlanRequest {
    PlanRequest {
        date: "2026-11-16".into(),
        departure: 8 * 3600,
        from: vec![Endpoint {
            stop_id: "A".into(),
            walk_seconds: 0,
        }],
        to: vec![Endpoint {
            stop_id: "C".into(),
            walk_seconds: 0,
        }],
        max_transfers: 3,
        max_results: 5,
    }
}

fn import_feed(overrides: &[(&str, &str)]) -> (TempDir, Snapshot) {
    let tmp = TempDir::new().unwrap();
    let src = feed(tmp.path(), overrides);
    let dest = tmp.path().join("snapshot");
    import_gtfs(&src, &dest, "2026-11-16").unwrap();
    // Serving must not depend on the GTFS source.
    std::fs::remove_dir_all(&src).unwrap();
    let snapshot = Snapshot::open(&dest).unwrap();
    (tmp, snapshot)
}

fn imported() -> (TempDir, Snapshot) {
    import_feed(&[])
}

fn transit_trips(r: &PlanResponse, i: usize) -> Vec<&str> {
    r.itineraries[i]
        .legs
        .iter()
        .filter(|l| l.kind == LegKind::Transit)
        .map(|l| l.trip_id.as_deref().unwrap())
        .collect()
}

#[test]
fn restart_from_disk_finds_transfer_and_slower_direct_alternative() {
    let (_tmp, s) = imported();
    let r = s.plan(&request()).unwrap();
    assert_eq!(r.itineraries.len(), 2, "{r:#?}");
    assert_eq!(r.omitted_unreconstructable, 0);
    let fast = &r.itineraries[0];
    assert_eq!(
        (fast.departure, fast.arrival, fast.transfers),
        (8 * 3600, 8 * 3600 + 25 * 60, 1)
    );
    assert_eq!(fast.departure_time, "2026-11-16T08:00:00+01:00");
    assert_eq!(fast.arrival_time, "2026-11-16T08:25:00+01:00");
    assert_eq!(transit_trips(&r, 0), ["T1", "T2"]);
    let kinds: Vec<_> = fast.legs.iter().map(|l| l.kind).collect();
    assert_eq!(kinds, [LegKind::Transit, LegKind::Walk, LegKind::Transit]);
    let walk = &fast.legs[1];
    assert_eq!(
        (
            walk.from.as_ref().unwrap().stop_id.as_str(),
            walk.to.as_ref().unwrap().stop_id.as_str()
        ),
        ("B", "D")
    );
    assert_eq!(
        (walk.departure, walk.arrival, walk.duration_seconds),
        (8 * 3600 + 600, 8 * 3600 + 720, 120)
    );
    assert_eq!(fast.legs[0].line.as_deref(), Some("1"));
    assert_eq!(fast.legs[0].route_id.as_deref(), Some("R1"));
    assert_eq!(fast.legs[0].geometry, vec![[50.0, 8.0], [50.01, 8.01]]);

    // Direct ride: leaves 08:05 (the actual departure, not the query time).
    let direct = &r.itineraries[1];
    assert_eq!(
        (direct.departure, direct.arrival, direct.transfers),
        (8 * 3600 + 300, 8 * 3600 + 2400, 0)
    );
    assert_eq!(transit_trips(&r, 1), ["SLOW"]);
    assert!(s.stats().mapped_bytes > 0);
}

#[test]
fn access_and_egress_offsets_shift_boarding_and_arrival() {
    let (_tmp, s) = imported();
    let mut q = request();
    q.from[0].walk_seconds = 60; // reaches A at 08:01: T1 (08:00) is gone
    q.to[0].walk_seconds = 180;
    let r = s.plan(&q).unwrap();
    assert_eq!(r.itineraries.len(), 1, "{r:#?}");
    let j = &r.itineraries[0];
    // Leave at 08:04 to walk 60 s to the 08:05 SLOW trip; arrive C 08:40 + 3 min.
    assert_eq!(
        (j.departure, j.arrival),
        (8 * 3600 + 240, 8 * 3600 + 2400 + 180)
    );
    let kinds: Vec<_> = j.legs.iter().map(|l| l.kind).collect();
    assert_eq!(kinds, [LegKind::Access, LegKind::Transit, LegKind::Egress]);
    assert!(j.legs[0].from.is_none() && j.legs[2].to.is_none());
    assert_eq!(j.legs[2].duration_seconds, 180);
}

#[test]
fn multiple_origins_pick_the_one_with_the_better_journey() {
    let (_tmp, s) = imported();
    let mut q = request();
    q.from = vec![
        Endpoint {
            stop_id: "A".into(),
            walk_seconds: 600,
        },
        Endpoint {
            stop_id: "D".into(),
            walk_seconds: 300,
        },
    ];
    let r = s.plan(&q).unwrap();
    let best = &r.itineraries[0];
    assert_eq!(transit_trips(&r, 0), ["T2"]);
    assert_eq!(best.legs[0].kind, LegKind::Access);
    assert_eq!(best.legs[0].to.as_ref().unwrap().stop_id, "D");
    assert_eq!(
        (best.departure, best.arrival),
        (8 * 3600 + 600, 8 * 3600 + 1500)
    );
}

#[test]
fn transfer_limit_excludes_two_vehicle_journey() {
    let (_tmp, s) = imported();
    let mut q = request();
    q.max_transfers = 0;
    let r = s.plan(&q).unwrap();
    assert_eq!(r.itineraries.len(), 1);
    assert_eq!(transit_trips(&r, 0), ["SLOW"]);
}

#[test]
fn pickup_and_drop_off_restrictions_are_respected() {
    // NOPICK (no boarding at A, would arrive 08:20) and NODROP (no alighting
    // at C, 08:22) would both beat 08:25 if restrictions were ignored.
    let (_tmp, s) = imported();
    let r = s.plan(&request()).unwrap();
    assert_eq!(r.itineraries[0].arrival, 8 * 3600 + 1500);
    let all: Vec<&str> = (0..r.itineraries.len())
        .flat_map(|i| transit_trips(&r, i))
        .collect();
    assert!(
        !all.contains(&"NOPICK") && !all.contains(&"NODROP"),
        "{all:?}"
    );
}

#[test]
fn overnight_times_and_calendar_exceptions_are_preserved() {
    let (_tmp, s) = imported();
    let mut q = request();
    q.departure = 24 * 3600;
    let r = s.plan(&q).unwrap();
    assert_eq!(r.itineraries.len(), 1);
    let j = &r.itineraries[0];
    assert_eq!(
        (j.departure, j.arrival),
        (25 * 3600 + 600, 25 * 3600 + 1800)
    );
    assert_eq!(j.departure_time, "2026-11-17T01:10:00+01:00");
    assert_eq!(transit_trips(&r, 0), ["NIGHT"]);
    q.departure = 26 * 3600;
    assert!(
        s.plan(&q).unwrap().itineraries.is_empty(),
        "Tuesday 17.11. is removed by calendar_dates"
    );
}

#[test]
fn next_day_trips_are_routable_when_service_runs() {
    let (_tmp, s) = import_feed(&[("calendar_dates.txt", "")]);
    let mut q = request();
    q.departure = 26 * 3600;
    let r = s.plan(&q).unwrap();
    assert_eq!(transit_trips(&r, 0), ["T1", "T2"]);
    assert_eq!(r.itineraries[0].arrival_time, "2026-11-17T08:25:00+01:00");
}

#[test]
fn multi_hop_footpaths_are_reconstructed_as_separate_walk_legs() {
    let (_tmp, s) = import_feed(&[
        (
            "stops.txt",
            "stop_id,stop_name,stop_lat,stop_lon\nA,Origin,50.000,8.000\nB,Interchange west,50.010,8.010\nC,Destination,50.020,8.020\nD,Interchange east,50.011,8.011\nX,Underpass,50.0105,8.0105\n",
        ),
        (
            "transfers.txt",
            "from_stop_id,to_stop_id,transfer_type,min_transfer_time\nB,X,2,60\nX,D,2,60\n",
        ),
    ]);
    let r = s.plan(&request()).unwrap();
    assert_eq!(r.omitted_unreconstructable, 0);
    assert_eq!(transit_trips(&r, 0), ["T1", "T2"]);
    let walks: Vec<_> = r.itineraries[0]
        .legs
        .iter()
        .filter(|l| l.kind == LegKind::Walk)
        .map(|l| {
            (
                l.from.as_ref().unwrap().stop_id.as_str(),
                l.to.as_ref().unwrap().stop_id.as_str(),
                l.arrival,
            )
        })
        .collect();
    assert_eq!(
        walks,
        [("B", "X", 8 * 3600 + 660), ("X", "D", 8 * 3600 + 720)]
    );
}

#[test]
fn equal_time_trips_are_all_exported() {
    let extra = "\nTWIN,08:00:00,08:00:00,A,1,0,0\nTWIN,08:10:00,08:10:00,B,2,0,0\n";
    let st =
        std::fs::read_to_string(fixture().join("stop_times.txt")).unwrap() + extra.trim_start();
    let trips = std::fs::read_to_string(fixture().join("trips.txt")).unwrap()
        + "R1,daily,TWIN,Interchange\n";
    let (_tmp, s) = import_feed(&[("stop_times.txt", &st), ("trips.txt", &trips)]);
    // Monday only (Tuesday removed): 7 trips.
    assert_eq!(s.stats().trips, 7);
    s.verify().unwrap();
}

#[test]
fn invalid_requests_fail_explicitly() {
    let (_tmp, s) = imported();
    let mut q = request();
    q.date = "2026-11-17".into();
    let e = s.plan(&q).unwrap_err().to_string();
    assert!(e.contains("not covered"), "{e}");
    q = request();
    q.from[0].stop_id = "missing".into();
    assert!(
        s.plan(&q)
            .unwrap_err()
            .to_string()
            .contains("unknown stop_id")
    );
    q = request();
    q.max_transfers = 255;
    assert!(s.plan(&q).is_err());
    q = request();
    q.max_results = 0;
    assert!(s.plan(&q).is_err());
    q = request();
    q.departure = 48 * 3600;
    assert!(s.plan(&q).is_err());
    q = request();
    q.to.clear();
    assert!(s.plan(&q).is_err());
    q = request();
    q.from = (0..65)
        .map(|_| Endpoint {
            stop_id: "A".into(),
            walk_seconds: 0,
        })
        .collect();
    assert!(s.plan(&q).is_err());
    assert!(s.stops_in_bbox([50.0, 8.0], [49.0, 9.0], 10).is_err());
    assert!(s.stops_in_bbox([f64::NAN, 8.0], [51.0, 9.0], 10).is_err());
    assert!(s.stops_in_bbox([49.0, 7.0], [51.0, 9.0], 0).is_err());
}

#[test]
fn unreachable_destination_returns_no_itineraries() {
    let (_tmp, s) = imported();
    let mut q = request();
    q.from[0].stop_id = "C".into();
    q.to[0].stop_id = "A".into();
    let r = s.plan(&q).unwrap();
    assert!(r.itineraries.is_empty());
}

fn make_writable(p: &Path) {
    let mut perm = std::fs::metadata(p).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perm.set_readonly(false);
    std::fs::set_permissions(p, perm).unwrap();
}

#[test]
fn snapshot_files_are_read_only_and_truncation_is_rejected() {
    let (tmp, s) = imported();
    drop(s);
    let bin = tmp.path().join("snapshot/timetable.bin");
    assert!(std::fs::metadata(&bin).unwrap().permissions().readonly());
    make_writable(&bin);
    let full = std::fs::read(&bin).unwrap();
    std::fs::write(&bin, &full[..full.len() - 8]).unwrap();
    assert!(Snapshot::open(&tmp.path().join("snapshot")).is_err());
    std::fs::write(&bin, [0u8; 3]).unwrap();
    assert!(Snapshot::open(&tmp.path().join("snapshot")).is_err());
}

#[test]
fn corrupted_payload_is_detected_by_verify() {
    let (tmp, s) = imported();
    drop(s);
    let bin = tmp.path().join("snapshot/timetable.bin");
    make_writable(&bin);
    let mut data = std::fs::read(&bin).unwrap();
    let n = data.len();
    data[n - 1] ^= 0xff; // last byte of the trip-id table
    std::fs::write(&bin, data).unwrap();
    let s = Snapshot::open(&tmp.path().join("snapshot")).unwrap();
    assert!(s.verify().unwrap_err().to_string().contains("checksum"));
}

#[test]
fn structurally_inconsistent_directory_is_rejected() {
    let (tmp, s) = imported();
    drop(s);
    let bin = tmp.path().join("snapshot/timetable.bin");
    make_writable(&bin);
    let mut data = std::fs::read(&bin).unwrap();
    // First directory entry's element count (RouteStopOffsets): claim one more.
    let at = 16 + 16;
    let count = u64::from_le_bytes(data[at..at + 8].try_into().unwrap());
    data[at..at + 8].copy_from_slice(&(count + 1).to_le_bytes());
    std::fs::write(&bin, data).unwrap();
    assert!(Snapshot::open(&tmp.path().join("snapshot")).is_err());
}

#[test]
fn import_never_overwrites_an_existing_snapshot() {
    let tmp = TempDir::new().unwrap();
    let dest = tmp.path().join("snapshot");
    import_gtfs(&fixture(), &dest, "2026-11-16").unwrap();
    let before = std::fs::read(dest.join("metadata.json")).unwrap();
    assert!(import_gtfs(&fixture(), &dest, "2026-11-18").is_err());
    assert_eq!(std::fs::read(dest.join("metadata.json")).unwrap(), before);
    let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(
        leftovers.len(),
        1,
        "no staging directories left: {leftovers:?}"
    );
}

#[test]
fn unsupported_feeds_fail_closed_without_leaving_output() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "frequencies.txt",
            "trip_id,start_time,end_time,headway_secs\nT1,06:00:00,09:00:00,600\n",
            "frequency",
        ),
        (
            "transfers.txt",
            "from_stop_id,to_stop_id,transfer_type,min_transfer_time\nB,D,1,\n",
            "type 1",
        ),
        (
            "transfers.txt",
            "from_stop_id,to_stop_id,transfer_type,min_transfer_time,from_trip_id,to_trip_id\nB,D,2,120,T1,T2\n",
            "trip/route-specific",
        ),
        (
            "transfers.txt",
            "from_stop_id,to_stop_id,transfer_type\nB,D,4\n",
            "type 4",
        ),
        (
            "agency.txt",
            "agency_id,agency_name,agency_url,agency_timezone\ntest,A,https://a.invalid,Europe/Berlin\nother,B,https://b.invalid,Europe/London\n",
            "timezone",
        ),
    ];
    for (file, body, needle) in cases {
        let tmp = TempDir::new().unwrap();
        let src = feed(tmp.path(), &[(file, body)]);
        let dest = tmp.path().join("snapshot");
        let err = format!("{:#}", import_gtfs(&src, &dest, "2026-11-16").unwrap_err());
        assert!(err.contains(needle), "{file}: expected {needle:?} in {err}");
        assert!(!dest.exists());
    }
}

#[test]
fn dst_transition_and_inactive_dates_are_rejected() {
    let tmp = TempDir::new().unwrap();
    let err = format!(
        "{:#}",
        import_gtfs(&fixture(), &tmp.path().join("a"), "2026-10-24").unwrap_err()
    );
    assert!(err.contains("DST"), "{err}");
    let err = format!(
        "{:#}",
        import_gtfs(&fixture(), &tmp.path().join("b"), "2027-03-01").unwrap_err()
    );
    assert!(err.contains("no trips"), "{err}");
}

#[test]
fn bbox_search_returns_served_stops_and_respects_limit() {
    let (_tmp, s) = imported();
    let stops = s.stops_in_bbox([49.99, 7.99], [50.005, 8.005], 10).unwrap();
    assert_eq!(stops.len(), 1);
    assert_eq!(
        (stops[0].stop_id.as_str(), stops[0].name.as_str()),
        ("A", "Origin")
    );
    assert_eq!(
        s.stops_in_bbox([49.0, 7.0], [51.0, 9.0], 2).unwrap().len(),
        2
    );
    assert_eq!(
        s.stops_in_bbox([49.0, 7.0], [51.0, 9.0], 100)
            .unwrap()
            .len(),
        4
    );
}

fn import_with(overrides: &[(&str, &str)], opts: &rustis::ImportOptions) -> (TempDir, Snapshot) {
    let tmp = TempDir::new().unwrap();
    let src = feed(tmp.path(), overrides);
    let dest = tmp.path().join("snapshot");
    rustis::import_gtfs_with(&src, &dest, opts).unwrap();
    let s = Snapshot::open(&dest).unwrap();
    (tmp, s)
}

#[test]
fn dropped_trip_specific_transfers_never_become_footpaths() {
    // Vulture alone would turn this guaranteed-connection row into a 300 s
    // B→D footpath; Rustis must drop it entirely in degraded mode.
    let transfers = "from_stop_id,to_stop_id,transfer_type,min_transfer_time,from_trip_id,to_trip_id\nB,D,1,,T1,T2\n";
    let mut opts = rustis::ImportOptions::for_date("2026-11-16");
    opts.drop_relaxing_transfers = true;
    let (_tmp, s) = import_with(&[("transfers.txt", transfers)], &opts);
    let r = s.plan(&request()).unwrap();
    assert_eq!(r.itineraries.len(), 1);
    assert_eq!(transit_trips(&r, 0), ["SLOW"]);
    assert!(
        s.import_notes()
            .iter()
            .any(|n| n.starts_with("CONSERVATIVE")),
        "{:?}",
        s.import_notes()
    );
}

#[test]
fn walk_radius_adds_footpaths_but_respects_impossible_transfers() {
    let mut opts = rustis::ImportOptions::for_date("2026-11-16");
    opts.walk_radius_m = Some(500.0);
    let no_rules = "from_stop_id,to_stop_id,transfer_type,min_transfer_time\n";
    let (_tmp, s) = import_with(&[("transfers.txt", no_rules)], &opts);
    let r = s.plan(&request()).unwrap();
    assert_eq!(transit_trips(&r, 0), ["T1", "T2"]);
    let walk = r.itineraries[0]
        .legs
        .iter()
        .find(|l| l.kind == LegKind::Walk)
        .unwrap();
    // B (50.010, 8.010) → D (50.011, 8.011): ≈133 m at 1.2 m/s.
    assert!(
        (100..=130).contains(&walk.duration_seconds),
        "{}",
        walk.duration_seconds
    );

    let forbidden = "from_stop_id,to_stop_id,transfer_type,min_transfer_time\nB,D,3,\n";
    let (_tmp, s) = import_with(&[("transfers.txt", forbidden)], &opts);
    let r = s.plan(&request()).unwrap();
    assert_eq!(
        transit_trips(&r, 0),
        ["SLOW"],
        "type 3 must block the generated walk"
    );
}

#[test]
fn later_trip_is_used_when_the_earliest_trip_forbids_alighting() {
    // RP: NODROP 08:03 (no drop-off at C) and LATE 08:10 → C 08:24 on the same
    // stop pattern. RAPTOR only scans the earliest catchable trip per route;
    // without pattern splitting LATE would never be considered.
    let st = std::fs::read_to_string(fixture().join("stop_times.txt")).unwrap()
        + "LATE,08:10:00,08:10:00,A,1,0,0\nLATE,08:24:00,08:24:00,C,2,0,0\n";
    let trips = std::fs::read_to_string(fixture().join("trips.txt")).unwrap()
        + "RP,daily,LATE,Destination\n";
    let (_tmp, s) = import_feed(&[("stop_times.txt", &st), ("trips.txt", &trips)]);
    let r = s.plan(&request()).unwrap();
    assert_eq!(r.omitted_unreconstructable, 0);
    assert_eq!(transit_trips(&r, 0), ["LATE"], "{r:#?}");
    assert_eq!(r.itineraries[0].arrival, 8 * 3600 + 24 * 60);
}
