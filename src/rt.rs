//! Realtime overlay (GTFS-RT TripUpdates) on top of an immutable snapshot.
//!
//! Identity never changes: a trip is still `(route, trip, day)` of the base
//! snapshot. The overlay only replaces the *times* of trips the feed reports
//! (delays, skipped stops, platform changes) and marks cancelled trips. The
//! feed is decoded entity by entity from the stream: decoding it as a whole
//! needs several hundred MB, one entity needs a few KB.
//!
//! Safety net: a feed entry is only applied if its scheduled time (feed time
//! minus feed delay) is within [`MAX_SCHED_DIFF`] of ours. Trip ids are reused
//! across feed versions, so unchecked matches would put delays on wrong trips;
//! those are dropped and counted.
use crate::build::NONE;
use crate::raptor::FxMap;
use crate::tt::Timetable;
use serde::Serialize;
use std::io::Read;

/// Largest accepted difference between the feed's scheduled time and ours.
const MAX_SCHED_DIFF: i64 = 120;
/// Delays beyond this are not believed (seconds).
const MAX_DELAY: i64 = 86_400;

/// Realtime data of one trip on one service day.
#[derive(Debug, Default, Clone)]
pub(crate) struct TripRt {
    pub cancelled: bool,
    /// (arrival, departure) per route position, relative seconds. Empty when
    /// cancelled.
    pub times: Vec<(u32, u32)>,
    /// Stop is skipped (no boarding or alighting); empty = none.
    pub skip: Vec<bool>,
    /// Stop actually used per position when it differs from the scheduled one
    /// (`NONE` = as scheduled); empty = none. Gives the changed platform.
    pub stop: Vec<u32>,
}

impl TripRt {
    pub fn skipped(&self, pos: usize) -> bool {
        self.skip.get(pos).copied().unwrap_or(false)
    }
    /// The stop used at `pos` if it differs from the scheduled one.
    pub fn changed_stop(&self, pos: usize) -> Option<u32> {
        self.stop.get(pos).copied().filter(|&s| s != NONE)
    }
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct RtStats {
    pub updated_at: String,
    pub entities: u64,
    pub trip_updates: u64,
    /// Not found in the snapshot (unknown id or not running that day).
    pub unmatched: u64,
    /// Found, but the scheduled time disagrees with ours: dropped.
    pub dropped_mismatch: u64,
    pub delayed: u64,
    pub cancelled: u64,
    pub skipped_stops: u64,
    pub platform_changes: u64,
    /// Trips the feed adds that the snapshot does not have (not shown yet).
    pub added: u64,
    pub stops_not_on_trip: u64,
    pub errors: u64,
}

#[derive(Default)]
pub struct Rt {
    over: FxMap<u64, TripRt>,
    /// route → its overridden (local trip index, day) pairs.
    by_route: FxMap<u32, Vec<(u32, u32)>>,
    route_bits: Vec<u64>,
    pub stats: RtStats,
}

fn key(trip: u32, day: u32) -> u64 {
    u64::from(trip) << 32 | u64::from(day)
}

impl Rt {
    pub fn is_empty(&self) -> bool {
        self.over.is_empty()
    }
    pub(crate) fn get(&self, trip: u32, day: u32) -> Option<&TripRt> {
        if self.over.is_empty() {
            return None;
        }
        self.over.get(&key(trip, day))
    }
    #[inline]
    pub(crate) fn has_route(&self, r: u32) -> bool {
        self.route_bits
            .get(r as usize / 64)
            .is_some_and(|w| w >> (r % 64) & 1 == 1)
    }
    /// Overridden `(local trip index, day)` pairs of route `r`.
    pub(crate) fn route_trips(&self, r: u32) -> &[(u32, u32)] {
        self.by_route.get(&r).map_or(&[], Vec::as_slice)
    }

    fn insert(&mut self, tt: &Timetable, route: u32, trip: u32, day: u32, t: TripRt) {
        if self.route_bits.is_empty() {
            self.route_bits = vec![0; tt.meta.n_routes / 64 + 1];
        }
        self.route_bits[route as usize / 64] |= 1 << (route % 64);
        let j = trip - tt.route_trip_off[route as usize];
        self.by_route.entry(route).or_default().push((j, day));
        self.over.insert(key(trip, day), t);
    }

