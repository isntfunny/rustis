//! Serving: request validation, the RAPTOR searches and response building.
use crate::Error;
use crate::build::{
    self, BuildOptions, FLAG_DROP_OFF_BOOKING, FLAG_PICKUP_BOOKING, FeedSpec, SnapshotMeta,
};
use crate::model::*;
use crate::raptor::{self, Journey, Query, Step};
use crate::tt::Timetable;
use anyhow::{Context, Result};
use std::path::Path;

/// Search windows tried one after another (seconds after the departure);
/// a later window is only searched if the earlier one found nothing.
const WINDOWS: [u32; 5] = [2 * 3600, 4 * 3600, 8 * 3600, 16 * 3600, 36 * 3600];
const MAX_ENDPOINT_WALK: u32 = 3 * 3600;
const MAX_TRAVEL: u32 = 4 * 86_400;

pub struct Snapshot {
    tt: Timetable,
    /// Per-search label budget (see `raptor::Query::max_labels`).
    max_labels: usize,
}

/// Default per-search label budget; bounds the memory and time of searches
/// that would otherwise explore most of the network (e.g. targets without
/// service on the requested day).
pub const DEFAULT_MAX_LABELS: usize = 150_000;

/// Builds a snapshot from several feeds into `dest` (staging dir + rename).
pub fn build_snapshot(feeds: &[FeedSpec], dest: &Path, opts: BuildOptions) -> Result<SnapshotMeta> {
    build_snapshot_jobs(feeds, dest, opts, 1, &|_| Ok(()))
}

