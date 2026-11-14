//! Opening a snapshot: small topology arrays are decoded into heap (Vulture's
//! `Timetable` trait returns slices of its private-layout index newtypes),
//! while per-trip times, flags and trip IDs stay in the read-only mapping.
use crate::format::{self, FLAG_NO_DROP_OFF, FLAG_NO_PICKUP, Range, Sec, le_u32, le_u64};
use crate::model::{METADATA_VERSION, Metadata, RouteInfo, Stop};
use anyhow::{Context, Result, ensure};
use memmap2::Mmap;
use std::fs::File;
use std::path::{Path, PathBuf};
use vulture::{Duration, RaptorCachePool, RouteIdx, SecondOfDay, StopIdx, Timetable, TripIdx};

pub struct Snapshot {
    pub(crate) dir: PathBuf,
    pub(crate) meta: Metadata,
    pub(crate) metadata_bytes: u64,
    pub(crate) tt: MappedTimetable,
    pub(crate) tz: chrono_tz::Tz,
    pub(crate) service_date: chrono::NaiveDate,
    pub(crate) stop_by_id: std::collections::HashMap<String, StopIdx>,
    pub(crate) caches: RaptorCachePool,
}

impl Snapshot {
    /// Opens and structurally validates a snapshot. GTFS is not read.
    pub fn open(dir: &Path) -> Result<Self> {
        let meta_raw = std::fs::read(dir.join("metadata.json"))
            .with_context(|| format!("reading {}/metadata.json", dir.display()))?;
        let meta: Metadata = serde_json::from_slice(&meta_raw).context("parsing metadata.json")?;
        ensure!(
            meta.version == METADATA_VERSION,
            "unsupported metadata version {}",
            meta.version
        );
        let file = File::open(dir.join("timetable.bin")).context("opening timetable.bin")?;
        let len = file.metadata()?.len();
        ensure!(
            len == meta.timetable_bytes,
            "timetable.bin size {len} != recorded {}",
            meta.timetable_bytes
        );
        // SAFETY: snapshot files are written once, made read-only and never
        // modified in place; truncation by another process while mapped would
        // fault (SIGBUS). This is an operational invariant, documented in README.
        let map = unsafe { Mmap::map(&file) }.context("mapping timetable.bin")?;
        #[cfg(unix)]
        let _ = map.advise(memmap2::Advice::Random);
        let tt = MappedTimetable::new(map, &meta)?;
        let tz: chrono_tz::Tz = meta
            .timezone
            .parse()
            .map_err(|e| anyhow::anyhow!("timezone: {e}"))?;
        let service_date = chrono::NaiveDate::parse_from_str(&meta.service_date, "%Y-%m-%d")?;
        let stop_by_id = meta
            .stops
            .iter()
            .enumerate()
            .map(|(i, s)| (s.stop_id.clone(), StopIdx::new(i as u32)))
            .collect();
        let caches = RaptorCachePool::for_timetable(&tt);
        Ok(Self {
            dir: dir.to_owned(),
            metadata_bytes: meta_raw.len() as u64,
            meta,
            tt,
            tz,
            service_date,
            stop_by_id,
            caches,
        })
    }

    /// Full integrity check: checksum and per-row departure ordering.
    pub fn verify(&self) -> Result<()> {
        let sum = format!("{:016x}", format::fnv64(&self.tt.map));
        ensure!(
            sum == self.meta.timetable_fnv64,
            "checksum mismatch: {sum} != {}",
            self.meta.timetable_fnv64
        );
        for r in 0..self.tt.n_routes() {
            let (n_pos, n_trips) = (self.tt.route_len(r), self.tt.route_trips(r));
            for p in 0..n_pos {
                let base = self.tt.cell(r, p, 0);
                for k in 1..n_trips {
                    ensure!(
                        self.tt.dep(base + k - 1) <= self.tt.dep(base + k),
                        "route {r} position {p}: departures not sorted"
                    );
                }
            }
        }
        Ok(())
    }

    pub(crate) fn stop(&self, s: StopIdx) -> &Stop {
        &self.meta.stops[s.get() as usize]
    }
    pub(crate) fn route_info(&self, r: RouteIdx) -> &RouteInfo {
        &self.meta.routes[self.tt.route_gtfs[r.get() as usize] as usize]
    }
}