    /// Reads one GTFS-RT feed (stream) into the overlay. `prefix` is put in
    /// front of feed trip and stop ids to get snapshot ids.
    pub fn apply_feed(&mut self, tt: &Timetable, prefix: &str, r: impl Read) {
        let mut buf = Vec::new();
        let mut rd = Stream(r);
        loop {
            let (field, wire) = match rd.tag() {
                Ok(Some(x)) => x,
                Ok(None) => break,
                Err(_) => {
                    self.stats.errors += 1;
                    break;
                }
            };
            if field == 2 && wire == 2 {
                let Ok(len) = rd.varint() else { break };
                if len > 16 << 20 {
                    self.stats.errors += 1;
                    break;
                }
                buf.clear();
                buf.resize(len as usize, 0);
                if rd.0.read_exact(&mut buf).is_err() {
                    self.stats.errors += 1;
                    break;
                }
                self.stats.entities += 1;
                self.entity(tt, prefix, &buf);
            } else if rd.skip(wire).is_err() {
                self.stats.errors += 1;
                break;
            }
        }
    }

    fn entity(&mut self, tt: &Timetable, prefix: &str, buf: &[u8]) {
        for (f, v) in Fields(buf) {
            if f == 3
                && let Val::Bytes(b) = v
            {
                self.stats.trip_updates += 1;
                if let Some(u) = TripUpd::parse(b) {
                    self.trip_update(tt, prefix, &u);
                } else {
                    self.stats.errors += 1;
                }
            }
        }
    }

