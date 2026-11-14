//! Transfer-rule correctness: minimum change times, forbidden transfers and
//! the conservative `drop_relaxing_transfers` mode.
//!
//! Base network (tests/fixtures/tiny) plus a fast feeder at B:
//!   T1  A 08:00 → B 08:10
//!   T3  B 08:12 → C 08:20   (needs a ≤ 120 s change at B)
//!   T2  D 08:15 → C 08:25   (B→D footpath when transfers.txt provides one)
//!   SLOW A 08:05 → C 08:40
use rustis::{
    Endpoint, ImportOptions, LegKind, PlanRequest, PlanResponse, Snapshot, import_gtfs_with,
};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const H8: u32 = 8 * 3600;

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny")
}

fn feed(dir: &Path, transfers: &str) -> PathBuf {
    let out = dir.join("feed");
    std::fs::create_dir_all(&out).unwrap();
    for e in std::fs::read_dir(fixture()).unwrap() {
        let e = e.unwrap();
        std::fs::copy(e.path(), out.join(e.file_name())).unwrap();
    }
    let read = |n: &str| std::fs::read_to_string(fixture().join(n)).unwrap();
    std::fs::write(
        out.join("routes.txt"),
        read("routes.txt") + "R3,test,3,Feeder,3\n",
    )
    .unwrap();
    std::fs::write(
        out.join("trips.txt"),
        read("trips.txt") + "R3,daily,T3,Destination\n",
    )
    .unwrap();
    std::fs::write(
        out.join("stop_times.txt"),
        read("stop_times.txt") + "T3,08:12:00,08:12:00,B,1,0,0\nT3,08:20:00,08:20:00,C,2,0,0\n",
    )
    .unwrap();
    std::fs::write(out.join("transfers.txt"), transfers).unwrap();
    out
}

fn opts() -> ImportOptions {
    ImportOptions::for_date("2026-11-16")
}

fn try_import(transfers: &str, opts: &ImportOptions) -> (TempDir, anyhow::Result<Snapshot>) {
    let tmp = TempDir::new().unwrap();
    let src = feed(tmp.path(), transfers);
    let dest = tmp.path().join("snapshot");
    let r = import_gtfs_with(&src, &dest, opts).and_then(|_| Snapshot::open(&dest));
    if r.is_err() {
        assert!(!dest.exists(), "failed import must not publish a snapshot");
    }
    (tmp, r)
}

fn import(transfers: &str, opts: &ImportOptions) -> (TempDir, Snapshot) {
    let (tmp, r) = try_import(transfers, opts);
    (tmp, r.unwrap())
}

fn import_err(transfers: &str, opts: &ImportOptions) -> String {
    match try_import(transfers, opts).1 {
        Ok(_) => panic!("import unexpectedly succeeded"),
        Err(e) => format!("{e:#}"),
    }
}