pub(crate) struct MappedTimetable {
    pub(crate) map: Mmap,
    n_stops: usize,
    route_stop_offsets: Vec<u32>,
    route_stops: Vec<StopIdx>,
    route_trip_offsets: Vec<u32>,
    route_cell_offsets: Vec<u64>,
    route_gtfs: Vec<u32>,
    stop_route_offsets: Vec<u32>,
    stop_routes: Vec<(RouteIdx, u32)>,
    foot_offsets: Vec<u32>,
    foot_to: Vec<StopIdx>,
    foot_secs: Vec<u32>,
    /// Minimum change time per stop (same-stop rule or import default).
    change_secs: Vec<u32>,
    closed: bool,
    trip_route: Range,
    arrivals: Range,
    departures: Range,
    flags: Range,
    trip_id_offsets: Range,
    trip_id_bytes: Range,
}

fn u32s(map: &[u8], r: Range) -> Vec<u32> {
    (0..r.count).map(|i| le_u32(map, r.start + 4 * i)).collect()
}
fn u64s(map: &[u8], r: Range) -> Vec<u64> {
    (0..r.count).map(|i| le_u64(map, r.start + 8 * i)).collect()
}
/// CSR offsets: `n + 1` entries, starting at 0, non-decreasing, ending at `len`.
fn check_offsets<T: Copy + Into<u64>>(name: &str, o: &[T], n: usize, len: u64) -> Result<()> {
    ensure!(
        o.len() == n + 1,
        "{name}: expected {} offsets, found {}",
        n + 1,
        o.len()
    );
    ensure!(o[0].into() == 0 && o[n].into() == len, "{name}: bad bounds");
    ensure!(
        o.windows(2).all(|w| w[0].into() <= w[1].into()),
        "{name}: not monotone"
    );
    Ok(())
}