    fn trip_update(&mut self, tt: &Timetable, prefix: &str, u: &TripUpd) {
        // schedule_relationship: 0 SCHEDULED, 1 ADDED, 3 CANCELED, 7 DELETED
        if u.rel == 1 {
            self.stats.added += 1;
            return;
        }
        let cancel = matches!(u.rel, 3 | 7);
        if u.rel != 0 && !cancel {
            return;
        }
        let Some(day) = parse_day(tt, u.start_date) else {
            self.stats.unmatched += 1;
            return;
        };
        let id = format!("{prefix}{}", u.trip_id);
        let Some(g) = tt.ix.gtfs_trip(&tt.gtfs_trip_id, &id) else {
            self.stats.unmatched += 1;
            return;
        };
        let mut found = false;
        for &trip in tt.ix.trips_of(g) {
            if !tt.active(tt.trip_bits[trip as usize], day as usize) {
                continue;
            }
            let route = (tt.route_trip_off.partition_point(|&o| o <= trip) - 1) as u32;
            let j = (trip - tt.route_trip_off[route as usize]) as usize;
            if let Some(hm) = u.start_time
                && let Some(want) = parse_hms(hm)
                && tt.deps(route as usize, 0)[j].abs_diff(want) > MAX_SCHED_DIFF as u32
            {
                continue;
            }
            found = true;
            if cancel {
                self.stats.cancelled += 1;
                self.insert(
                    tt,
                    route,
                    trip,
                    day,
                    TripRt {
                        cancelled: true,
                        ..Default::default()
                    },
                );
                continue;
            }
            match compute(tt, prefix, route, j, trip, day, u) {
                Compute::Mismatch => self.stats.dropped_mismatch += 1,
                Compute::Same => {}
                Compute::Changed(t, c) => {
                    self.stats.delayed += u64::from(c.delayed);
                    self.stats.skipped_stops += c.skipped;
                    self.stats.platform_changes += c.platform;
                    self.stats.stops_not_on_trip += c.not_on_trip;
                    self.insert(tt, route, trip, day, t);
                }
            }
        }
        if !found {
            self.stats.unmatched += 1;
        }
    }
}

#[derive(Default)]
struct Counts {
    delayed: bool,
    skipped: u64,
    platform: u64,
    not_on_trip: u64,
}

enum Compute {
    Mismatch,
    Same,
    Changed(TripRt, Counts),
}

fn compute(
    tt: &Timetable,
    prefix: &str,
    route: u32,
    j: usize,
    trip: u32,
    day: u32,
    u: &TripUpd,
) -> Compute {
    let _ = trip;
    let r = route as usize;
    let n = tt.n_route_stops(r);
    let base = tt.base[tt.route_tz[r] as usize][day as usize];
    let sched: Vec<(u32, u32)> = (0..n)
        .map(|p| (base + tt.arrs(r, p)[j], base + tt.deps(r, p)[j]))
        .collect();
    // explicit (arrival, departure) delays per position
    let mut ex: Vec<(Option<i64>, Option<i64>)> = vec![(None, None); n];
    let mut skip = vec![false; n];
    let mut stop = vec![NONE; n];
    let mut c = Counts::default();
    let mut cursor = 0usize;
    for s in &u.stus {
        let feed_stop = (!s.stop_id.is_empty())
            .then(|| tt.catalog.lookup(&format!("{prefix}{}", s.stop_id)))
            .flatten()
            .map(|x| x as u32);
        let pos = feed_stop.and_then(|fs| {
            let g = tt.ix.stop_group[fs as usize];
            (cursor..n).find(|&p| {
                let rs = tt.route_stop(r, p);
                rs == fs || (g != NONE && tt.ix.stop_group[rs as usize] == g)
            })
        });
        let Some(p) = pos else {
            c.not_on_trip += 1;
            continue;
        };
        cursor = p + 1;
        let fs = feed_stop.unwrap();
        if fs != tt.route_stop(r, p) {
            stop[p] = fs;
        }
        match s.rel {
            1 => {
                skip[p] = true;
                c.skipped += 1;
                continue;
            }
            2 | 3 => continue, // NO_DATA / UNSCHEDULED: carry the delay
            _ => {}
        }
        let one = |ev: &Option<Ev>, sched_t: u32| -> Result<Option<i64>, ()> {
            let Some(e) = ev else { return Ok(None) };
            let t = e.time.map(|t| t - tt.origin);
            match (e.delay, t) {
                (Some(d), Some(t)) => {
                    // validate against our schedule
                    if (t - d - i64::from(sched_t)).abs() > MAX_SCHED_DIFF {
                        return Err(());
                    }
                    Ok(Some(d))
                }
                (Some(d), None) => Ok(Some(d)),
                (None, Some(t)) => Ok(Some(t - i64::from(sched_t))),
                (None, None) => Ok(None),
            }
        };
        let a = match one(&s.arr, sched[p].0) {
            Ok(x) => x,
            Err(()) => return Compute::Mismatch,
        };
        let d = match one(&s.dep, sched[p].1) {
            Ok(x) => x,
            Err(()) => return Compute::Mismatch,
        };
        if a.is_some_and(|x| x.abs() > MAX_DELAY) || d.is_some_and(|x| x.abs() > MAX_DELAY) {
            return Compute::Mismatch;
        }
        ex[p] = (a, d);
    }
    // propagate delays along the trip
    let mut cur = 0i64;
    let mut times = Vec::with_capacity(n);
    let mut prev_dep = 0u32;
    let mut changed = false;
    for p in 0..n {
        let (ea, ed) = ex[p];
        let ad = ea.or(ed).unwrap_or(cur);
        let dd = ed.or(ea).unwrap_or(cur);
        cur = dd;
        let arr = (i64::from(sched[p].0) + ad).max(i64::from(prev_dep)) as u32;
        let dep = ((i64::from(sched[p].1) + dd).max(0) as u32).max(arr);
        changed |= arr != sched[p].0 || dep != sched[p].1;
        prev_dep = dep;
        times.push((arr, dep));
    }
    c.delayed = changed;
    c.platform = stop.iter().filter(|&&s| s != NONE).count() as u64;
    if !changed && c.skipped == 0 && c.platform == 0 {
        return Compute::Same;
    }
    Compute::Changed(
        TripRt {
            cancelled: false,
            times,
            skip: if c.skipped > 0 { skip } else { Vec::new() },
            stop: if c.platform > 0 { stop } else { Vec::new() },
        },
        c,
    )
}

fn parse_day(tt: &Timetable, date: &str) -> Option<u32> {
    if date.len() != 8 {
        return None;
    }
    let d = jiff::civil::Date::strptime("%Y%m%d", date).ok()?;
    crate::board::day_of(tt, d)
}

fn parse_hms(s: &str) -> Option<u32> {
    let mut it = s.split(':');
    let (h, m, sec): (u32, u32, u32) = (
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next().unwrap_or("0").parse().ok()?,
    );
    Some(h * 3600 + m * 60 + sec)
}

// ---------------------------------------------------------------- protobuf

struct Ev {
    delay: Option<i64>,
    time: Option<i64>,
}

struct Stu<'a> {
    stop_id: &'a str,
    rel: u64,
    arr: Option<Ev>,
    dep: Option<Ev>,
}