fn plan(s: &Snapshot) -> PlanResponse {
    let r = s
        .plan(&PlanRequest {
            date: "2026-11-16".into(),
            departure: H8,
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
    // The adapter itself must enforce the rules; the reconstruction safety net
    // (which omits inconsistent journeys) must not be what hides a violation.
    assert_eq!(r.omitted_unreconstructable, 0, "{r:#?}");
    r
}

fn trips(r: &PlanResponse) -> Vec<Vec<String>> {
    r.itineraries
        .iter()
        .map(|i| {
            i.legs
                .iter()
                .filter(|l| l.kind == LegKind::Transit)
                .map(|l| l.trip_id.clone().unwrap())
                .collect()
        })
        .collect()
}

/// Every vehicle change in every itinerary must respect `min_change(stop)` for
/// same-stop changes and `walk_min(from, to)` for walked changes.
fn assert_changes_valid(
    r: &PlanResponse,
    min_change: impl Fn(&str) -> u32,
    walk_min: impl Fn(&str, &str) -> Option<u32>,
) {
    for it in &r.itineraries {
        let mut last_alight: Option<(String, u32)> = None;
        let mut walked: Option<(String, u32)> = None;
        for l in &it.legs {
            match l.kind {
                LegKind::Transit => {
                    let board = l.from.as_ref().unwrap().stop_id.clone();
                    if let Some((stop, arr)) = &last_alight {
                        let total = l.departure - arr;
                        if *stop == board {
                            assert!(
                                total >= min_change(stop),
                                "change at {stop} too short: {total} s in {it:#?}"
                            );
                        } else {
                            let need =
                                walk_min(stop, &board).expect("walk without a footpath rule");
                            assert!(total >= need, "walk {stop}->{board}: {total} < {need} s");
                            assert!(walked.as_ref().is_some_and(|(s, _)| *s == board));
                        }
                    }
                    last_alight = Some((l.to.as_ref().unwrap().stop_id.clone(), l.arrival));
                    walked = None;
                }
                LegKind::Walk => walked = Some((l.to.as_ref().unwrap().stop_id.clone(), l.arrival)),
                _ => {}
            }
        }
    }
}

const HEADER: &str = "from_stop_id,to_stop_id,transfer_type,min_transfer_time\n";

#[test]
fn same_stop_minimum_transfer_time_blocks_too_short_change() {
    let (_t, s) = import(&format!("{HEADER}B,B,2,300\n"), &opts());
    let r = plan(&s);
    assert!(
        !trips(&r).contains(&vec!["T1".into(), "T3".into()]),
        "{:?}",
        trips(&r)
    );
    assert_eq!(trips(&r), [["SLOW"]]);
    assert_changes_valid(&r, |s| if s == "B" { 300 } else { 0 }, |_, _| None);
}

#[test]
fn same_stop_minimum_transfer_time_allows_sufficient_change() {
    let (_t, s) = import(&format!("{HEADER}B,B,2,120\n"), &opts());
    let r = plan(&s);
    assert_eq!(trips(&r)[0], ["T1", "T3"]);
    assert_eq!(r.itineraries[0].arrival, H8 + 20 * 60);
}

#[test]
fn default_change_time_applies_where_no_same_stop_rule_exists() {
    let mut o = opts();
    o.default_change_seconds = 180;
    let (_t, s) = import(HEADER, &o);
    let r = plan(&s);
    assert_eq!(trips(&r), [["SLOW"]]);
    // A same-stop rule overrides the default.
    let (_t, s) = import(&format!("{HEADER}B,B,2,60\n"), &o);
    assert_eq!(trips(&plan(&s))[0], ["T1", "T3"]);
    let mut o = opts();
    o.default_change_seconds = 120;
    let (_t, s) = import(HEADER, &o);
    assert_eq!(trips(&plan(&s))[0], ["T1", "T3"]);
}

#[test]
fn zero_change_time_without_rules_is_the_documented_default() {
    let (_t, s) = import(HEADER, &opts());
    assert_eq!(trips(&plan(&s))[0], ["T1", "T3"]);
}

#[test]
fn walked_transfer_keeps_its_min_time_and_same_stop_time_only_delays() {
    // B→D needs 120 s; T2 at D 08:15 is reachable (08:10 + 120 ≤ 08:15).
    let (_t, s) = import(&format!("{HEADER}B,D,2,120\n"), &opts());
    let r = plan(&s);
    assert_eq!(trips(&r)[0], ["T1", "T3"]);
    assert_changes_valid(&r, |_| 0, |a, b| (a == "B" && b == "D").then_some(120));
    // With a 300 s same-stop rule at B the change to T3 is forbidden. The
    // adapter conservatively also delays walking off at B, so T2 is missed:
    // that loses a connection, never admits an invalid one.
    let (_t, s) = import(&format!("{HEADER}B,B,2,300\nB,D,2,120\n"), &opts());
    let r = plan(&s);
    assert_changes_valid(
        &r,
        |s| if s == "B" { 300 } else { 0 },
        |a, b| (a == "B" && b == "D").then_some(120),
    );
    assert!(!trips(&r).contains(&vec!["T1".into(), "T3".into()]));
}

#[test]
fn duplicate_rules_use_the_strictest_time() {
    let (_t, s) = import(
        &format!("{HEADER}B,D,2,120\nB,D,2,600\nB,B,2,60\nB,B,2,300\n"),
        &opts(),
    );
    let r = plan(&s);
    assert_eq!(trips(&r), [["SLOW"]]);
}

#[test]
fn forbidden_transfers_are_enforced_or_rejected() {
    // Distinct stops: no footpath, also not from --walk-radius-m.
    let mut o = opts();
    o.walk_radius_m = Some(500.0);
    let (_t, s) = import(&format!("{HEADER}B,D,3,\n"), &o);
    assert!(!trips(&plan(&s)).contains(&vec!["T1".into(), "T2".into()]));
    // Same stop: the adapter cannot forbid boarding at the arrival stop.
    let e = import_err(&format!("{HEADER}B,B,3,\n"), &opts());
    assert!(e.contains("same-stop transfer_type 3"), "{e}");
    // Reachable through chained footpaths: would circumvent the rule.
    let e = import_err(&format!("{HEADER}B,X,2,30\nX,D,2,30\nB,D,3,\n"), &opts()).to_string();
    assert!(e.contains("X") || e.contains("chain"), "{e}");
}

#[test]
fn min_time_rule_without_time_is_rejected() {
    let e = import_err(&format!("{HEADER}B,D,2,\n"), &opts());
    assert!(e.contains("min_transfer_time"), "{e}");
}

#[test]
fn restricting_trip_specific_rules_are_rejected_even_in_conservative_mode() {
    let mut o = opts();
    o.drop_relaxing_transfers = true;
    let h = "from_stop_id,to_stop_id,transfer_type,min_transfer_time,from_trip_id,to_trip_id,from_route_id,to_route_id\n";
    for row in [
        "B,B,2,600,T1,T3,,",
        "B,B,3,,T1,T3,,",
        "B,B,2,600,,,R1,R3",
        "B,B,3,,,,R1,R3",
    ] {
        let e = import_err(&format!("{h}{row}\n"), &o);
        assert!(e.contains("cannot be enforced"), "{row}: {e}");
        let e = import_err(&format!("{h}{row}\n"), &opts());
        assert!(e.contains("not supported"), "{row}: {e}");
    }
}

#[test]
fn relaxing_rules_are_rejected_by_default_and_dropped_only_on_request() {
    let h = "from_stop_id,to_stop_id,transfer_type,min_transfer_time,from_trip_id,to_trip_id\n";
    for row in [
        "B,D,1,,T1,T2",
        "B,B,4,,T1,T3",
        "B,B,5,,T1,T3",
        "B,D,0,,T1,T2",
        "B,D,1,,,",
        "B,B,4,,,",
    ] {
        let e = import_err(&format!("{h}{row}\n"), &opts());
        assert!(e.contains("not supported"), "{row}: {e}");
    }
    let mut o = opts();
    o.drop_relaxing_transfers = true;
    let (_t, s) = import(&format!("{h}B,D,1,,T1,T2\nB,B,2,300,,\n"), &o);
    let r = plan(&s);
    // Dropping the guaranteed connection must not create a B→D footpath, and
    // the kept same-stop rule still applies.
    assert_eq!(trips(&r), [["SLOW"]]);
    assert!(
        r.warnings.iter().any(|w| w.contains("dropped")),
        "{:?}",
        r.warnings
    );
    assert!(s.stats().transfer_rules.contains("conservative"));
}

#[test]
fn strict_snapshots_carry_no_warning() {
    let (_t, s) = import(HEADER, &opts());
    assert!(plan(&s).warnings.is_empty());
    assert_eq!(s.stats().transfer_rules, "strict");
}

#[test]
fn origin_walks_are_not_delayed_by_the_origin_change_time() {
    // Starting at B (change time 300 s), walking to D takes 120 s and T2
    // leaves D at 08:15: reachable when leaving B at 08:10.
    let (_t, s) = import(&format!("{HEADER}B,B,2,300\nB,D,2,120\n"), &opts());
    let r = s
        .plan(&PlanRequest {
            date: "2026-11-16".into(),
            departure: H8 + 600,
            from: vec![Endpoint {
                stop_id: "B".into(),
                walk_seconds: 0,
            }],
            to: vec![Endpoint {
                stop_id: "C".into(),
                walk_seconds: 0,
            }],
            max_transfers: 0,
            max_results: 3,
        })
        .unwrap();
    // T3 (08:12 at B) is boardable from the origin without any change time.
    assert_eq!(trips(&r)[0], ["T3"]);
    let r2 = s
        .plan(&PlanRequest {
            date: "2026-11-16".into(),
            departure: H8 + 13 * 60,
            from: vec![Endpoint {
                stop_id: "B".into(),
                walk_seconds: 0,
            }],
            to: vec![Endpoint {
                stop_id: "C".into(),
                walk_seconds: 0,
            }],
            max_transfers: 0,
            max_results: 3,
        })
        .unwrap();
    assert_eq!(
        trips(&r2),
        [["T2"]],
        "walk B→D at 08:13 must catch T2 at 08:15 (+120 s)"
    );
}

#[test]
fn reported_arrivals_exclude_the_internal_change_time() {
    let (_t, s) = import(&format!("{HEADER}C,C,2,600\n"), &opts());
    let r = plan(&s);
    assert_eq!(trips(&r)[0], ["T1", "T3"]);
    assert_eq!(r.itineraries[0].arrival, H8 + 20 * 60);
    assert_eq!(r.itineraries[0].arrival_time, "2026-11-16T08:20:00+01:00");
}

fn generalize() -> ImportOptions {
    let mut o = opts();
    o.generalize_restricting_transfers = true;
    o
}

const SPECIFIC: &str = "from_stop_id,to_stop_id,transfer_type,min_transfer_time,from_trip_id,to_trip_id,from_route_id,to_route_id\n";

#[test]
fn generalized_route_specific_same_stop_time_applies_to_every_change() {
    // Only R1→R3 needs 300 s at B; generalised to every change at B.
    let (_t, s) = import(&format!("{SPECIFIC}B,B,2,300,,,R1,R3\n"), &generalize());
    let r = plan(&s);
    assert_eq!(trips(&r), [["SLOW"]]);
    assert!(
        r.warnings.iter().any(|w| w.contains("generalised")),
        "{:?}",
        r.warnings
    );
    assert_changes_valid(&r, |s| if s == "B" { 300 } else { 0 }, |_, _| None);
    // A more permissive specific time never lowers a stop-level rule.
    let (_t, s) = import(
        &format!("{SPECIFIC}B,B,2,300,,,,\nB,B,2,60,,,R1,R3\n"),
        &generalize(),
    );
    assert_eq!(trips(&plan(&s)), [["SLOW"]]);
}

#[test]
fn generalized_specific_walk_time_only_tightens_existing_footpaths() {
    // Stop-level B→D 120 s raised to 600 s: T2 (08:15) no longer reachable.
    let (_t, s) = import(
        &format!("{SPECIFIC}B,D,2,120,,,,\nB,D,2,600,,,R1,R2\nC,C,2,600,,,,\n"),
        &generalize(),
    );
    let r = plan(&s);
    assert!(
        !trips(&r).contains(&vec!["T1".into(), "T2".into()]),
        "{:?}",
        trips(&r)
    );
    // Without a stop-level footpath the specific row creates none.
    let (_t, s) = import(
        &format!("{SPECIFIC}B,D,2,60,,,R1,R2\nB,B,2,600,,,,\n"),
        &generalize(),
    );
    assert_eq!(trips(&plan(&s)), [["SLOW"]]);
}

#[test]
fn generalized_specific_forbidden_transfer_removes_the_footpath() {
    let (_t, s) = import(
        &format!("{SPECIFIC}B,D,2,120,,,,\nB,D,3,,T1,T2,,\nB,B,2,600,,,,\n"),
        &generalize(),
    );
    assert_eq!(trips(&plan(&s)), [["SLOW"]]);
    // Same-stop specific prohibition still cannot be enforced.
    let e = import_err(&format!("{SPECIFIC}B,B,3,,,,R1,R3\n"), &generalize());
    assert!(e.contains("cannot be enforced"), "{e}");
}