impl MappedTimetable {
    fn new(map: Mmap, meta: &Metadata) -> Result<Self> {
        let dir = format::parse_directory(&map, format::MAGIC, &format::ALL_SECTIONS)?;
        let sec = |s: Sec| dir[s as usize];
        let n_stops = meta.stops.len();
        let n_routes = meta.n_raptor_routes;
        let n_trips = meta.n_trips;

        let route_stop_offsets = u32s(&map, sec(Sec::RouteStopOffsets));
        let route_stops_raw = u32s(&map, sec(Sec::RouteStops));
        check_offsets(
            "route stops",
            &route_stop_offsets,
            n_routes,
            route_stops_raw.len() as u64,
        )?;
        ensure!(
            route_stops_raw.iter().all(|&s| (s as usize) < n_stops),
            "route stop out of range"
        );
        ensure!(
            route_stop_offsets.windows(2).all(|w| w[1] - w[0] >= 2),
            "every route must have at least two stops"
        );
        let route_trip_offsets = u32s(&map, sec(Sec::RouteTripOffsets));
        check_offsets("route trips", &route_trip_offsets, n_routes, n_trips as u64)?;
        let route_cell_offsets = u64s(&map, sec(Sec::RouteCellOffsets));
        let n_cells = sec(Sec::Arrivals).count as u64;
        check_offsets("route cells", &route_cell_offsets, n_routes, n_cells)?;
        for r in 0..n_routes {
            let stops = u64::from(route_stop_offsets[r + 1] - route_stop_offsets[r]);
            let trips = u64::from(route_trip_offsets[r + 1] - route_trip_offsets[r]);
            ensure!(
                route_cell_offsets[r + 1] - route_cell_offsets[r] == stops * trips,
                "route {r}: cell count mismatch"
            );
        }
        ensure!(
            sec(Sec::Departures).count as u64 == n_cells && sec(Sec::Flags).count as u64 == n_cells,
            "timing arrays disagree in length"
        );
        let route_gtfs = u32s(&map, sec(Sec::RouteGtfs));
        ensure!(route_gtfs.len() == n_routes, "route info count mismatch");
        ensure!(
            route_gtfs.iter().all(|&g| (g as usize) < meta.routes.len()),
            "route info out of range"
        );

        let stop_route_offsets = u32s(&map, sec(Sec::StopRouteOffsets));
        let sr = u32s(&map, sec(Sec::StopRoutes));
        ensure!(sr.len().is_multiple_of(2), "stop-route pairs truncated");
        check_offsets(
            "stop routes",
            &stop_route_offsets,
            n_stops,
            sr.len() as u64 / 2,
        )?;
        let mut stop_routes = Vec::with_capacity(sr.len() / 2);
        for (s, w) in stop_route_offsets.windows(2).enumerate() {
            for i in w[0] as usize..w[1] as usize {
                let (r, p) = (sr[2 * i] as usize, sr[2 * i + 1]);
                ensure!(r < n_routes, "serving route out of range");
                let (a, b) = (
                    route_stop_offsets[r] as usize,
                    route_stop_offsets[r + 1] as usize,
                );
                ensure!(
                    (p as usize) < b - a && route_stops_raw[a + p as usize] as usize == s,
                    "serving position inconsistent with route stops"
                );
                stop_routes.push((RouteIdx::new(r as u32), p));
            }
        }
        let foot_offsets = u32s(&map, sec(Sec::FootOffsets));
        let foot_to_raw = u32s(&map, sec(Sec::FootTo));
        let foot_secs = u32s(&map, sec(Sec::FootSecs));
        let change_secs = u32s(&map, sec(Sec::StopChangeSecs));
        ensure!(change_secs.len() == n_stops, "change-time count mismatch");
        check_offsets(
            "footpaths",
            &foot_offsets,
            n_stops,
            foot_to_raw.len() as u64,
        )?;
        ensure!(
            foot_secs.len() == foot_to_raw.len(),
            "footpath arrays disagree"
        );
        ensure!(
            foot_to_raw.iter().all(|&s| (s as usize) < n_stops),
            "footpath target out of range"
        );

        let trip_route = sec(Sec::TripRoute);
        ensure!(trip_route.count == n_trips, "trip-route count mismatch");
        // O(trips) but only 4 bytes per trip; guarantees in-range route lookups.
        for t in 0..n_trips {
            let r = le_u32(&map, trip_route.start + 4 * t) as usize;
            ensure!(
                r < n_routes
                    && (route_trip_offsets[r] as usize..route_trip_offsets[r + 1] as usize)
                        .contains(&t),
                "trip {t}: route mapping inconsistent"
            );
        }
        let trip_id_offsets = sec(Sec::TripIdOffsets);
        let trip_id_bytes = sec(Sec::TripIdBytes);
        ensure!(
            trip_id_offsets.count == n_trips + 1,
            "trip id offsets count mismatch"
        );
        let last = le_u64(&map, trip_id_offsets.start + 8 * n_trips);
        ensure!(
            last == trip_id_bytes.count as u64,
            "trip id table truncated"
        );
        Ok(Self {
            n_stops,
            route_stop_offsets,
            route_stops: route_stops_raw.into_iter().map(StopIdx::new).collect(),
            route_trip_offsets,
            route_cell_offsets,
            route_gtfs,
            stop_route_offsets,
            stop_routes,
            foot_offsets,
            foot_to: foot_to_raw.into_iter().map(StopIdx::new).collect(),
            foot_secs,
            change_secs,
            closed: meta.footpaths_closed,
            trip_route,
            arrivals: sec(Sec::Arrivals),
            departures: sec(Sec::Departures),
            flags: sec(Sec::Flags),
            trip_id_offsets,
            trip_id_bytes,
            map,
        })
    }

