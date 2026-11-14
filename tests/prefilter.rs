//! The streaming pre-filter must not change routing results.
use rustis::{Endpoint, ImportOptions, PlanRequest, Snapshot, import_gtfs_with};
use std::io::Write;
use std::path::Path;
use tempfile::TempDir;

fn fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny")
}

/// Zips the fixture inside a sub-folder (as some publishers do) and adds a
/// shapes.txt plus a trip on a service that is never active.
fn zipped(dir: &Path) -> std::path::PathBuf {
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

fn results(input: &Path, dir: &Path, prefilter: bool) -> (usize, Vec<String>) {
    let dest = dir.join(format!("snap-{prefilter}"));
    let mut o = ImportOptions::for_date("2026-11-16");
    o.prefilter = prefilter;
    let report = import_gtfs_with(input, &dest, &o).unwrap();
    assert_eq!(
        report.notes.iter().any(|n| n.starts_with("prefilter")),
        prefilter
    );
    let s = Snapshot::open(&dest).unwrap();
    let mut out = Vec::new();
    for (from, to) in [("A", "C"), ("A", "B"), ("D", "C")] {
        for dep in [0, 8 * 3600, 24 * 3600, 26 * 3600] {
            let r = s
                .plan(&PlanRequest {
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
                })
                .unwrap();
            out.push(serde_json::to_string(&r.itineraries).unwrap());
        }
    }
    (s.stats().trips, out)
}

#[test]
fn prefilter_preserves_results_for_directories_and_zips() {
    let tmp = TempDir::new().unwrap();
    let zip = zipped(tmp.path());
    for input in [fixture(), zip] {
        let a = results(&input, tmp.path(), true);
        let b = results(&input, tmp.path(), false);
        assert_eq!(a, b, "{}", input.display());
        assert!(a.1.iter().all(|r| !r.contains("GHOST")));
        for e in std::fs::read_dir(tmp.path()).unwrap() {
            let n = e.unwrap().file_name().into_string().unwrap();
            assert!(!n.contains("prefilter"), "work dir left behind: {n}");
        }
        std::fs::remove_dir_all(tmp.path().join("snap-true")).unwrap();
        std::fs::remove_dir_all(tmp.path().join("snap-false")).unwrap();
    }
}

/// Adds a GTFS-Flex trip (stop_times row without stop_id but with a
/// location group and pickup window) that would otherwise be fastest.
fn flex_feed(dir: &Path) -> std::path::PathBuf {
    let out = dir.join("flex");
    std::fs::create_dir_all(&out).unwrap();
    for e in std::fs::read_dir(fixture()).unwrap() {
        let e = e.unwrap();
        std::fs::copy(e.path(), out.join(e.file_name())).unwrap();
    }
    let st = std::fs::read_to_string(fixture().join("stop_times.txt")).unwrap();
    let mut body = String::from(
        "trip_id,arrival_time,departure_time,stop_id,stop_sequence,pickup_type,drop_off_type,location_group_id,start_pickup_drop_off_window,end_pickup_drop_off_window\n",
    );
    for l in st.lines().skip(1) {
        body += &format!("{l},,,\n");
    }
    body += "FLEX,08:00:00,08:00:00,A,1,0,0,,,\nFLEX,,,,2,0,0,zone1,08:00:00,09:00:00\nFLEX,08:05:00,08:05:00,C,3,0,0,,,\n";
    std::fs::write(out.join("stop_times.txt"), body).unwrap();
    let trips =
        std::fs::read_to_string(fixture().join("trips.txt")).unwrap() + "RS,daily,FLEX,Flex\n";
    std::fs::write(out.join("trips.txt"), trips).unwrap();
    out
}

#[test]
fn flex_trips_are_rejected_by_default_and_excluded_on_request() {
    let tmp = TempDir::new().unwrap();
    let src = flex_feed(tmp.path());
    let e = format!(
        "{:#}",
        import_gtfs_with(
            &src,
            &tmp.path().join("a"),
            &ImportOptions::for_date("2026-11-16")
        )
        .unwrap_err()
    );
    assert!(e.contains("GTFS-Flex"), "{e}");
    assert!(!tmp.path().join("a").exists());
    let mut o = ImportOptions::for_date("2026-11-16");
    o.exclude_flex_trips = true;
    import_gtfs_with(&src, &tmp.path().join("b"), &o).unwrap();
    let s = Snapshot::open(&tmp.path().join("b")).unwrap();
    let r = s
        .plan(&PlanRequest {
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
            max_results: 10,
        })
        .unwrap();
    let json = serde_json::to_string(&r.itineraries).unwrap();
    assert!(!json.contains("FLEX") && json.contains("T2"), "{json}");
    assert!(
        r.warnings.iter().any(|w| w.contains("GTFS-Flex")),
        "{:?}",
        r.warnings
    );
}

#[test]
fn frequency_trips_are_rejected_by_default_and_excluded_on_request() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("freq");
    std::fs::create_dir_all(&src).unwrap();
    for e in std::fs::read_dir(fixture()).unwrap() {
        let e = e.unwrap();
        std::fs::copy(e.path(), src.join(e.file_name())).unwrap();
    }
    std::fs::write(
        src.join("frequencies.txt"),
        "trip_id,start_time,end_time,headway_secs\nSLOW,06:00:00,09:00:00,600\n",
    )
    .unwrap();
    let e = format!(
        "{:#}",
        import_gtfs_with(
            &src,
            &tmp.path().join("a"),
            &ImportOptions::for_date("2026-11-16")
        )
        .unwrap_err()
    );
    assert!(
        e.contains("frequency") && e.contains("--exclude-frequency-trips"),
        "{e}"
    );
    let mut o = ImportOptions::for_date("2026-11-16");
    o.exclude_frequency_trips = true;
    import_gtfs_with(&src, &tmp.path().join("b"), &o).unwrap();
    let s = Snapshot::open(&tmp.path().join("b")).unwrap();
    let r = s
        .plan(&PlanRequest {
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
            max_results: 10,
        })
        .unwrap();
    assert!(
        !serde_json::to_string(&r.itineraries)
            .unwrap()
            .contains("SLOW")
    );
    assert!(
        r.warnings.iter().any(|w| w.contains("frequency")),
        "{:?}",
        r.warnings
    );
}

