//! Randomised validity/optimality check against an independent reference.
//!
//! Small random networks (seeded, deterministic) are written as GTFS,
//! imported and queried. Every returned itinerary is replayed against the
//! generated timetable by an independent checker (trip times, pickup/drop-off,
//! same-stop change times, footpath times, chronology). An exact-rule
//! brute-force search provides the true earliest arrival:
//! * no itinerary may beat it (that would mean a rule was violated);
//! * without change times, Rustis must match it exactly (completeness);
//! * with change times, the conservative gap is counted and reported.
use rustis::{Endpoint, ImportOptions, LegKind, PlanRequest, Snapshot, import_gtfs_with};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};
use std::fmt::Write as _;
use tempfile::TempDir;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

struct Trip {
    id: String,
    stops: Vec<usize>,
    arr: Vec<u32>,
    dep: Vec<u32>,
    pickup: Vec<bool>,
    drop_off: Vec<bool>,
}

struct Net {
    n_stops: usize,
    trips: Vec<Trip>,
    change: Vec<u32>,
    foot: BTreeMap<(usize, usize), u32>,
}

fn hms(t: u32) -> String {
    format!("{:02}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60)
}

fn generate(seed: u64, with_change_times: bool) -> Net {
    let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let n_stops = 6;
    let mut trips = Vec::new();
    for route in 0..4 {
        let len = 2 + r.below(3) as usize;
        let mut stops: Vec<usize> = Vec::new();
        while stops.len() < len {
            let s = r.below(n_stops as u64) as usize;
            if !stops.contains(&s) {
                stops.push(s);
            }
        }
        let hops: Vec<u32> = (1..len).map(|_| 120 + 60 * r.below(5) as u32).collect();
        for k in 0..(2 + r.below(4)) {
            let mut t = 8 * 3600 + 60 * r.below(45) as u32;
            let (mut arr, mut dep) = (Vec::new(), Vec::new());
            for i in 0..len {
                if i > 0 {
                    t += hops[i - 1];
                }
                arr.push(t);
                t += 60 * r.below(2) as u32;
                dep.push(t);
            }
            let pickup = (0..len).map(|_| r.below(8) != 0).collect();
            let drop_off = (0..len).map(|_| r.below(8) != 0).collect();
            trips.push(Trip {
                id: format!("R{route}T{k}"),
                stops: stops.clone(),
                arr,
                dep,
                pickup,
                drop_off,
            });
        }
    }
    let change = (0..n_stops)
        .map(|_| {
            if with_change_times {
                [0, 60, 180, 300][r.below(4) as usize]
            } else {
                0
            }
        })
        .collect();
    let mut foot = BTreeMap::new();
    for _ in 0..(2 + r.below(5)) {
        let (a, b) = (
            r.below(n_stops as u64) as usize,
            r.below(n_stops as u64) as usize,
        );
        if a != b {
            foot.insert((a, b), 60 + 60 * r.below(5) as u32);
        }
    }
    Net {
        n_stops,
        trips,
        change,
        foot,
    }
}

fn write_gtfs(net: &Net, dir: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    let w = |n: &str, s: String| std::fs::write(dir.join(n), s).unwrap();
    w(
        "agency.txt",
        "agency_id,agency_name,agency_url,agency_timezone\na,A,https://a.invalid,Europe/Berlin\n"
            .into(),
    );
    w(
        "calendar.txt",
        "service_id,monday,tuesday,wednesday,thursday,friday,saturday,sunday,start_date,end_date\nd,1,1,1,1,1,1,1,20261101,20261231\n".into(),
    );
    let mut stops = String::from("stop_id,stop_name,stop_lat,stop_lon\n");
    for s in 0..net.n_stops {
        writeln!(stops, "S{s},Stop {s},50.{s:03},8.{s:03}").unwrap();
    }
    w("stops.txt", stops);
    let mut routes = String::from("route_id,agency_id,route_short_name,route_type\n");
    let mut trips = String::from("route_id,service_id,trip_id\n");
    let mut st = String::from(
        "trip_id,arrival_time,departure_time,stop_id,stop_sequence,pickup_type,drop_off_type\n",
    );
    for t in &net.trips {
        let route = &t.id[..2];
        if !routes.contains(&format!("\n{route},")) {
            writeln!(routes, "{route},a,{route},3").unwrap();
        }
        writeln!(trips, "{route},d,{}", t.id).unwrap();
        for i in 0..t.stops.len() {
            writeln!(
                st,
                "{},{},{},S{},{},{},{}",
                t.id,
                hms(t.arr[i]),
                hms(t.dep[i]),
                t.stops[i],
                i + 1,
                u8::from(!t.pickup[i]),
                u8::from(!t.drop_off[i])
            )
            .unwrap();
        }
    }
    w("routes.txt", routes);
    w("trips.txt", trips);
    w("stop_times.txt", st);
    let mut tr = String::from("from_stop_id,to_stop_id,transfer_type,min_transfer_time\n");
    for (s, &c) in net.change.iter().enumerate() {
        if c > 0 {
            writeln!(tr, "S{s},S{s},2,{c}").unwrap();
        }
    }
    for (&(a, b), &m) in &net.foot {
        writeln!(tr, "S{a},S{b},2,{m}").unwrap();
    }
    w("transfers.txt", tr);
}

/// Exact-rule earliest arrival with at least one vehicle (RAPTOR returns no
/// walk-only journeys). Modes: 0 on foot, never ridden; 1 just alighted;
/// 2 on foot after riding.
/// Returns `None` if the target is reachable on foot alone (not compared).
fn brute_force(net: &Net, from: usize, to: usize, depart: u32) -> Option<Option<u32>> {
    let mut best = vec![[u32::MAX; 3]; net.n_stops];
    let mut heap = BinaryHeap::from([Reverse((depart, from, 0usize))]);
    best[from][0] = depart;
    let relax = |heap: &mut BinaryHeap<Reverse<(u32, usize, usize)>>,
                 best: &mut Vec<[u32; 3]>,
                 s: usize,
                 m: usize,
                 t: u32| {
        if t < best[s][m] {
            best[s][m] = t;
            heap.push(Reverse((t, s, m)));
        }
    };
    while let Some(Reverse((t, s, m))) = heap.pop() {
        if t > best[s][m] {
            continue;
        }
        for (&(a, b), &w) in &net.foot {
            if a == s {
                relax(&mut heap, &mut best, b, if m == 0 { 0 } else { 2 }, t + w);
            }
        }
        let ready = t + if m == 1 { net.change[s] } else { 0 };
        for day in [0, 86_400] {
            for trip in &net.trips {
                let Some(i) = trip.stops.iter().position(|&x| x == s) else {
                    continue;
                };
                if trip.dep[i] + day < ready || !trip.pickup[i] {
                    continue;
                }
                for j in i + 1..trip.stops.len() {
                    if trip.drop_off[j] {
                        relax(&mut heap, &mut best, trip.stops[j], 1, trip.arr[j] + day);
                    }
                }
            }
        }
    }
    if best[to][0] != u32::MAX {
        return None;
    }
    let a = best[to][1].min(best[to][2]);
    Some((a != u32::MAX).then_some(a))
}

fn stop_no(id: &str) -> usize {
    id[1..].parse().unwrap()
}

/// Replays an itinerary against the generated timetable.
fn check(net: &Net, it: &rustis::Itinerary, depart: u32, ctx: &str) {
    let mut t = depart;
    let mut alighted: Option<(usize, u32)> = None;
    for l in &it.legs {
        assert!(l.departure >= t, "{ctx}: leg starts before previous ends");
        let from = stop_no(&l.from.as_ref().unwrap().stop_id);
        let to = stop_no(&l.to.as_ref().unwrap().stop_id);
        match l.kind {
            LegKind::Walk => {
                assert_eq!(
                    Some(&l.duration_seconds),
                    net.foot.get(&(from, to)),
                    "{ctx}: walk {from}->{to}"
                );
                alighted = None;
            }
            LegKind::Transit => {
                let trip = net
                    .trips
                    .iter()
                    .find(|x| Some(x.id.as_str()) == l.trip_id.as_deref())
                    .unwrap();
                let i = trip.stops.iter().position(|&x| x == from).unwrap();
                let j = trip.stops.iter().position(|&x| x == to).unwrap();
                assert!(
                    i < j && trip.pickup[i] && trip.drop_off[j],
                    "{ctx}: boarding/alighting not allowed"
                );
                let day = l.departure - trip.dep[i];
                assert!(
                    day == 0 || day == 86_400,
                    "{ctx}: departure not on a service day"
                );
                assert_eq!(
                    (l.departure, l.arrival),
                    (trip.dep[i] + day, trip.arr[j] + day),
                    "{ctx}: times"
                );
                if let Some((s, a)) = alighted {
                    assert_eq!(s, from, "{ctx}: teleport");
                    assert!(
                        l.departure >= a + net.change[s],
                        "{ctx}: change at S{s} too short"
                    );
                }
                alighted = Some((to, l.arrival));
            }
            other => panic!("{ctx}: unexpected leg {other:?}"),
        }
        t = l.arrival;
    }
    assert_eq!(it.arrival, t, "{ctx}: arrival");
}

fn run(with_change_times: bool, seeds: std::ops::Range<u64>) -> (usize, usize, usize) {
    let (mut queries, mut found, mut gaps, mut walk_only) = (0, 0, 0, 0);
    for seed in seeds {
        let net = generate(seed, with_change_times);
        let tmp = TempDir::new().unwrap();
        write_gtfs(&net, &tmp.path().join("feed"));
        let dest = tmp.path().join("snap");
        import_gtfs_with(
            &tmp.path().join("feed"),
            &dest,
            &ImportOptions::for_date("2026-11-16"),
        )
        .unwrap();
        let snap = Snapshot::open(&dest).unwrap();
        for from in 0..net.n_stops {
            for to in 0..net.n_stops {
                if from == to {
                    continue;
                }
                for depart in [8 * 3600, 8 * 3600 + 1200] {
                    queries += 1;
                    let ctx = format!("seed {seed} S{from}->S{to} @{depart}");
                    let r = snap.plan(&PlanRequest {
                        date: "2026-11-16".into(),
                        departure: depart,
                        from: vec![Endpoint {
                            stop_id: format!("S{from}"),
                            walk_seconds: 0,
                        }],
                        to: vec![Endpoint {
                            stop_id: format!("S{to}"),
                            walk_seconds: 0,
                        }],
                        max_transfers: 8,
                        max_results: 10,
                    });
                    // The prefilter drops stops no trip serves on the snapshot days.
                    let r = match r {
                        Ok(r) => r,
                        Err(e) if e.to_string().contains("not served") => {
                            assert!(
                                brute_force(&net, from, to, depart).is_none_or(|t| t.is_none()),
                                "{ctx}: {e}"
                            );
                            continue;
                        }
                        Err(e) => panic!("{ctx}: {e}"),
                    };
                    assert_eq!(r.omitted_unreconstructable, 0, "{ctx}");
                    for it in &r.itineraries {
                        check(&net, it, depart, &ctx);
                    }
                    let best = r.itineraries.iter().map(|i| i.arrival).min();
                    let Some(truth) = brute_force(&net, from, to, depart) else {
                        walk_only += 1;
                        continue;
                    };
                    if let (Some(b), Some(t)) = (best, truth) {
                        assert!(
                            b >= t,
                            "{ctx}: beats exact reference ({b} < {t}) – rule violated"
                        );
                    }
                    if best.is_some() {
                        assert!(
                            truth.is_some(),
                            "{ctx}: result where the reference finds none"
                        );
                        found += 1;
                    }
                    if best != truth {
                        assert!(
                            with_change_times,
                            "{ctx}: without change times Rustis must be exact: {best:?} vs {truth:?}\n{}\nfoot {:?}",
                            net.trips
                                .iter()
                                .map(|t| format!(
                                    "{} {:?} dep {:?} arr {:?} pu {:?} do {:?}",
                                    t.id, t.stops, t.dep, t.arr, t.pickup, t.drop_off
                                ))
                                .collect::<Vec<_>>()
                                .join("\n"),
                            net.foot
                        );
                        gaps += 1;
                    }
                }
            }
        }
    }
    eprintln!("({walk_only} queries skipped: target reachable on foot)");
    (queries, found, gaps)
}

#[test]
fn random_networks_without_change_times_match_exact_reference() {
    let (q, found, gaps) = run(false, 0..300);
    eprintln!("no change times: {q} queries, {found} with results, {gaps} gaps");
    assert!(found > q / 4, "generator too sparse: {found}/{q}");
    assert_eq!(gaps, 0);
}

#[test]
fn random_networks_with_change_times_are_valid_and_gap_is_reported() {
    let (q, found, gaps) = run(true, 1000..1300);
    eprintln!("with change times: {q} queries, {found} with results, {gaps} conservative gaps");
    assert!(found > q / 4, "generator too sparse: {found}/{q}");
}