    pub(crate) fn n_trips(&self) -> usize {
        self.trip_route.count
    }
    pub(crate) fn n_footpaths(&self) -> usize {
        self.foot_to.len()
    }
    /// Bytes of the per-trip sections that are only ever read via the mapping.
    pub(crate) fn mapped_bytes(&self) -> u64 {
        [
            self.trip_route.count * 4,
            self.arrivals.count * 4,
            self.departures.count * 4,
            self.flags.count,
        ]
        .iter()
        .sum::<usize>() as u64
            + (self.trip_id_offsets.count * 8 + self.trip_id_bytes.count) as u64
    }
    fn route_len(&self, r: usize) -> usize {
        (self.route_stop_offsets[r + 1] - self.route_stop_offsets[r]) as usize
    }
    fn route_trips(&self, r: usize) -> usize {
        (self.route_trip_offsets[r + 1] - self.route_trip_offsets[r]) as usize
    }
    /// Global cell index of (route, stop position, trip position).
    #[inline]
    fn cell(&self, r: usize, pos: usize, trip_pos: usize) -> usize {
        self.route_cell_offsets[r] as usize + pos * self.route_trips(r) + trip_pos
    }
    #[inline]
    fn dep(&self, cell: usize) -> u32 {
        le_u32(&self.map, self.departures.start + 4 * cell)
    }
    #[inline]
    fn arr(&self, cell: usize) -> u32 {
        le_u32(&self.map, self.arrivals.start + 4 * cell)
    }
    #[inline]
    fn flag(&self, cell: usize) -> u8 {
        self.map[self.flags.start + cell]
    }
    #[inline]
    fn trip_route_cell(&self, trip: TripIdx, pos: u32) -> (usize, usize) {
        let t = trip.get() as usize;
        let r = le_u32(&self.map, self.trip_route.start + 4 * t) as usize;
        assert!(
            (pos as usize) < self.route_len(r),
            "stop position out of range"
        );
        (
            r,
            self.cell(r, pos as usize, t - self.route_trip_offsets[r] as usize),
        )
    }
    #[inline]
    fn trip_cell(&self, trip: TripIdx, pos: u32) -> usize {
        self.trip_route_cell(trip, pos).1
    }
    /// `(pickup, drop-off)` booking marks of a trip at a stop position.
    pub(crate) fn booking(&self, trip: TripIdx, pos: u32) -> (bool, bool) {
        let f = self.flag(self.trip_cell(trip, pos));
        (
            f & format::FLAG_PICKUP_BOOKING != 0,
            f & format::FLAG_DROP_OFF_BOOKING != 0,
        )
    }
    /// Timetable arrival without the change-time inflation.
    pub(crate) fn raw_arrival(&self, trip: TripIdx, pos: u32) -> u32 {
        self.arr(self.trip_cell(trip, pos))
    }
    pub(crate) fn change_secs(&self, stop: StopIdx) -> u32 {
        self.change_secs[stop.get() as usize]
    }
    pub(crate) fn trip_id(&self, trip: TripIdx) -> String {
        let t = trip.get() as usize;
        let a = le_u64(&self.map, self.trip_id_offsets.start + 8 * t) as usize;
        let b = le_u64(&self.map, self.trip_id_offsets.start + 8 * (t + 1)) as usize;
        let bytes = self
            .map
            .get(self.trip_id_bytes.start + a..self.trip_id_bytes.start + b)
            .unwrap_or(&[]);
        String::from_utf8_lossy(bytes).into_owned()
    }
    /// Position of `stop` on `route` strictly after `after`, if any.
    pub(crate) fn position_after(&self, route: RouteIdx, after: u32, stop: StopIdx) -> Option<u32> {
        let stops = self.get_stops_after(route, after);
        stops
            .iter()
            .skip(1)
            .position(|&s| s == stop)
            .map(|o| after + 1 + o as u32)
    }
    /// First trip at index ≥ the earliest catchable one that allows pickup at
    /// `board` and drop-off at `alight`.
    pub(crate) fn first_usable_trip(
        &self,
        route: RouteIdx,
        at: SecondOfDay,
        board: u32,
        alight: u32,
    ) -> Option<TripIdx> {
        let r = route.get() as usize;
        let first = self.route_trip_offsets[r];
        let n = self.route_trips(r);
        let base = self.cell(r, board as usize, 0);
        let mut k = partition(n, |k| self.dep(base + k) < at.0);
        while k < n {
            let trip = TripIdx::new(first + k as u32);
            if self.pickup_allowed(trip, board) && self.drop_off_allowed(trip, alight) {
                return Some(trip);
            }
            k += 1;
        }
        None
    }
    pub(crate) fn footpaths(&self, stop: StopIdx) -> impl Iterator<Item = (StopIdx, u32)> + '_ {
        let s = stop.get() as usize;
        let (a, b) = (
            self.foot_offsets[s] as usize,
            self.foot_offsets[s + 1] as usize,
        );
        (a..b).map(|i| (self.foot_to[i], self.foot_secs[i]))
    }
    pub(crate) fn route_stop_slice(&self, route: RouteIdx, from: u32, to: u32) -> &[StopIdx] {
        let a = self.route_stop_offsets[route.get() as usize] as usize;
        &self.route_stops[a + from as usize..=a + to as usize]
    }
}