/// Like [`build_snapshot`], parsing up to `jobs` feeds in parallel. Feeds are
/// committed in input order (the snapshot is identical to a serial build).
/// `ready(i)` blocks until feed `i` can be read (e.g. its download finished);
/// an `Err` marks the feed as failed with that message.
pub fn build_snapshot_jobs(
    feeds: &[FeedSpec],
    dest: &Path,
    opts: BuildOptions,
    jobs: usize,
    ready: &(dyn Fn(usize) -> Result<(), String> + Sync),
) -> Result<SnapshotMeta> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex, mpsc};
    let t0 = std::time::Instant::now();
    anyhow::ensure!(!dest.exists(), "{} already exists", dest.display());
    let staging = dest.with_extension("staging");
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(&staging)?;
    let mut b = build::Builder::new(&staging, opts.clone())?;
    let jobs = jobs.max(1);
    // Parsed feeds wait in order for their commit; at most `window` feeds are
    // in flight or waiting (bounds memory when a large feed is slow).
    let window = 2 * jobs;
    let next = AtomicUsize::new(0);
    let gate = (Mutex::new(0usize), Condvar::new());
    std::thread::scope(|sc| {
        let (tx, rx) = mpsc::channel();
        for _ in 0..jobs {
            let tx = tx.clone();
            let (next, gate, opts) = (&next, &gate, &opts);
            sc.spawn(move || {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    if i >= feeds.len() {
                        break;
                    }
                    {
                        let mut done = gate.0.lock().unwrap();
                        while i >= *done + window {
                            done = gate.1.wait(done).unwrap();
                        }
                    }
                    let r = ready(i);
                    let t = std::time::Instant::now();
                    let parsed = match r {
                        Ok(()) => build::parse_feed(&feeds[i], opts),
                        Err(e) => Err(anyhow::anyhow!(e)),
                    };
                    if tx.send((i, parsed, t)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        let mut pending = std::collections::BTreeMap::new();
        let mut k = 0usize;
        for (i, parsed, t) in rx {
            pending.insert(i, (parsed, t));
            while let Some((parsed, t)) = pending.remove(&k) {
                b.add_parsed(&feeds[k], parsed, t);
                k += 1;
                *gate.0.lock().unwrap() = k;
                gate.1.notify_all();
            }
        }
    });
    let meta = b.finish(t0)?;
    let json = serde_json::to_vec_pretty(&meta)?;
    std::fs::write(staging.join("snapshot.json"), json)?;
    for e in std::fs::read_dir(&staging)? {
        let p = e?.path();
        let mut perm = std::fs::metadata(&p)?.permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&p, perm)?;
        std::fs::File::open(&p)?.sync_all()?;
    }
    std::fs::rename(&staging, dest).context("publishing snapshot")?;
    Ok(meta)
}

struct Resolved {
    stops: Vec<(u32, u32)>,
}

impl Snapshot {
    pub fn open(dir: &Path) -> Result<Self> {
        Ok(Self {
            tt: Timetable::open(dir)?,
            max_labels: DEFAULT_MAX_LABELS,
        })
    }

    pub fn verify(&self) -> Result<()> {
        self.tt.verify()
    }

    pub fn max_labels(&self) -> usize {
        self.max_labels
    }

    pub fn set_max_labels(&mut self, n: usize) {
        self.max_labels = n.max(1);
    }

    pub fn meta(&self) -> &SnapshotMeta {
        &self.tt.meta
    }

    pub fn first_day(&self) -> &str {
        &self.tt.meta.first_day
    }

    fn endpoints(&self, side: &str, eps: &[Endpoint]) -> Result<Resolved, Error> {
        if eps.is_empty() || eps.len() > MAX_ENDPOINTS {
            return Err(Error::invalid(format!(
                "`{side}` needs 1..={MAX_ENDPOINTS} stops"
            )));
        }
        let mut stops: Vec<(u32, u32)> = Vec::new();
        for e in eps {
            let s = self.tt.catalog.lookup(&e.stop_id).ok_or_else(|| {
                Error::invalid(format!("unknown stop_id {:?} in `{side}`", e.stop_id))
            })? as u32;
            if e.walk_seconds > MAX_ENDPOINT_WALK {
                return Err(Error::invalid(format!(
                    "walk_seconds must be ≤ {MAX_ENDPOINT_WALK}"
                )));
            }
            match stops.iter_mut().find(|(b, _)| *b == s) {
                Some(b) => b.1 = b.1.min(e.walk_seconds),
                None => stops.push((s, e.walk_seconds)),
            }
        }
        Ok(Resolved { stops })
    }

    /// Unix seconds of local midnight of `date` in time zone `tz`.
    fn midnight(&self, date: &str, tz: usize) -> Result<i64, Error> {
        let d = jiff::civil::Date::strptime("%Y-%m-%d", date)
            .map_err(|_| Error::invalid("date must be YYYY-MM-DD"))?;
        let z = d
            .at(0, 0, 0, 0)
            .to_zoned(self.tt.tzs[tz].clone())
            .map_err(|e| Error::invalid(format!("date: {e}")))?;
        Ok(z.timestamp().as_second())
    }

    pub fn plan(&self, q: &PlanRequest) -> Result<PlanResponse, Error> {
        if q.max_transfers > MAX_TRANSFERS {
            return Err(Error::invalid(format!(
                "max_transfers must be ≤ {MAX_TRANSFERS}"
            )));
        }
        if q.max_results == 0 || q.max_results > MAX_RESULTS {
            return Err(Error::invalid(format!(
                "max_results must be in 1..={MAX_RESULTS}"
            )));
        }
        if q.max_travel_seconds == 0 || q.max_travel_seconds > MAX_TRAVEL {
            return Err(Error::invalid(format!(
                "max_travel_seconds must be in 1..={MAX_TRAVEL}"
            )));
        }
        if q.departure >= 7 * 86_400 {
            return Err(Error::invalid(
                "departure must be < 7 days after the date's midnight",
            ));
        }
        let from = self.endpoints("from", &q.from)?;
        let to = self.endpoints("to", &q.to)?;
        let tz = self.tt.stop_tz[from.stops[0].0 as usize] as usize;
        let midnight = self.midnight(&q.date, tz)?;
        let depart_unix = midnight + i64::from(q.departure);
        let rel = depart_unix - self.tt.origin;
        let first = self.tt.base[tz][1] as i64; // service day `first_day`, noon − 12 h
        let last = self.tt.base[tz][self.tt.days - 1] as i64 + 86_400;
        if rel < first - 12 * 3600 || rel >= last {
            return Err(Error::invalid(format!(
                "date {} is outside the snapshot horizon ({} + {} days)",
                q.date, self.tt.meta.first_day, self.tt.meta.horizon_days
            )));
        }
        let to_local = |t: u32| -> u32 { (i64::from(t) + self.tt.origin - midnight).max(0) as u32 };

        // Exact, cheap pre-check: if no origin stop (or stop within a footpath)
        // has any service in the search period, or likewise no target stop,
        // there is no journey – answer without searching.
        if let Some(side) =
            self.no_service(&from.stops, &to.stops, rel as u32, q.max_travel_seconds, tz)
        {
            let mut warnings = self.warnings();
            warnings.push(format!(
                "NO SERVICE: no trip serves any {side} stop (or a stop within walking distance) between {} and \
                 {} + max_travel_seconds; no journey exists in the imported data.",
                q.date, q.date
            ));
            return Ok(PlanResponse {
                service_date: q.date.clone(),
                timezone: self.tt.meta.timezones[tz].clone(),
                itineraries: Vec::new(),
                omitted_unreconstructable: 0,
                warnings,
            });
        }
        let t0 = std::time::Instant::now();
        let mut found: Vec<(Journey, u32)> = Vec::new();
        let mut depart = rel as u32;
        let mut searches = 0;
        let mut stats = Vec::new();
        let mut first_dep: Option<u32> = None;
        // Lower bounds depend only on the targets: shared by all searches.
        let mut lb = raptor::LowerBound::new(&self.tt, &to.stops);
        let mut ok_window: Option<u32> = None;
        let mut truncated = false;
        'outer: while found.len() < q.max_results && searches < 3 * q.max_results {
            // Iterative deepening: the first window with a result decides;
            // then one more search with slack collects Pareto alternatives
            // (fewer transfers, later arrival).
            let mut result = None;
            // Once a window produced results, alternatives reuse it instead of
            // growing from the smallest window again.
            let windows: Vec<u32> = match ok_window {
                Some(w) => vec![w],
                None => WINDOWS
                    .iter()
                    .copied()
                    .filter(|&w| w < q.max_travel_seconds)
                    .chain([q.max_travel_seconds])
                    .collect(),
            };
            for w in windows {
                let query = Query {
                    sources: &from.stops,
                    targets: &to.stops,
                    depart,
                    max_transfers: q.max_transfers as usize,
                    window: w,
                    max_labels: self.max_labels,
                };
                searches += 1;
                let (js, st) = raptor::search(&self.tt, &query, &mut lb);
                stats.push(st);
                truncated |= st.truncated;
                if let Some(best) = js.iter().map(|j| j.arrival).min() {
                    let dur = best - depart;
                    let slack = (dur / 2).max(1800);
                    let w2 = (dur + slack).min(q.max_travel_seconds).max(w);
                    let (js2, st2) = if w2 > w {
                        searches += 1;
                        raptor::search(
                            &self.tt,
                            &Query {
                                window: w2,
                                ..query
                            },
                            &mut lb,
                        )
                    } else {
                        (Vec::new(), Default::default())
                    };
                    stats.push(st2);
                    truncated |= st2.truncated;
                    result = Some(if js2.is_empty() { js } else { js2 });
                    ok_window = Some(w2.max(w));
                    break;
                }
                if st.truncated {
                    break; // a larger window would exceed the budget too
                }
            }
            let Some(js) = result else { break 'outer };
            let mut next = u32::MAX;
            let mut added = false;
            for j in js {
                let dep = self.journey_departure(&j, depart);
                if first_dep.is_some_and(|f| dep > f.saturating_add(q.alternatives_within_seconds))
                {
                    continue;
                }
                next = next.min(dep);
                let sig = signature(&j);
                if !found.iter().any(|(f, _)| signature(f) == sig) {
                    found.push((j, dep));
                    added = true;
                }
            }
            if next == u32::MAX || (!added && next < depart) {
                break;
            }
            first_dep.get_or_insert(next);
            depart = next.max(depart) + 1;
            if first_dep.is_some_and(|f| depart > f.saturating_add(q.alternatives_within_seconds)) {
                break;
            }
        }
        let mut itineraries: Vec<Itinerary> = found
            .iter()
            .filter_map(|(j, _)| self.itinerary(j, &to_local))
            .collect();
        let omitted = found.len() - itineraries.len();
        itineraries.sort_by_key(|i| (i.departure, i.arrival, i.transfers));
        // Drop journeys dominated by one leaving no earlier and arriving no later
        // with no more transfers.
        let mut kept: Vec<Itinerary> = Vec::new();
        for it in itineraries {
            if !kept.iter().any(|k| {
                k.departure >= it.departure
                    && k.arrival <= it.arrival
                    && k.transfers <= it.transfers
                    && (k.departure, k.arrival, k.transfers)
                        != (it.departure, it.arrival, it.transfers)
            }) {
                kept.retain(|k| {
                    !(it.departure >= k.departure
                        && it.arrival <= k.arrival
                        && it.transfers <= k.transfers)
                });
                kept.push(it);
            }
        }
        kept.sort_by_key(|i| (i.arrival, i.transfers));
        kept.truncate(q.max_results);
        if std::env::var_os("RUSTIS_TIMING").is_some() {
            eprintln!("timing: {:?} searches={searches} {:?}", t0.elapsed(), stats);
        }
        let mut warnings = self.warnings();
        if truncated {
            warnings.push(format!(
                "SEARCH LIMIT: the search stopped after {} labels; journeys (or later/fewer-transfer \
                 alternatives) may exist that were not explored.",
                self.max_labels
            ));
        }
        if kept.iter().any(|i| i.booking_required) {
            warnings.push(
                "BOOKING: itineraries marked booking_required use stops where boarding or alighting must be \
                 arranged (GTFS pickup/drop_off 2/3); service is not guaranteed without it."
                    .into(),
            );
        }
        Ok(PlanResponse {
            service_date: q.date.clone(),
            timezone: self.tt.meta.timezones[tz].clone(),
            itineraries: kept,
            omitted_unreconstructable: omitted,
            warnings,
        })
    }

    /// `Some("origin"|"destination")` if that side has no service at all in
    /// the period the search could use (conservative: the period is widened
    /// by the longest trip span and footpath neighbours count as served).
    fn no_service(
        &self,
        from: &[(u32, u32)],
        to: &[(u32, u32)],
        depart: u32,
        max_travel: u32,
        tz: usize,
    ) -> Option<&'static str> {
        let tt = &self.tt;
        let base = &tt.base[tz];
        let d_t = base.partition_point(|&b| b <= depart).saturating_sub(1);
        let span = (tt.meta.max_time / 86_400) as usize + 1;
        let lo = d_t.saturating_sub(span + 1);
        let hi = d_t + (max_travel as usize).div_ceil(86_400) + 1;
        let served = |s: u32| tt.served_between(s as usize, lo, hi);
        let near_out = |s: u32| served(s) || tt.footpaths(s).any(|(u, _)| served(u));
        let near_in = |s: u32| {
            served(s) || {
                let (a, b) = (
                    tt.lb_in_off[s as usize] as usize,
                    tt.lb_in_off[s as usize + 1] as usize,
                );
                tt.lb_in_from[a..b].iter().any(|&u| served(u))
            }
        };
        if !from.iter().any(|&(s, _)| near_out(s)) {
            return Some("origin");
        }
        if !to.iter().any(|&(s, _)| near_in(s)) {
            return Some("destination");
        }
        None
    }

    /// When the rider leaves the origin (latest possible) – relative seconds.
    fn journey_departure(&self, j: &Journey, depart: u32) -> u32 {
        let mut before = 0u32;
        for s in &j.steps {
            match s {
                Step::Access { secs, .. } | Step::Walk { secs, .. } => before += secs,
                Step::Ride {
                    route,
                    trip,
                    day,
                    board_pos,
                    ..
                } => {
                    let (t, d) = self.ride_times(*route, *trip, *day, *board_pos, *board_pos);
                    let _ = d;
                    return t.saturating_sub(before);
                }
                Step::Egress { .. } => {}
            }
        }
        depart
    }

    /// (departure at `bpos`, arrival at `apos`) of a trip, relative seconds.
    fn ride_times(&self, route: u32, trip: u32, day: u32, bpos: u32, apos: u32) -> (u32, u32) {
        let tt = &self.tt;
        let r = route as usize;
        let j = (trip - tt.route_trip_off[r]) as usize;
        let base = tt.base[tt.route_tz[r] as usize][day as usize];
        (
            base + tt.deps(r, bpos as usize)[j],
            base + tt.arrs(r, apos as usize)[j],
        )
    }

    fn stop(&self, s: u32) -> Stop {
        self.tt.catalog.stop(s as usize, self.tt.served(s as usize))
    }

    fn iso(&self, rel: u32, stop: Option<u32>) -> String {
        let tz = stop.map_or(0, |s| self.tt.stop_tz[s as usize] as usize);
        jiff::Timestamp::from_second(i64::from(rel) + self.tt.origin)
            .map(|t| {
                t.to_zoned(self.tt.tzs[tz].clone())
                    .strftime("%Y-%m-%dT%H:%M:%S%:z")
                    .to_string()
            })
            .unwrap_or_default()
    }

    #[allow(clippy::too_many_arguments)]
    fn leg(
        &self,
        kind: LegKind,
        from: Option<u32>,
        to: Option<u32>,
        dep: u32,
        arr: u32,
        to_local: &dyn Fn(u32) -> u32,
        geometry: Vec<[f64; 2]>,
    ) -> Leg {
        Leg {
            kind,
            from: from.map(|s| self.stop(s)),
            to: to.map(|s| self.stop(s)),
            departure: to_local(dep),
            arrival: to_local(arr),
            departure_time: self.iso(dep, from.or(to)),
            arrival_time: self.iso(arr, to.or(from)),
            duration_seconds: arr - dep,
            trip_id: None,
            route_id: None,
            line: None,
            mode: None,
            route_type: None,
            headsign: None,
            agency: None,
            color: None,
            intermediate_stops: Vec::new(),
            pickup_booking_required: false,
            drop_off_booking_required: false,
            geometry,
            geometry_source: "stops",
        }
    }

    fn itinerary(&self, j: &Journey, to_local: &dyn Fn(u32) -> u32) -> Option<Itinerary> {
        let tt = &self.tt;
        let coord = |s: u32| tt.catalog.coord(s as usize);
        // Absolute times: rides fix the clock; walks after a ride start at its
        // arrival; walks before the first ride end at its departure.
        let first_ride = j.steps.iter().position(|s| matches!(s, Step::Ride { .. }));
        let mut legs: Vec<Leg> = Vec::new();
        let mut clock: Option<u32> = None;
        // backward pass for the pre-ride part
        let pre_end = first_ride.map(|i| match j.steps[i] {
            Step::Ride {
                route,
                trip,
                day,
                board_pos,
                ..
            } => self.ride_times(route, trip, day, board_pos, board_pos).0,
            _ => unreachable!(),
        });
        let pre: u32 = j.steps[..first_ride.unwrap_or(j.steps.len())]
            .iter()
            .map(|s| match s {
                Step::Access { secs, .. } | Step::Walk { secs, .. } => *secs,
                _ => 0,
            })
            .sum();
        let start = match pre_end {
            Some(e) => e.checked_sub(pre)?,
            None => j.arrival.checked_sub(
                j.steps
                    .iter()
                    .map(|s| match s {
                        Step::Access { secs, .. }
                        | Step::Walk { secs, .. }
                        | Step::Egress { secs, .. } => *secs,
                        _ => 0,
                    })
                    .sum(),
            )?,
        };
        let mut t = start;
        let mut transfers = 0usize;
        let mut rides = 0usize;
        for s in &j.steps {
            match *s {
                Step::Access { stop, secs } => {
                    if secs > 0 {
                        legs.push(self.leg(
                            LegKind::Access,
                            None,
                            Some(stop),
                            t,
                            t + secs,
                            to_local,
                            coord(stop).into_iter().collect(),
                        ));
                    }
                    t += secs;
                }
                Step::Walk { from, to, secs } => {
                    legs.push(self.leg(
                        LegKind::Walk,
                        Some(from),
                        Some(to),
                        t,
                        t + secs,
                        to_local,
                        [coord(from), coord(to)].into_iter().flatten().collect(),
                    ));
                    t += secs;
                }
                Step::Ride {
                    route,
                    trip,
                    day,
                    board_pos,
                    alight_pos,
                } => {
                    let (d, a) = self.ride_times(route, trip, day, board_pos, alight_pos);
                    if d < t || clock.is_some_and(|c| d < c) {
                        return None; // inconsistent (must never happen)
                    }
                    let r = route as usize;
                    let (b, e) = (
                        tt.route_stop(r, board_pos as usize),
                        tt.route_stop(r, alight_pos as usize),
                    );
                    let intermediate: Vec<Stop> = (board_pos + 1..alight_pos)
                        .map(|p| self.stop(tt.route_stop(r, p as usize)))
                        .collect();
                    let geometry = (board_pos..=alight_pos)
                        .filter_map(|p| coord(tt.route_stop(r, p as usize)))
                        .collect();
                    let mut leg =
                        self.leg(LegKind::Transit, Some(b), Some(e), d, a, to_local, geometry);
                    let info = tt.route_info[r] as usize;
                    leg.trip_id = Some(
                        tt.gtfs_trip_id
                            .get(tt.trip_gtfs[trip as usize] as usize)
                            .to_owned(),
                    );
                    leg.route_id = Some(tt.route_info_id.get(info).to_owned());
                    leg.line = Some(tt.route_info_line.get(info).to_owned());
                    leg.mode = Some(build::mode_name(tt.route_info_type[info]).to_owned());
                    leg.route_type = Some(tt.route_info_type[info]);
                    leg.intermediate_stops = intermediate;
                    let gtfs = tt.trip_gtfs[trip as usize] as usize;
                    let h = tt.gtfs_trip_headsign[gtfs] as usize;
                    leg.headsign = Some(if h != 0 {
                        tt.headsigns.get(h).to_owned()
                    } else {
                        let last = tt.route_stop_off[r + 1] - tt.route_stop_off[r] - 1;
                        self.stop(tt.route_stop(r, last as usize)).name
                    });
                    let nonempty = |v: &str| (!v.is_empty()).then(|| v.to_owned());
                    leg.agency = nonempty(tt.route_info_agency.get(info));
                    leg.color = nonempty(tt.route_info_color.get(info));
                    leg.pickup_booking_required =
                        tt.route_flag(r, board_pos as usize) & FLAG_PICKUP_BOOKING != 0;
                    leg.drop_off_booking_required =
                        tt.route_flag(r, alight_pos as usize) & FLAG_DROP_OFF_BOOKING != 0;
                    legs.push(leg);
                    if rides > 0 {
                        transfers += 1;
                    }
                    rides += 1;
                    t = a;
                    clock = Some(a);
                }
                Step::Egress { stop, secs } => {
                    if secs > 0 {
                        legs.push(self.leg(
                            LegKind::Egress,
                            Some(stop),
                            None,
                            t,
                            t + secs,
                            to_local,
                            coord(stop).into_iter().collect(),
                        ));
                    }
                    t += secs;
                }
            }
        }
        if t != j.arrival {
            return None;
        }
        let (departure, arrival) = (start, t);
        let first_stop = j.steps.iter().find_map(|s| match s {
            Step::Access { stop, .. } => Some(*stop),
            _ => None,
        });
        let last_stop = j.steps.iter().rev().find_map(|s| match s {
            Step::Egress { stop, .. } => Some(*stop),
            _ => None,
        });
        Some(Itinerary {
            departure: to_local(departure),
            arrival: to_local(arrival),
            departure_time: self.iso(departure, first_stop),
            arrival_time: self.iso(arrival, last_stop),
            duration_seconds: arrival - departure,
            transfers,
            booking_required: legs
                .iter()
                .any(|l| l.pickup_booking_required || l.drop_off_booking_required),
            legs,
        })
    }

    /// Stops of the full catalogue inside the box (served and unserved; the
    /// `served` flag tells whether any trip of the horizon stops there).
    pub fn stops_in_bbox(
        &self,
        min: [f64; 2],
        max: [f64; 2],
        limit: usize,
        served_only: bool,
    ) -> Result<Vec<Stop>, Error> {
        let ok = min.iter().chain(&max).all(|v| v.is_finite())
            && (-90.0..=90.0).contains(&min[0])
            && (-90.0..=90.0).contains(&max[0])
            && (-180.0..=180.0).contains(&min[1])
            && (-180.0..=180.0).contains(&max[1])
            && min[0] <= max[0]
            && min[1] <= max[1];
        if !ok {
            return Err(Error::invalid("bbox must be finite lat/lon with min ≤ max"));
        }
        if limit == 0 || limit > MAX_BBOX_RESULTS {
            return Err(Error::invalid(format!(
                "limit must be in 1..={MAX_BBOX_RESULTS}"
            )));
        }
        Ok(self
            .tt
            .catalog
            .lat_band(min[0], max[0])
            .filter(|&i| {
                self.tt
                    .catalog
                    .coord(i)
                    .is_some_and(|[_, lo]| lo >= min[1] && lo <= max[1])
            })
            .filter(|&i| !served_only || self.tt.served(i))
            .take(limit)
            .map(|i| self.tt.catalog.stop(i, self.tt.served(i)))
            .collect())
    }

    pub fn stats(&self) -> Stats {
        let m = &self.tt.meta;
        let disk: u64 = std::fs::read_dir(&self.tt.dir)
            .map(|d| {
                d.filter_map(|e| e.ok()?.metadata().ok())
                    .map(|m| m.len())
                    .sum()
            })
            .unwrap_or(0);
        Stats {
            snapshot_dir: self.tt.dir.display().to_string(),
            first_day: m.first_day.clone(),
            horizon_days: m.horizon_days,
            feeds_imported: m.feeds.iter().filter(|f| f.imported).count(),
            feeds_failed: m.feeds.iter().filter(|f| !f.imported).count(),
            stops: m.n_stops,
            routes: m.n_routes,
            trips: m.n_trips,
            gtfs_trips: m.n_gtfs_trips,
            bitfields: m.n_bitfields,
            footpaths: m.n_footpaths,
            specific_transfer_rules: m.n_specific_rules,
            disk_bytes: disk,
            mapped_bytes: self.tt.mapped_bytes(),
            process_memory: crate::memory::ProcessMemory::current(),
        }
    }

    /// Warnings attached to every response (known coverage/semantic limits).
    /// Diagnostics: what Rustis requires to change from stop `from` to stop
    /// `to` (same-stop change time, direct footpath, specific rules).
    pub fn transfer_info(&self, from: &str, to: &str) -> Result<serde_json::Value, Error> {
        let tt = &self.tt;
        let a = tt
            .catalog
            .lookup(from)
            .ok_or_else(|| Error::invalid(format!("unknown stop {from}")))?;
        let b = tt
            .catalog
            .lookup(to)
            .ok_or_else(|| Error::invalid(format!("unknown stop {to}")))?;
        let secs = |x: u32| if x == build::FORBIDDEN { None } else { Some(x) };
        let foot = tt.footpaths_flagged(a as u32).find(|f| f.0 as usize == b);
        let name =
            |i: u32, s: &crate::tt::Strs| (i != build::NONE).then(|| s.get(i as usize).to_owned());
        let rules: Vec<serde_json::Value> = (tt.rule_off[b] as usize..tt.rule_off[b + 1] as usize)
            .filter(|&i| tt.rule_from_stop[i] as usize == a)
            .map(|i| {
                serde_json::json!({
                    "secs": secs(tt.rule_secs[i]),
                    "from_trip": name(tt.rule_from_trip[i], &tt.gtfs_trip_id),
                    "to_trip": name(tt.rule_to_trip[i], &tt.gtfs_trip_id),
                    "from_route": name(tt.rule_from_route[i], &tt.route_info_id),
                    "to_route": name(tt.rule_to_route[i], &tt.route_info_id),
                })
            })
            .take(50)
            .collect();
        Ok(serde_json::json!({
            "from": from,
            "to": to,
            "same_stop": a == b,
            "change_secs_at_from": secs(tt.stop_change[a]),
            "change_secs_at_to": secs(tt.stop_change[b]),
            "footpath_secs": foot.map(|f| f.1),
            "footpath_explicit": foot.map(|f| f.2),
            "specific_rules": rules,
        }))
    }

    /// Snapshot directory.
    /// IANA time zone of a stop (catalogue id), `None` if unknown.
    pub fn stop_timezone(&self, id: &str) -> Option<&str> {
        let i = self.tt.catalog.lookup(id)?;
        Some(&self.tt.meta.timezones[self.tt.stop_tz[i] as usize])
    }

    /// Walking speed and link radius the snapshot was built with.
    pub fn walk_speed(&self) -> f64 {
        self.tt.meta.walk_speed_m_per_s
    }

    pub fn dir(&self) -> &Path {
        &self.tt.dir
    }

    /// Per-feed diagnostics: import report, published validity and (for
    /// snapshots built by `rustis update`) the download status.
    pub fn feeds(&self) -> Vec<serde_json::Value> {
        let update: Option<crate::update::UpdateReport> =
            std::fs::read(self.tt.dir.join("update.json"))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok());
        let dl: std::collections::HashMap<&str, &crate::update::FeedStatus> = update
            .as_ref()
            .map(|u| u.feeds.iter().map(|f| (f.name.as_str(), f)).collect())
            .unwrap_or_default();
        self.tt
            .meta
            .feeds
            .iter()
            .map(|f| {
                let d = dl.get(f.name.as_str());
                serde_json::json!({
                    "name": f.name,
                    "file": f.file,
                    "imported": f.imported,
                    "error": f.error,
                    "trips": f.trips,
                    "service_range": f.service_range,
                    "download": d.map(|d| &d.status),
                    "download_error": d.and_then(|d| d.error.as_ref()),
                    "last_modified": d.and_then(|d| d.last_modified.as_ref()),
                })
            })
            .collect()
    }

    /// Feed-level problems for `/health`: failed/stale downloads, failed
    /// imports, published validity ending within `days` days from today.
    pub fn feed_warnings(&self, days: i64) -> Vec<String> {
        let mut out = Vec::new();
        let feeds = self.feeds();
        let names = |pred: &dyn Fn(&serde_json::Value) -> bool| -> Vec<String> {
            feeds
                .iter()
                .filter(|f| pred(f))
                .map(|f| f["name"].as_str().unwrap_or("").to_owned())
                .collect()
        };
        let list = |v: &[String]| {
            let mut s = v.iter().take(10).cloned().collect::<Vec<_>>().join(", ");
            if v.len() > 10 {
                s += &format!(", … (+{})", v.len() - 10);
            }
            s
        };
        let stale = names(&|f| f["download"] == "stale");
        if !stale.is_empty() {
            out.push(format!(
                "{} feed(s) could not be refreshed, previous file used: {}",
                stale.len(),
                list(&stale)
            ));
        }
        let failed = names(&|f| f["imported"] == false);
        if !failed.is_empty() {
            out.push(format!(
                "{} feed(s) not imported: {}",
                failed.len(),
                list(&failed)
            ));
        }
        let limit = (jiff::Zoned::now().date() + jiff::Span::new().days(days)).to_string();
        let ending = names(&|f| {
            f["imported"] == true
                && f["trips"].as_u64().unwrap_or(0) > 0
                && f["service_range"][1]
                    .as_str()
                    .is_some_and(|e| e < limit.as_str())
        });
        if !ending.is_empty() {
            out.push(format!(
                "{} feed(s) publish service only until before {limit}: {}",
                ending.len(),
                list(&ending)
            ));
        }
        out
    }

    pub fn warnings(&self) -> Vec<String> {
        self.tt
            .meta
            .warnings
            .iter()
            .map(|w| format!("INCOMPLETE: {w}"))
            .collect()
    }
}

