//! Demand-responsive stops (GTFS pickup/drop_off type 2/3) and unknown values.
use rustis::{
    BuildOptions, Endpoint, LegKind, PlanRequest, PlanResponse, Snapshot, import_gtfs_with,
};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Adds trip BOOK (A 08:01 → C 08:18) with the given pickup type at A and
/// drop-off type at C. It beats every regular journey (08:25 / 08:40).
fn import(pickup: &str, drop_off: &str, opts: &BuildOptions) -> (TempDir, Snapshot) {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny");
    let tmp = TempDir::new().unwrap();
    let out: PathBuf = tmp.path().join("feed");
    std::fs::create_dir_all(&out).unwrap();
    for e in std::fs::read_dir(&fixture).unwrap() {
        let e = e.unwrap();
        std::fs::copy(e.path(), out.join(e.file_name())).unwrap();
    }
    let read = |n: &str| std::fs::read_to_string(fixture.join(n)).unwrap();
    std::fs::write(
        out.join("routes.txt"),
        read("routes.txt") + "RB,test,B,On demand,3\n",
    )
    .unwrap();
    std::fs::write(
        out.join("trips.txt"),
        read("trips.txt") + "RB,daily,BOOK,Destination\n",
    )
    .unwrap();
    std::fs::write(
        out.join("stop_times.txt"),
        read("stop_times.txt")
            + &format!(
                "BOOK,08:01:00,08:01:00,A,1,{pickup},0\nBOOK,08:18:00,08:18:00,C,2,0,{drop_off}\n"
            ),
    )
    .unwrap();
    let dest = tmp.path().join("snapshot");
    import_gtfs_with(&out, &dest, opts).unwrap();
    let s = Snapshot::open(&dest).unwrap();
    (tmp, s)
}

fn plan(s: &Snapshot) -> PlanResponse {
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
            max_travel_seconds: 86_400,
            alternatives_within_seconds: 3600,
        })
        .unwrap();
    assert_eq!(r.omitted_unreconstructable, 0);
    r
}

fn first_trip(r: &PlanResponse) -> &str {
    r.itineraries[0]
        .legs
        .iter()
        .find(|l| l.kind == LegKind::Transit)
        .unwrap()
        .trip_id
        .as_deref()
        .unwrap()
}

#[test]
fn booking_pickup_is_routed_but_marked() {
    for code in ["2", "3"] {
        let (_t, s) = import(code, "0", &BuildOptions::for_date("2026-11-16"));
        let r = plan(&s);
        assert_eq!(first_trip(&r), "BOOK");
        let it = &r.itineraries[0];
        assert!(it.booking_required);
        let leg = it.legs.iter().find(|l| l.kind == LegKind::Transit).unwrap();
        assert!(leg.pickup_booking_required && !leg.drop_off_booking_required);
        assert!(
            r.warnings.iter().any(|w| w.contains("booking")),
            "{:?}",
            r.warnings
        );
        // Regular alternatives stay unmarked.
        assert!(r.itineraries[1..].iter().all(|i| !i.booking_required));
    }
}

#[test]
fn booking_drop_off_is_marked() {
    let (_t, s) = import("0", "3", &BuildOptions::for_date("2026-11-16"));
    let r = plan(&s);
    let leg = r.itineraries[0]
        .legs
        .iter()
        .find(|l| l.kind == LegKind::Transit)
        .unwrap();
    assert_eq!(leg.trip_id.as_deref(), Some("BOOK"));
    assert!(leg.drop_off_booking_required && !leg.pickup_booking_required);
}

#[test]
fn booking_stops_can_be_excluded() {
    let mut o = BuildOptions::for_date("2026-11-16");
    o.exclude_booking_stops = true;
    for (p, d) in [("2", "0"), ("0", "2"), ("3", "0"), ("0", "3")] {
        let (_t, s) = import(p, d, &o);
        let r = plan(&s);
        assert_ne!(first_trip(&r), "BOOK", "pickup {p} drop_off {d}");
        assert!(r.itineraries.iter().all(|i| !i.booking_required));
        assert!(r.warnings.is_empty());
    }
}

#[test]
fn regular_journeys_have_no_booking_warning() {
    let (_t, s) = import("1", "0", &BuildOptions::for_date("2026-11-16"));
    let r = plan(&s);
    assert_ne!(first_trip(&r), "BOOK");
    assert!(r.warnings.is_empty() && r.itineraries.iter().all(|i| !i.booking_required));
}