#[test]
fn full_stop_catalogue_is_kept_including_unserved_stops() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("feed");
    std::fs::create_dir_all(&src).unwrap();
    for e in std::fs::read_dir(fixture()).unwrap() {
        let e = e.unwrap();
        std::fs::copy(e.path(), src.join(e.file_name())).unwrap();
    }
    let stops = std::fs::read_to_string(fixture().join("stops.txt")).unwrap()
        + "X,Underpass,50.0105,8.0105\nZ,Lonely,51.0,9.0\n";
    std::fs::write(src.join("stops.txt"), stops).unwrap();
    std::fs::write(
        src.join("transfers.txt"),
        "from_stop_id,to_stop_id,transfer_type,min_transfer_time\nB,X,2,60\nX,D,2,60\n",
    )
    .unwrap();
    for prefilter in [true, false] {
        let dest = tmp.path().join(format!("s{prefilter}"));
        let mut o = ImportOptions::for_date("2026-11-16");
        o.prefilter = prefilter;
        import_gtfs_with(&src, &dest, &o).unwrap();
        let s = Snapshot::open(&dest).unwrap();
        assert_eq!(s.stats().stops, 6, "every stop is kept, also the unserved Z");
        let r = s
            .plan(&PlanRequest {
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
                max_results: 10,
            })
            .unwrap();
        assert_eq!(
            r.itineraries[0].arrival,
            8 * 3600 + 1500,
            "chain B→X→D kept"
        );
    }
}