struct TripUpd<'a> {
    trip_id: &'a str,
    start_date: &'a str,
    start_time: Option<&'a str>,
    rel: u64,
    stus: Vec<Stu<'a>>,
}

enum Val<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Other,
}

/// Fields of one protobuf message held in memory.
struct Fields<'a>(&'a [u8]);

impl<'a> Iterator for Fields<'a> {
    type Item = (u64, Val<'a>);
    fn next(&mut self) -> Option<Self::Item> {
        if self.0.is_empty() {
            return None;
        }
        let tag = varint(&mut self.0)?;
        let (f, wire) = (tag >> 3, tag & 7);
        let v = match wire {
            0 => Val::Varint(varint(&mut self.0)?),
            1 => {
                self.0 = self.0.get(8..)?;
                Val::Other
            }
            2 => {
                let len = varint(&mut self.0)? as usize;
                let b = self.0.get(..len)?;
                self.0 = &self.0[len..];
                Val::Bytes(b)
            }
            5 => {
                self.0 = self.0.get(4..)?;
                Val::Other
            }
            _ => {
                self.0 = &[];
                return None;
            }
        };
        Some((f, v))
    }
}

fn varint(b: &mut &[u8]) -> Option<u64> {
    let mut v = 0u64;
    for i in 0..10 {
        let x = *b.get(i)?;
        v |= u64::from(x & 0x7f) << (7 * i);
        if x & 0x80 == 0 {
            *b = &b[i + 1..];
            return Some(v);
        }
    }
    None
}

fn text(b: &[u8]) -> &str {
    std::str::from_utf8(b).unwrap_or("")
}

impl<'a> TripUpd<'a> {
    fn parse(b: &'a [u8]) -> Option<Self> {
        let mut u = TripUpd {
            trip_id: "",
            start_date: "",
            start_time: None,
            rel: 0,
            stus: Vec::new(),
        };
        for (f, v) in Fields(b) {
            match (f, v) {
                (1, Val::Bytes(d)) => {
                    for (f, v) in Fields(d) {
                        match (f, v) {
                            (1, Val::Bytes(s)) => u.trip_id = text(s),
                            (2, Val::Bytes(s)) => u.start_time = Some(text(s)),
                            (3, Val::Bytes(s)) => u.start_date = text(s),
                            (4, Val::Varint(x)) => u.rel = x,
                            _ => {}
                        }
                    }
                }
                (2, Val::Bytes(d)) => u.stus.push(Stu::parse(d)),
                _ => {}
            }
        }
        (!u.trip_id.is_empty()).then_some(u)
    }
}

impl<'a> Stu<'a> {
    fn parse(b: &'a [u8]) -> Self {
        let mut s = Stu {
            stop_id: "",
            rel: 0,
            arr: None,
            dep: None,
        };
        for (f, v) in Fields(b) {
            match (f, v) {
                (2, Val::Bytes(d)) => s.arr = Some(Ev::parse(d)),
                (3, Val::Bytes(d)) => s.dep = Some(Ev::parse(d)),
                (4, Val::Bytes(d)) => s.stop_id = text(d),
                (5, Val::Varint(x)) => s.rel = x,
                _ => {}
            }
        }
        s
    }
}

impl Ev {
    fn parse(b: &[u8]) -> Self {
        let mut e = Ev {
            delay: None,
            time: None,
        };
        for (f, v) in Fields(b) {
            match (f, v) {
                (1, Val::Varint(x)) => e.delay = Some(i64::from(x as i32)),
                (2, Val::Varint(x)) => e.time = Some(x as i64),
                _ => {}
            }
        }
        e
    }
}

/// Top-level reader over a stream.
struct Stream<R>(R);

impl<R: Read> Stream<R> {
    fn byte(&mut self) -> std::io::Result<Option<u8>> {
        let mut b = [0u8];
        match self.0.read(&mut b)? {
            0 => Ok(None),
            _ => Ok(Some(b[0])),
        }
    }
    fn varint(&mut self) -> std::io::Result<u64> {
        let mut v = 0u64;
        for i in 0..10 {
            let x = self.byte()?.ok_or(std::io::ErrorKind::UnexpectedEof)?;
            v |= u64::from(x & 0x7f) << (7 * i);
            if x & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(std::io::ErrorKind::InvalidData.into())
    }
    fn tag(&mut self) -> std::io::Result<Option<(u64, u64)>> {
        // clean EOF only before a tag
        let Some(first) = self.byte()? else {
            return Ok(None);
        };
        let mut v = u64::from(first & 0x7f);
        let mut i = 1;
        let mut x = first;
        while x & 0x80 != 0 {
            x = self.byte()?.ok_or(std::io::ErrorKind::UnexpectedEof)?;
            v |= u64::from(x & 0x7f) << (7 * i);
            i += 1;
            if i > 10 {
                return Err(std::io::ErrorKind::InvalidData.into());
            }
        }
        Ok(Some((v >> 3, v & 7)))
    }
    fn skip(&mut self, wire: u64) -> std::io::Result<()> {
        let n = match wire {
            0 => {
                self.varint()?;
                return Ok(());
            }
            1 => 8,
            5 => 4,
            2 => self.varint()?,
            _ => return Err(std::io::ErrorKind::InvalidData.into()),
        };
        std::io::copy(&mut (&mut self.0).take(n), &mut std::io::sink())?;
        Ok(())
    }
}

// ----------------------------------------------------------------- updater

/// A realtime source: feed URL and the id prefix of its feed in the snapshot.
#[derive(Clone, Debug)]
pub struct RtSource {
    pub url: String,
    pub prefix: String,
}

impl std::str::FromStr for RtSource {
    type Err = String;
    /// `URL` or `URL|ID_PREFIX`.
    fn from_str(s: &str) -> Result<Self, String> {
        let (url, prefix) = s.rsplit_once('|').unwrap_or((s, ""));
        Ok(Self {
            url: url.to_owned(),
            prefix: prefix.to_owned(),
        })
    }
}

fn fetch(url: &str) -> anyhow::Result<impl Read + use<>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(180)))
        .build()
        .into();
    let resp = agent
        .get(url)
        .header("User-Agent", "rustis-realtime")
        .call()?;
    Ok(resp.into_body().into_with_config().limit(u64::MAX).reader())
}

