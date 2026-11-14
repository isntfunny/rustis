//! Transfer-rule correctness: minimum change times, forbidden transfers,
//! trip/route-specific rules and rules that are deliberately not exploited.
//!
//! Base network (tests/fixtures/tiny) plus a fast feeder at B:
//!   T1  A 08:00 → B 08:10
//!   T3  B 08:12 → C 08:20   (needs a ≤ 120 s change at B)
//!   T2  D 08:15 → C 08:25   (B→D footpath when transfers.txt provides one)
//!   SLOW A 08:05 → C 08:40
use rustis::{
    BuildOptions, Endpoint, LegKind, PlanRequest, PlanResponse, Snapshot, import_gtfs_with,
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

fn opts() -> BuildOptions {
    BuildOptions::for_date("2026-11-16")
}

fn try_import(transfers: &str, opts: &BuildOptions) -> (TempDir, anyhow::Result<Snapshot>) {
    let tmp = TempDir::new().unwrap();
    let src = feed(tmp.path(), transfers);
    let dest = tmp.path().join("snapshot");
    let r = import_gtfs_with(&src, &dest, opts).and_then(|_| Snapshot::open(&dest));
    if r.is_err() {
        assert!(!dest.exists(), "failed import must not publish a snapshot");
    }
    (tmp, r)
}

fn import(transfers: &str, opts: &BuildOptions) -> (TempDir, Snapshot) {
    let (tmp, r) = try_import(transfers, opts);
    (tmp, r.unwrap())
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
            max_travel_seconds: 86_400,
            alternatives_within_seconds: 3600,
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
fn walked_transfer_keeps_its_min_time_and_same_stop_time_does_not_delay_it() {
    // B→D needs 120 s; T2 at D 08:15 is reachable (08:10 + 120 ≤ 08:15).
    let (_t, s) = import(&format!("{HEADER}B,D,2,120\n"), &opts());
    let r = plan(&s);
    assert_eq!(trips(&r)[0], ["T1", "T3"]);
    assert_changes_valid(&r, |_| 0, |a, b| (a == "B" && b == "D").then_some(120));
    // A 300 s same-stop rule at B forbids the change to T3, but walking off to
    // D is governed by the B→D rule alone: T1 → T2 stays possible (exact).
    let (_t, s) = import(&format!("{HEADER}B,B,2,300\nB,D,2,120\n"), &opts());
    let r = plan(&s);
    assert_changes_valid(
        &r,
        |s| if s == "B" { 300 } else { 0 },
        |a, b| (a == "B" && b == "D").then_some(120),
    );
    assert!(!trips(&r).contains(&vec!["T1".into(), "T3".into()]));
    assert_eq!(trips(&r)[0], ["T1", "T2"]);
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
fn forbidden_transfers_are_enforced() {
    // Distinct stops: no footpath, also not from generated links.
    let mut o = opts();
    o.link_radius_m = 500.0;
    let (_t, s) = import(&format!("{HEADER}B,D,3,\n"), &o);
    let r = plan(&s);
    assert!(
        !trips(&r).contains(&vec!["T1".into(), "T2".into()]),
        "{:?}",
        trips(&r)
    );
    // Same stop: no change at B at all (T1 → T3 impossible).
    let (_t, s) = import(&format!("{HEADER}B,B,3,\n"), &opts());
    let r = plan(&s);
    assert!(
        !trips(&r).contains(&vec!["T1".into(), "T3".into()]),
        "{:?}",
        trips(&r)
    );
}

#[test]
fn min_time_rule_without_time_uses_the_walking_time() {
    // B→D ≈ 110 m straight line at 1.2 m/s ≈ 92 s: T1 (B 08:10) → T2 (D 08:15).
    // (Changing at B itself is forbidden so T1 → T3 cannot win.)
    let (_t, s) = import(&format!("{HEADER}B,D,2,\nB,B,3,\n"), &opts());
    let r = plan(&s);
    assert!(
        trips(&r).contains(&vec!["T1".into(), "T2".into()]),
        "{:?}",
        trips(&r)
    );
}

const SPECIFIC: &str = "from_stop_id,to_stop_id,transfer_type,min_transfer_time,from_trip_id,to_trip_id,from_route_id,to_route_id\n";

#[test]
fn trip_and_route_specific_restrictions_are_enforced() {
    for row in [
        "B,B,2,600,T1,T3,,",
        "B,B,3,,T1,T3,,",
        "B,B,2,600,,,R1,R3",
        "B,B,3,,,,R1,R3",
        "B,B,3,,T1,,,R3",
    ] {
        let (_t, s) = import(&format!("{SPECIFIC}{row}\n"), &opts());
        let r = plan(&s);
        assert_eq!(trips(&r), [["SLOW"]], "{row}");
    }
    // A rule for other trips does not affect T1 → T3.
    let (_t, s) = import(&format!("{SPECIFIC}B,B,3,,SLOW,T3,,\n"), &opts());
    assert_eq!(trips(&plan(&s))[0], ["T1", "T3"]);
}

#[test]
fn the_most_specific_rule_wins_also_when_it_is_shorter() {
    // Stop-level 300 s at B, but T1 → T3 explicitly needs only 60 s.
    let (_t, s) = import(
        &format!("{SPECIFIC}B,B,2,300,,,,\nB,B,2,60,T1,T3,,\n"),
        &opts(),
    );
    assert_eq!(trips(&plan(&s))[0], ["T1", "T3"]);
    // Trip pair beats route pair.
    let (_t, s) = import(
        &format!("{SPECIFIC}B,B,3,,,,R1,R3\nB,B,2,60,T1,T3,,\n"),
        &opts(),
    );
    assert_eq!(trips(&plan(&s))[0], ["T1", "T3"]);
}

#[test]
fn relaxing_rules_are_not_exploited_and_flagged() {
    let h = "from_stop_id,to_stop_id,transfer_type,min_transfer_time,from_trip_id,to_trip_id\n";
    for row in [
        "B,D,1,,T1,T2",
        "B,B,5,,T1,T3",
        "B,D,0,,T1,T2",
        "B,D,1,,,",
        "B,B,4,,,",
    ] {
        let (_t, s) = import(&format!("{h}{row}\nB,B,2,300,,\n"), &opts());
        let r = plan(&s);
        // Never turned into a footpath or a shorter change.
        assert_eq!(trips(&r), [["SLOW"]], "{row}");
        assert!(
            r.warnings.iter().any(|w| w.contains("not exploited")),
            "{row}: {:?}",
            r.warnings
        );
    }
}

#[test]
fn fully_modelled_snapshots_carry_no_warning() {
    let (_t, s) = import(&format!("{HEADER}B,B,2,60\nB,D,2,120\n"), &opts());
    assert!(plan(&s).warnings.is_empty());
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
            max_travel_seconds: 86_400,
            alternatives_within_seconds: 3600,
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
            max_travel_seconds: 86_400,
            alternatives_within_seconds: 3600,
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

/// Feeder T3 (B 08:12 → C 08:20) continues T1 (A → B 08:10) when both
/// share a block (stay seated): no change time, even though B requires 600 s.
fn block_feed(t1_block: &str, t3_block: &str, t3_service: &str) -> (TempDir, Snapshot) {
    let tmp = TempDir::new().unwrap();
    let src = feed(tmp.path(), &format!("{HEADER}B,B,2,600\n"));
    let trips = format!(
        "route_id,service_id,trip_id,trip_headsign,block_id\nR1,daily,T1,Interchange,{t1_block}\nR2,daily,T2,Destination,\nRS,daily,SLOW,Destination,\nRN,daily,NIGHT,Destination,\nRP,daily,NOPICK,Destination,\nRP,daily,NODROP,Destination,\nR3,{t3_service},T3,Destination,{t3_block}\n"
    );
    std::fs::write(src.join("trips.txt"), trips).unwrap();
    let cal = std::fs::read_to_string(fixture().join("calendar.txt")).unwrap()
        + "other,1,1,1,1,1,1,1,20261101,20261231\n";
    std::fs::write(src.join("calendar.txt"), cal).unwrap();
    let dest = tmp.path().join("snapshot");
    import_gtfs_with(&src, &dest, &opts()).unwrap();
    (tmp, Snapshot::open(&dest).unwrap())
}

#[test]
fn stay_seated_continuations_skip_the_change_time() {
    let (_t, s) = block_feed("blk", "blk", "daily");
    assert_eq!(trips(&plan(&s))[0], ["T1", "T3"]);
    // Different block, or same block on another service: a normal change (600 s) applies.
    let (_t, s) = block_feed("blk", "other", "daily");
    assert_eq!(trips(&plan(&s)), [["SLOW"]]);
    let (_t, s) = block_feed("blk", "blk", "other");
    assert_eq!(trips(&plan(&s)), [["SLOW"]]);
    // transfers.txt type 4 between the two trips has the same effect.
    let h = "from_stop_id,to_stop_id,transfer_type,min_transfer_time,from_trip_id,to_trip_id\n";
    let (_t, s) = import(&format!("{h}B,B,2,600,,\nB,B,4,,T1,T3\n"), &opts());
    assert_eq!(trips(&plan(&s))[0], ["T1", "T3"]);
}