/// `partition_point` over `0..n` without materialising a slice.
fn partition(n: usize, pred: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0, n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if pred(mid) { lo = mid + 1 } else { hi = mid }
    }
    lo
}

impl Timetable for MappedTimetable {
    fn n_stops(&self) -> usize {
        self.n_stops
    }
    fn n_routes(&self) -> usize {
        self.route_gtfs.len()
    }
    fn get_routes_serving_stop(&self, stop: StopIdx) -> &[(RouteIdx, u32)] {
        let s = stop.get() as usize;
        &self.stop_routes
            [self.stop_route_offsets[s] as usize..self.stop_route_offsets[s + 1] as usize]
    }
    fn get_stops_after(&self, route: RouteIdx, pos: u32) -> &[StopIdx] {
        let r = route.get() as usize;
        let (a, b) = (
            self.route_stop_offsets[r] as usize,
            self.route_stop_offsets[r + 1] as usize,
        );
        &self.route_stops[a + pos as usize..b]
    }
    fn stop_at(&self, route: RouteIdx, pos: u32) -> StopIdx {
        self.get_stops_after(route, pos)[0]
    }
    fn get_earliest_trip(&self, route: RouteIdx, at: SecondOfDay, pos: u32) -> Option<TripIdx> {
        let r = route.get() as usize;
        let n = self.route_trips(r);
        let base = self.cell(r, pos as usize, 0);
        let mut k = partition(n, |k| self.dep(base + k) < at.0);
        // Skip trips without pickup here, as Vulture's GTFS adapter does.
        while k < n {
            if self.flag(base + k) & FLAG_NO_PICKUP == 0 {
                return Some(TripIdx::new(self.route_trip_offsets[r] + k as u32));
            }
            k += 1;
        }
        None
    }
    /// Arrival as seen by RAPTOR: the stop's minimum change time is added, so
    /// re-boarding at the same stop requires `departure ≥ arrival + change`.
    /// RAPTOR labels carry no provenance, so walking off after alighting is
    /// delayed by the same amount (conservative; never admits a too-short
    /// change). Origin seeds and walked arrivals are not inflated. Rustis
    /// reports real times via [`MappedTimetable::raw_arrival`].
    fn get_arrival_time(&self, trip: TripIdx, pos: u32) -> SecondOfDay {
        let (r, cell) = self.trip_route_cell(trip, pos);
        let stop = self.route_stops[self.route_stop_offsets[r] as usize + pos as usize];
        SecondOfDay(
            self.arr(cell)
                .saturating_add(self.change_secs[stop.get() as usize]),
        )
    }
    fn get_departure_time(&self, trip: TripIdx, pos: u32) -> SecondOfDay {
        SecondOfDay(self.dep(self.trip_cell(trip, pos)))
    }
    fn pickup_allowed(&self, trip: TripIdx, pos: u32) -> bool {
        self.flag(self.trip_cell(trip, pos)) & FLAG_NO_PICKUP == 0
    }
    fn drop_off_allowed(&self, trip: TripIdx, pos: u32) -> bool {
        self.flag(self.trip_cell(trip, pos)) & FLAG_NO_DROP_OFF == 0
    }
    fn get_footpaths_from(&self, stop: StopIdx) -> &[StopIdx] {
        let s = stop.get() as usize;
        &self.foot_to[self.foot_offsets[s] as usize..self.foot_offsets[s + 1] as usize]
    }
    fn get_transfer_time(&self, from: StopIdx, to: StopIdx) -> Duration {
        let s = from.get() as usize;
        let (a, b) = (
            self.foot_offsets[s] as usize,
            self.foot_offsets[s + 1] as usize,
        );
        let i = (a..b)
            .find(|&i| self.foot_to[i] == to)
            .expect("transfer time requested for a non-footpath");
        Duration(self.foot_secs[i])
    }
    fn footpaths_are_transitively_closed(&self) -> bool {
        self.closed
    }
}