/// Fetches all sources and builds an overlay for `tt`.
pub(crate) fn build(tt: &Timetable, sources: &[RtSource]) -> (Rt, Vec<String>) {
    let mut rt = Rt::default();
    let mut errs = Vec::new();
    for s in sources {
        match fetch(&s.url) {
            Ok(r) => rt.apply_feed(tt, &s.prefix, std::io::BufReader::with_capacity(1 << 16, r)),
            Err(e) => {
                rt.stats.errors += 1;
                errs.push(format!("{}: {e:#}", s.url));
            }
        }
    }
    rt.stats.updated_at = jiff::Timestamp::now().to_string();
    (rt, errs)
}

/// Background thread: refreshes the overlay of the served snapshot every
/// `every` seconds. A failing fetch keeps the previous overlay.
pub fn spawn_updater(
    handle: std::sync::Arc<crate::http::SnapshotHandle>,
    sources: Vec<RtSource>,
    every: u64,
) {
    std::thread::spawn(move || {
        loop {
            let snap = handle.get();
            let (rt, errs) = build(snap.timetable(), &sources);
            for e in &errs {
                eprintln!("realtime fetch failed: {e}");
            }
            if errs.len() < sources.len() {
                eprintln!(
                    "realtime: {} trips changed, {} cancelled, {} dropped, {} unmatched",
                    rt.over.len() as u64 - rt.stats.cancelled,
                    rt.stats.cancelled,
                    rt.stats.dropped_mismatch,
                    rt.stats.unmatched
                );
                snap.set_rt(rt);
            }
            drop(snap);
            std::thread::sleep(std::time::Duration::from_secs(every));
        }
    });
}