fn signature(j: &Journey) -> Vec<(u32, u32, u32, u32)> {
    j.steps
        .iter()
        .map(|s| match *s {
            Step::Ride {
                trip,
                day,
                board_pos,
                alight_pos,
                ..
            } => (trip, day, board_pos, alight_pos),
            Step::Walk { from, to, .. } => (u32::MAX, from, to, 0),
            Step::Access { stop, .. } => (u32::MAX - 1, stop, 0, 0),
            Step::Egress { stop, .. } => (u32::MAX - 2, stop, 0, 0),
        })
        .collect()
}

/// Convenience: one feed, ids unprefixed, horizon starting at `first_day`.
pub fn import_gtfs(src: &Path, dest: &Path, first_day: &str) -> Result<SnapshotMeta> {
    let d = jiff::civil::Date::strptime("%Y-%m-%d", first_day)?;
    import_gtfs_with(src, dest, &BuildOptions::new(d))
}

/// One feed, ids unprefixed; fails if the feed could not be imported.
pub fn import_gtfs_with(src: &Path, dest: &Path, opts: &BuildOptions) -> Result<SnapshotMeta> {
    let m = build_snapshot(
        &[FeedSpec {
            name: String::new(),
            path: src.to_owned(),
        }],
        dest,
        opts.clone(),
    )?;
    if let Some(e) = m.feeds.iter().find_map(|f| f.error.clone()) {
        let _ = std::fs::remove_dir_all(dest);
        anyhow::bail!("{e}");
    }
    Ok(m)
}
