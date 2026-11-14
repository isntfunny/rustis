//! Offline import: many GTFS feeds → one immutable snapshot directory.
//!
//! Every feed is streamed (`csv` over the zip member) and processed trip by
//! trip; the whole feed is validated in memory first and only then committed
//! to the snapshot, so a broken feed is reported (`failed_feeds`) without
//! leaving partial data behind. IDs are namespaced `dataset:id` when several
//! feeds are combined.
//!
//! Model (see docs/decisions/2026-11-14-architecture.md):
//! * one traffic-day bitfield per trip (deduplicated table), bit 0 = the day
//!   before `first_day` (overnight trips), `horizon_days + 1` bits;
//! * times are GTFS-local seconds since "noon − 12 h" of the service day
//!   (may exceed 24 h; the router scans back over as many service days as the
//!   route's longest time needs);
//! * routes = trips with identical stop sequence and pickup/drop-off flags,
//!   split so trips never overtake each other (also across service days);
//! * frequencies.txt trips are expanded into individual trips.
#[path = "netex.rs"]
mod netex;

use crate::catalog;
use crate::feed::{Feed, field, parse_date, parse_time, text};
use crate::model::Stop;
use crate::store::{ArrayWriter, Manifest, write_all};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

pub const FLAG_NO_PICKUP: u8 = 1;
pub const FLAG_NO_DROP_OFF: u8 = 2;
pub const FLAG_PICKUP_BOOKING: u8 = 4;
pub const FLAG_DROP_OFF_BOOKING: u8 = 8;
/// Change time value meaning "no transfer possible at this stop" (type 3).
pub const FORBIDDEN: u32 = u32::MAX;
pub const NONE: u32 = u32::MAX;
pub const SNAPSHOT_VERSION: u32 = 6;
/// Bit set in `foot_secs` for footpaths given explicitly by transfers.txt
/// (the walk is not raised to the change time of the stop it starts at).
pub const FOOT_EXPLICIT: u32 = 1 << 31;

#[derive(Clone, Debug)]
pub struct BuildOptions {
    /// First service day of the horizon (`YYYY-MM-DD`).
    pub first_day: jiff::civil::Date,
    /// Number of service days from `first_day` (MOTIS: 365).
    pub horizon_days: u32,
    /// Generated footpaths between stops closer than this (cross-feed linking).
    pub link_radius_m: f64,
    pub walk_speed_m_per_s: f64,
    /// Footpaths are closed transitively up to this duration.
    pub max_footpath_secs: u32,
    /// Minimum change time at stops without a same-stop transfers.txt rule.
    pub default_change_seconds: u32,
    /// Treat pickup/drop_off 2/3 (booking) as not available.
    pub exclude_booking_stops: bool,
    /// Comparison mode: footpaths and change times as MOTIS/nigiri builds them
    /// (within a feed only stops ≤ 100 m apart, ≥ 2 min; across feeds ≤ 300 m
    /// at 1.5 m/s in whole minutes; transfers.txt footpaths made symmetric;
    /// minute-truncated rule times; the *shortest* same-stop rule wins).
    pub motis_footpaths: bool,
}

impl BuildOptions {
    /// `YYYY-MM-DD` convenience (panics on a malformed date; for tests/CLI).
    pub fn for_date(first_day: &str) -> Self {
        Self::new(jiff::civil::Date::strptime("%Y-%m-%d", first_day).expect("YYYY-MM-DD"))
    }
    pub fn new(first_day: jiff::civil::Date) -> Self {
        Self {
            first_day,
            horizon_days: 366,
            link_radius_m: 0.0,
            walk_speed_m_per_s: 1.2,
            max_footpath_secs: 720,
            default_change_seconds: 0,
            exclude_booking_stops: false,
            motis_footpaths: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct FeedSpec {
    /// Namespace (MOTIS dataset name); empty = ids are used unprefixed.
    pub name: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FeedReport {
    pub name: String,
    pub file: String,
    pub imported: bool,
    pub error: Option<String>,
    pub stops: u64,
    pub gtfs_trips: u64,
    pub trips: u64,
    pub routes: u64,
    pub stop_times: u64,
    pub timezone: String,
    pub service_range: Option<[String; 2]>,
    /// Counters of everything not routed or only partly honoured.
    pub skipped: BTreeMap<String, u64>,
    pub transfer_rules: BTreeMap<String, u64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SnapshotMeta {
    pub version: u32,
    pub rustis_version: String,
    pub first_day: String,
    /// Bit 0 of every traffic-day bitfield is `first_day − 1`.
    pub horizon_days: u32,
    pub bitfield_words: u32,
    pub timezones: Vec<String>,
    pub n_stops: usize,
    pub n_routes: usize,
    pub n_trips: usize,
    pub n_gtfs_trips: usize,
    pub n_route_infos: usize,
    pub n_bitfields: usize,
    pub n_footpaths: usize,
    pub n_specific_rules: usize,
    pub max_time: u32,
    pub link_radius_m: f64,
    pub walk_speed_m_per_s: f64,
    pub max_footpath_secs: u32,
    pub default_change_seconds: u32,
    pub exclude_booking_stops: bool,
    pub feeds: Vec<FeedReport>,
    pub warnings: Vec<String>,
    pub manifest: Manifest,
    pub build_seconds: f64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Rule {
    pub to_stop: u32,
    pub from_stop: u32,
    pub from_trip: u32,
    pub from_route: u32,
    pub to_trip: u32,
    pub to_route: u32,
    /// Minimum seconds between arrival and departure, or FORBIDDEN.
    pub secs: u32,
}

/// One trip ready for routing (times are `[arr0, dep0, arr1, dep1, …]`).
struct Trip {
    times: Vec<u32>,
    bits: u32,
    gtfs: u32,
}

struct Pattern {
    stops: Vec<u32>,
    flags: Vec<u8>,
    tz: u16,
    info: u32,
    trips: Vec<Trip>,
}

/// Everything one feed contributes; committed only if the whole feed parsed.
struct FeedData {
    report: FeedReport,
    stops: Vec<Stop>,
    stop_tz: Vec<u16>,
    change: Vec<(u32, u32)>,
    route_infos: Vec<RouteInfo>,
    gtfs_trip_ids: Vec<String>,
    /// `trip_headsign` per GTFS trip (parallel to `gtfs_trip_ids`; empty = none).
    gtfs_trip_headsigns: Vec<String>,
    patterns: Vec<Pattern>,
    footpaths: Vec<(u32, u32, u32)>,
    forbidden: Vec<(u32, u32)>,
    rules: Vec<Rule>,
    /// Feed-local tables referenced by `stop_tz`/`Pattern::tz` and `Trip::bits`.
    tz_names: Vec<String>,
    bitfield_table: Vec<u64>,
    max_time: u32,
}

/// Display attributes of one GTFS route (or NeTEx line).
pub(crate) struct RouteInfo {
    pub id: String,
    pub line: String,
    pub route_type: u16,
    /// `route_color` as given (hex without `#`), empty if none.
    pub color: String,
    /// Name of the operating agency, empty if unknown.
    pub agency: String,
}

/// A parsed, validated feed (feed-local indices) ready to be committed.
pub struct ParsedFeed(FeedData);

impl ParsedFeed {
    pub fn report(&self) -> &FeedReport {
        &self.0.report
    }
}

/// Parses and validates one feed without touching a builder (thread-safe).
pub fn parse_feed(spec: &FeedSpec, opts: &BuildOptions) -> Result<ParsedFeed> {
    let mut p = Parser {
        opts,
        words: (opts.horizon_days as usize + 1).div_ceil(64),
        tz_names: Vec::new(),
        bitfields: HashMap::new(),
        bitfield_table: Vec::new(),
        max_time: 0,
    };
    p.read_feed(spec).map(ParsedFeed)
}

pub struct Builder {
    opts: BuildOptions,
    dir: PathBuf,
    words: usize,
    tz_names: Vec<String>,
    /// Index of the feed (in `feeds`) each stop came from.
    stop_feed: Vec<u32>,
    stops: Vec<Stop>,
    stop_tz: Vec<u16>,
    change: Vec<u32>,
    bitfields: HashMap<Box<[u64]>, u32>,
    bitfield_table: Vec<u64>,
    route_info_id: StrWriter,
    route_info_line: StrWriter,
    route_info_type: Vec<u16>,
    route_info_color: StrWriter,
    route_info_agency: StrWriter,
    gtfs_trip_id: StrWriter,
    n_gtfs_trips: u32,
    /// Per GTFS trip: index into `headsigns` (deduplicated strings; 0 = none).
    gtfs_trip_headsign: ArrayWriter<u32>,
    headsigns: StrWriter,
    headsign_idx: HashMap<String, u32>,
    route_stop_off: Vec<u64>,
    route_stops: Vec<u32>,
    route_flags: Vec<u8>,
    route_trip_off: Vec<u32>,
    route_time_off: Vec<u64>,
    route_tz: Vec<u16>,
    route_info: Vec<u32>,
    route_max_time: Vec<u32>,
    /// Union of the traffic days of a route's trips (bitfield index).
    route_days: Vec<u32>,
    trip_bits: ArrayWriter<u32>,
    trip_gtfs: ArrayWriter<u32>,
    dep: ArrayWriter<u32>,
    arr: ArrayWriter<u32>,
    lb_edges: Vec<(u32, u32, u32)>,
    footpaths: Vec<(u32, u32, u32)>,
    forbidden: HashSet<(u32, u32)>,
    rules: Vec<Rule>,
    feeds: Vec<FeedReport>,
    max_time: u32,
    manifest: Manifest,
}

/// Streaming string table (`<name>.off` u64 offsets + `<name>.bytes`).
struct StrWriter {
    off: ArrayWriter<u64>,
    bytes: ArrayWriter<u8>,
}

impl StrWriter {
    fn create(dir: &Path, name: &str) -> Result<Self> {
        let mut off = ArrayWriter::create(dir, &format!("{name}.off"))?;
        off.push(0)?;
        Ok(Self {
            off,
            bytes: ArrayWriter::create(dir, &format!("{name}.bytes"))?,
        })
    }
    fn push(&mut self, s: &str) -> Result<()> {
        self.bytes.extend(s.bytes())?;
        self.off.push(self.bytes.len())
    }
    fn finish(self, m: &mut Manifest) -> Result<()> {
        self.off.finish(m)?;
        self.bytes.finish(m)
    }
}

fn ns(name: &str, id: &str) -> String {
    if name.is_empty() {
        id.to_owned()
    } else {
        format!("{name}:{id}")
    }
}

pub fn mode_name(route_type: u16) -> &'static str {
    match route_type {
        0 | 900..=999 => "tram",
        1 | 400..=499 => "subway",
        2 | 100..=199 => "rail",
        3 | 700..=799 => "bus",
        4 | 1000..=1099 | 1200..=1299 => "ferry",
        5 => "cable_tram",
        6 | 1300..=1399 => "aerial_lift",
        7 | 1400..=1499 => "funicular",
        11 | 800..=899 => "trolleybus",
        12 => "monorail",
        200..=299 => "coach",
        1500..=1599 => "taxi",
        _ => "other",
    }
}

/// UTC instant (unix seconds) of "noon − 12 h" local time on `date` in `tz`.
pub fn noon_minus_12h(tz: &jiff::tz::TimeZone, date: jiff::civil::Date) -> Result<i64> {
    let noon = date.at(12, 0, 0, 0).to_zoned(tz.clone())?;
    Ok(noon.timestamp().as_second() - 12 * 3600)
}

impl Builder {
    pub fn new(dir: &Path, opts: BuildOptions) -> Result<Self> {
        ensure!(
            opts.horizon_days > 0 && opts.horizon_days <= 3660,
            "horizon_days out of range"
        );
        let words = (opts.horizon_days as usize + 1).div_ceil(64);
        Ok(Self {
            words,
            tz_names: Vec::new(),
            stops: Vec::new(),
            stop_feed: Vec::new(),
            stop_tz: Vec::new(),
            change: Vec::new(),
            bitfields: HashMap::new(),
            bitfield_table: Vec::new(),
            route_info_id: StrWriter::create(dir, "route_info_id")?,
            route_info_line: StrWriter::create(dir, "route_info_line")?,
            route_info_type: Vec::new(),
            route_info_color: StrWriter::create(dir, "route_info_color")?,
            route_info_agency: StrWriter::create(dir, "route_info_agency")?,
            gtfs_trip_id: StrWriter::create(dir, "gtfs_trip_id")?,
            n_gtfs_trips: 0,
            gtfs_trip_headsign: ArrayWriter::create(dir, "gtfs_trip_headsign")?,
            headsigns: {
                let mut w = StrWriter::create(dir, "headsigns")?;
                w.push("")?;
                w
            },
            headsign_idx: HashMap::new(),
            route_stop_off: vec![0],
            route_stops: Vec::new(),
            route_flags: Vec::new(),
            route_trip_off: vec![0],
            route_time_off: vec![0],
            route_tz: Vec::new(),
            route_info: Vec::new(),
            route_max_time: Vec::new(),
            route_days: Vec::new(),
            trip_bits: ArrayWriter::create(dir, "trip_bits")?,
            trip_gtfs: ArrayWriter::create(dir, "trip_gtfs")?,
            dep: ArrayWriter::create(dir, "dep")?,
            arr: ArrayWriter::create(dir, "arr")?,
            lb_edges: Vec::new(),
            footpaths: Vec::new(),
            forbidden: HashSet::new(),
            rules: Vec::new(),
            feeds: Vec::new(),
            max_time: 0,
            manifest: Manifest::new(),
            dir: dir.to_owned(),
            opts,
        })
    }

    fn tz_index(&mut self, name: &str) -> Result<u16> {
        if let Some(i) = self.tz_names.iter().position(|t| t == name) {
            return Ok(i as u16);
        }
        jiff::tz::TimeZone::get(name).with_context(|| format!("unknown time zone {name:?}"))?;
        self.tz_names.push(name.to_owned());
        Ok((self.tz_names.len() - 1) as u16)
    }

    fn bitfield(&mut self, bits: &[u64]) -> u32 {
        if let Some(&i) = self.bitfields.get(bits) {
            return i;
        }
        let i = (self.bitfield_table.len() / self.words) as u32;
        self.bitfield_table.extend_from_slice(bits);
        self.bitfields.insert(bits.into(), i);
        i
    }

    /// Imports one feed; failures are recorded in the report, not propagated.
    pub fn add_feed(&mut self, spec: &FeedSpec) -> FeedReport {
        let t0 = std::time::Instant::now();
        let parsed = parse_feed(spec, &self.opts);
        self.add_parsed(spec, parsed, t0)
    }

    /// Commits a feed parsed with [`parse_feed`] (possibly on another thread);
    /// `t0` is when its processing started (for the log line).
    pub fn add_parsed(
        &mut self,
        spec: &FeedSpec,
        parsed: Result<ParsedFeed>,
        t0: std::time::Instant,
    ) -> FeedReport {
        let mut report = match parsed {
            Ok(ParsedFeed(data)) => self.commit(data),
            Err(e) => FeedReport {
                name: spec.name.clone(),
                file: spec.path.display().to_string(),
                error: Some(format!("{e:#}")),
                ..Default::default()
            },
        };
        report.skipped.retain(|_, v| *v > 0);
        eprintln!(
            "feed {} imported={} trips={} stop_times={} {:.1}s rss={}MiB {}",
            spec.name,
            report.imported,
            report.trips,
            report.stop_times,
            t0.elapsed().as_secs_f64(),
            crate::memory::rss_mib().unwrap_or(0),
            report.error.as_deref().unwrap_or("")
        );
        self.feeds.push(report.clone());
        report
    }
}

/// Feed parser: produces feed-local indices (stops, route infos, GTFS trips,
/// time zones, bitfields), remapped when the feed is committed. Independent
/// of the builder, so several feeds can be parsed in parallel.
struct Parser<'a> {
    opts: &'a BuildOptions,
    words: usize,
    tz_names: Vec<String>,
    bitfields: HashMap<Box<[u64]>, u32>,
    bitfield_table: Vec<u64>,
    max_time: u32,
}

impl Parser<'_> {
    fn tz_index(&mut self, name: &str) -> Result<u16> {
        if let Some(i) = self.tz_names.iter().position(|t| t == name) {
            return Ok(i as u16);
        }
        jiff::tz::TimeZone::get(name).with_context(|| format!("unknown time zone {name:?}"))?;
        self.tz_names.push(name.to_owned());
        Ok((self.tz_names.len() - 1) as u16)
    }

    fn bitfield(&mut self, bits: &[u64]) -> u32 {
        if let Some(&i) = self.bitfields.get(bits) {
            return i;
        }
        let i = (self.bitfield_table.len() / self.words) as u32;
        self.bitfield_table.extend_from_slice(bits);
        self.bitfields.insert(bits.into(), i);
        i
    }

    fn read_feed(&mut self, spec: &FeedSpec) -> Result<FeedData> {
        let name = spec.name.as_str();
        let mut feed = Feed::open(&spec.path)?;
        let members = feed.members();
        if !members.iter().any(|m| m == "stops.txt") && members.iter().any(|m| m.ends_with(".xml"))
        {
            return netex::read(self, spec);
        }
        for f in ["stops.txt", "trips.txt", "stop_times.txt", "routes.txt"] {
            ensure!(
                members.iter().any(|m| m == f),
                "not a GTFS feed: {f} missing (members: {})",
                members
                    .iter()
                    .take(5)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let mut report = FeedReport {
            name: name.to_owned(),
            file: spec
                .path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default(),
            ..Default::default()
        };
        let mut skip = BTreeMap::<String, u64>::new();

        // agency.txt: one time zone per feed (GTFS requires all agencies to agree).
        let mut tz_name = None::<String>;
        let mut agencies: HashMap<Box<[u8]>, String> = HashMap::new();
        let mut first_agency = None::<String>;
        feed.rows("agency.txt", |c, r| {
            let agency_name = text(r, c.get("agency_name"));
            first_agency.get_or_insert_with(|| agency_name.clone());
            agencies.insert(field(r, c.get("agency_id")).into(), agency_name);
            let t = text(r, c.get("agency_timezone"));
            match &tz_name {
                None if !t.is_empty() => tz_name = Some(t),
                Some(x) if !t.is_empty() && *x != t => {
                    bail!("agencies disagree on the time zone ({x} vs {t}); GTFS requires one per feed")
                }
                _ => {}
            }
            Ok(())
        })?;
        let tz_name = tz_name.context("agency.txt: no agency_timezone")?;
        let tz = self.tz_index(&tz_name)?;
        report.timezone = tz_name;

        // stops.txt
        let stop_base = 0u32;
        let mut stops = Vec::new();
        let mut stop_tz = Vec::new();
        let mut stop_idx: HashMap<Box<[u8]>, u32> = HashMap::new();
        let mut parents: Vec<(u32, Vec<u8>)> = Vec::new();
        feed.rows("stops.txt", |c, r| {
            let id = field(r, c.get("stop_id"));
            if id.is_empty() {
                return Ok(());
            }
            let gi = stop_base + stops.len() as u32;
            if stop_idx.insert(id.into(), gi).is_some() {
                bail!("duplicate stop_id {:?}", String::from_utf8_lossy(id));
            }
            let coord = |k: &str| {
                std::str::from_utf8(field(r, c.get(k)))
                    .ok()
                    .and_then(|v| v.parse::<f64>().ok())
            };
            let (lat, lon) = match (coord("stop_lat"), coord("stop_lon")) {
                (Some(a), Some(b)) if a.is_finite() && b.is_finite() && (a, b) != (0.0, 0.0) => {
                    (Some(a), Some(b))
                }
                _ => (None, None),
            };
            let p = field(r, c.get("parent_station"));
            if !p.is_empty() {
                parents.push((gi - stop_base, p.to_vec()));
            }
            let stz = text(r, c.get("stop_timezone"));
            stop_tz.push(stz);
            stops.push(Stop {
                stop_id: ns(name, &String::from_utf8_lossy(id)),
                name: text(r, c.get("stop_name")),
                lat,
                lon,
                parent_station: None,
                served: false,
            });
            Ok(())
        })?;
        for (i, p) in parents {
            if stop_idx.contains_key(p.as_slice()) {
                stops[i as usize].parent_station = Some(ns(name, &String::from_utf8_lossy(&p)));
            } else {
                *skip.entry("unknown_parent_station".into()).or_default() += 1;
            }
        }
        let stop_tz = stop_tz
            .into_iter()
            .map(|t| {
                if t.is_empty() {
                    Ok(tz)
                } else {
                    self.tz_index(&t).or(Ok(tz))
                }
            })
            .collect::<Result<Vec<u16>>>()?;
        report.stops = stops.len() as u64;

        // routes.txt
        let info_base = 0u32;
        let mut route_infos = Vec::new();
        let mut route_idx: HashMap<Box<[u8]>, u32> = HashMap::new();
        feed.rows("routes.txt", |c, r| {
            let id = field(r, c.get("route_id"));
            let short = text(r, c.get("route_short_name"));
            let line = if short.is_empty() {
                text(r, c.get("route_long_name"))
            } else {
                short
            };
            let rt = std::str::from_utf8(field(r, c.get("route_type")))
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(u16::MAX);
            route_idx.insert(id.into(), info_base + route_infos.len() as u32);
            let agency = agencies
                .get(field(r, c.get("agency_id")))
                .or(first_agency.as_ref())
                .cloned()
                .unwrap_or_default();
            route_infos.push(RouteInfo {
                id: ns(name, &String::from_utf8_lossy(id)),
                line,
                route_type: rt,
                color: text(r, c.get("route_color")),
                agency,
            });
            Ok(())
        })?;

        // calendar.txt + calendar_dates.txt → bitfield per service (horizon).
        let d0 = crate::feed::day_number(self.opts.first_day) - 1;
        let nbits = self.opts.horizon_days as i32 + 1;
        let words = self.words;
        let mut services: HashMap<Box<[u8]>, Vec<u64>> = HashMap::new();
        let (mut lo, mut hi) = (i32::MAX, i32::MIN);
        let mut beyond = 0u64;
        feed.rows("calendar.txt", |c, r| {
            let s = parse_date(field(r, c.get("start_date")))?;
            let e = parse_date(field(r, c.get("end_date")))?;
            let wd: Vec<bool> = [
                "monday",
                "tuesday",
                "wednesday",
                "thursday",
                "friday",
                "saturday",
                "sunday",
            ]
            .iter()
            .map(|d| field(r, c.get(d)) == b"1")
            .collect();
            let bits = services
                .entry(field(r, c.get("service_id")).into())
                .or_insert_with(|| vec![0; words]);
            if e >= s {
                lo = lo.min(s);
                hi = hi.max(e);
            }
            if e >= d0 + nbits {
                beyond += 1;
            }
            for day in s.max(d0)..=e.min(d0 + nbits - 1) {
                // 1970-01-01 was a Thursday (index 3 from Monday).
                if wd[((day + 3).rem_euclid(7)) as usize] {
                    let b = (day - d0) as usize;
                    bits[b / 64] |= 1 << (b % 64);
                }
            }
            Ok(())
        })?;
        feed.rows("calendar_dates.txt", |c, r| {
            let d = parse_date(field(r, c.get("date")))?;
            let add = match field(r, c.get("exception_type")) {
                b"1" => true,
                b"2" => false,
                other => bail!("bad exception_type {:?}", String::from_utf8_lossy(other)),
            };
            let bits = services
                .entry(field(r, c.get("service_id")).into())
                .or_insert_with(|| vec![0; words]);
            if add {
                lo = lo.min(d);
                hi = hi.max(d);
            }
            if (d0..d0 + nbits).contains(&d) {
                let b = (d - d0) as usize;
                if add {
                    bits[b / 64] |= 1 << (b % 64);
                } else {
                    bits[b / 64] &= !(1 << (b % 64));
                }
            } else if add && d >= d0 + nbits {
                beyond += 1;
            }
            Ok(())
        })?;
        if hi >= lo {
            report.service_range = Some([
                crate::feed::date_of(lo).to_string(),
                crate::feed::date_of(hi).to_string(),
            ]);
        }
        skip.insert("service_rows_beyond_horizon".into(), beyond);

        // trips.txt
        struct TripMeta {
            info: u32,
            service: Option<Box<[u8]>>,
            gtfs: u32,
            block: Option<Box<[u8]>>,
        }
        let gtfs_base = 0u32;
        let mut gtfs_trip_ids = Vec::new();
        let mut gtfs_trip_headsigns = Vec::new();
        let mut trips: HashMap<Box<[u8]>, TripMeta> = HashMap::new();
        feed.rows("trips.txt", |c, r| {
            let id = field(r, c.get("trip_id"));
            let Some(&info) = route_idx.get(field(r, c.get("route_id"))) else {
                *skip.entry("trips_unknown_route".into()).or_default() += 1;
                return Ok(());
            };
            let sid = field(r, c.get("service_id"));
            let service = services.contains_key(sid).then(|| sid.into());
            let gtfs = gtfs_base + gtfs_trip_ids.len() as u32;
            let block = field(r, c.get("block_id"));
            let block = (!block.is_empty()).then(|| block.into());
            if trips
                .insert(
                    id.into(),
                    TripMeta {
                        info,
                        service,
                        gtfs,
                        block,
                    },
                )
                .is_some()
            {
                bail!("duplicate trip_id {:?}", String::from_utf8_lossy(id));
            }
            gtfs_trip_ids.push(ns(name, &String::from_utf8_lossy(id)));
            gtfs_trip_headsigns.push(text(r, c.get("trip_headsign")));
            Ok(())
        })?;
        report.gtfs_trips = gtfs_trip_ids.len() as u64;

        // frequencies.txt
        // trip_id → (start, end, headway)
        type Freqs = HashMap<Box<[u8]>, Vec<(u32, u32, u32)>>;
        let mut freqs: Freqs = HashMap::new();
        feed.rows("frequencies.txt", |c, r| {
            let s = parse_time(field(r, c.get("start_time")))?.context("start_time")?;
            let e = parse_time(field(r, c.get("end_time")))?.context("end_time")?;
            let h: u32 = std::str::from_utf8(field(r, c.get("headway_secs")))?
                .parse()
                .context("headway_secs")?;
            if h == 0 || e <= s {
                *skip.entry("frequency_rows_invalid".into()).or_default() += 1;
                return Ok(());
            }
            // exact_times 0 ("about every h seconds") is expanded like exact_times 1;
            // the response marks such legs as schedule-based (headway) service.
            freqs
                .entry(field(r, c.get("trip_id")).into())
                .or_default()
                .push((s, e, h));
            Ok(())
        })?;

        // stop_times.txt, trip by trip.
        struct Row {
            seq: u32,
            stop: u32,
            arr: Option<u32>,
            dep: Option<u32>,
            flags: u8,
        }
        let excl_booking = self.opts.exclude_booking_stops;
        let flag_of = |v: &[u8], none: u8, booking: u8| -> u8 {
            match v {
                b"" | b"0" => 0,
                b"2" | b"3" if !excl_booking => booking,
                _ => none, // 1, booking when excluded, unknown values
            }
        };
        let mut pattern_idx: HashMap<(Vec<u32>, Vec<u8>, u32), usize> = HashMap::new();
        let mut patterns: Vec<Pattern> = Vec::new();
        let mut cur: Option<Box<[u8]>> = None;
        let mut rows: Vec<Row> = Vec::new();
        let mut bad_trip = false;
        let mut seen_trips: HashSet<u32> = HashSet::new();
        let mut n_rows = 0u64;
        let mut n_trips = 0u64;
        let mut tmp_bits = vec![0u64; words];
        type BlockEnd = (Box<[u8]>, Box<[u8]>, u32, u32, u32, u32, u32);
        let mut block_ends: Vec<BlockEnd> = Vec::new();
        let mut finish = |trip_id: &[u8],
                          rows: &mut Vec<Row>,
                          bad: bool,
                          skip: &mut BTreeMap<String, u64>,
                          builder: &mut Parser<'_>|
         -> Result<()> {
            let take = std::mem::take(rows);
            let Some(meta) = trips.get(trip_id) else {
                *skip.entry("stop_times_unknown_trip".into()).or_default() += take.len() as u64;
                return Ok(());
            };
            if !seen_trips.insert(meta.gtfs) {
                bail!(
                    "stop_times of trip {:?} are not contiguous",
                    String::from_utf8_lossy(trip_id)
                );
            }
            if bad {
                *skip.entry("flex_or_unknown_stop_trips".into()).or_default() += 1;
                return Ok(());
            }
            let mut take = take;
            take.sort_by_key(|r| r.seq);
            if take.len() < 2 {
                *skip
                    .entry("trips_with_fewer_than_2_stops".into())
                    .or_default() += 1;
                return Ok(());
            }
            let Some(service) = &meta.service else {
                *skip.entry("trips_unknown_service".into()).or_default() += 1;
                return Ok(());
            };
            // Times: interpolate missing ones linearly by straight-line distance.
            let n = take.len();
            let mut t: Vec<(u32, u32)> = Vec::with_capacity(n);
            let timed: Vec<usize> = (0..n)
                .filter(|&i| take[i].arr.is_some() || take[i].dep.is_some())
                .collect();
            if timed.first() != Some(&0) || timed.last() != Some(&(n - 1)) {
                *skip
                    .entry("trips_without_first_or_last_time".into())
                    .or_default() += 1;
                return Ok(());
            }
            let coord = |i: usize| {
                let s = &stops[(take[i].stop - stop_base) as usize];
                s.lat.zip(s.lon)
            };
            for w in timed.windows(2) {
                let (a, b) = (w[0], w[1]);
                let ta = take[a].dep.or(take[a].arr).unwrap();
                let tb = take[b].arr.or(take[b].dep).unwrap();
                if t.len() == a {
                    let ar = take[a].arr.or(take[a].dep).unwrap();
                    t.push((ar, ta.max(ar)));
                }
                if b > a + 1 {
                    let mut d = vec![0.0f64; b - a + 1];
                    for i in a + 1..=b {
                        let seg = match (coord(i - 1), coord(i)) {
                            (Some(p), Some(q)) => haversine(p, q),
                            _ => 1.0,
                        };
                        d[i - a] = d[i - a - 1] + seg.max(1e-3);
                    }
                    for i in a + 1..b {
                        let x = ta as f64 + (tb.saturating_sub(ta)) as f64 * d[i - a] / d[b - a];
                        let x = x.round() as u32;
                        t.push((x, x));
                    }
                    *skip.entry("interpolated_stop_times".into()).or_default() +=
                        (b - a - 1) as u64;
                }
                let br = take[b].arr.or(take[b].dep).unwrap();
                t.push((br, take[b].dep.unwrap_or(br).max(br)));
            }
            if t.windows(2).any(|w| w[1].0 < w[0].1) {
                *skip
                    .entry("trips_with_decreasing_times".into())
                    .or_default() += 1;
                return Ok(());
            }
            let base_bits = &services[service];
            if let (Some(block), false) = (&meta.block, freqs.contains_key(trip_id)) {
                // (block, service, first stop, first departure, last stop, last arrival, gtfs trip)
                block_ends.push((
                    block.clone(),
                    service.clone(),
                    take[0].stop,
                    t[0].1,
                    take[n - 1].stop,
                    t[n - 1].0,
                    meta.gtfs,
                ));
            }
            let starts: Vec<u32> = match freqs.get(trip_id) {
                None => vec![t[0].1],
                Some(f) => {
                    let mut v = Vec::new();
                    for &(s, e, h) in f {
                        let mut x = s;
                        while x < e {
                            v.push(x);
                            x += h;
                        }
                    }
                    *skip.entry("frequency_trips_expanded".into()).or_default() += 1;
                    v
                }
            };
            let stops_seq: Vec<u32> = take.iter().map(|r| r.stop).collect();
            let flags: Vec<u8> = take
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let mut f = r.flags;
                    if i == 0 {
                        f |= FLAG_NO_DROP_OFF;
                        f &= !FLAG_DROP_OFF_BOOKING;
                    }
                    if i == n - 1 {
                        f |= FLAG_NO_PICKUP;
                        f &= !FLAG_PICKUP_BOOKING;
                    }
                    f
                })
                .collect();
            let key = (stops_seq, flags, meta.info);
            let pi = match pattern_idx.get(&key) {
                Some(&p) => p,
                None => {
                    let p = patterns.len();
                    patterns.push(Pattern {
                        stops: key.0.clone(),
                        flags: key.1.clone(),
                        tz,
                        info: meta.info,
                        trips: Vec::new(),
                    });
                    pattern_idx.insert(key, p);
                    p
                }
            };
            for start in starts {
                // Times stay relative to their own service day (no day
                // normalisation: on DST days consecutive service-day bases are
                // not 86400 s apart, so shifting by a day would be wrong).
                let off = start as i64 - t[0].1 as i64;
                tmp_bits.copy_from_slice(base_bits);
                if tmp_bits.iter().all(|&w| w == 0) {
                    *skip
                        .entry("trips_not_running_in_horizon".into())
                        .or_default() += 1;
                    continue;
                }
                let times: Vec<u32> = t
                    .iter()
                    .flat_map(|&(a, d)| [(a as i64 + off) as u32, (d as i64 + off) as u32])
                    .collect();
                builder.max_time = builder.max_time.max(*times.last().unwrap());
                let bits = builder.bitfield(&tmp_bits);
                patterns[pi].trips.push(Trip {
                    times,
                    bits,
                    gtfs: meta.gtfs,
                });
                n_trips += 1;
            }
            Ok(())
        };
        feed.rows("stop_times.txt", |c, r| {
            n_rows += 1;
            let tid = field(r, c.get("trip_id"));
            if cur.as_deref() != Some(tid) {
                if let Some(prev) = cur.take() {
                    finish(&prev, &mut rows, bad_trip, &mut skip, self)?;
                }
                cur = Some(tid.into());
                bad_trip = false;
            }
            let sid = field(r, c.get("stop_id"));
            let window = !field(r, c.get("start_pickup_drop_off_window")).is_empty();
            let Some(&stop) = stop_idx.get(sid) else {
                bad_trip = true; // GTFS-Flex location/group or unknown stop
                return Ok(());
            };
            if window {
                bad_trip = true;
            }
            let seq: u32 = std::str::from_utf8(field(r, c.get("stop_sequence")))?
                .parse()
                .context("stop_sequence")?;
            let arr = parse_time(field(r, c.get("arrival_time")))?;
            let dep = parse_time(field(r, c.get("departure_time")))?;
            let flags = flag_of(
                field(r, c.get("pickup_type")),
                FLAG_NO_PICKUP,
                FLAG_PICKUP_BOOKING,
            ) | flag_of(
                field(r, c.get("drop_off_type")),
                FLAG_NO_DROP_OFF,
                FLAG_DROP_OFF_BOOKING,
            );
            rows.push(Row {
                seq,
                stop,
                arr,
                dep,
                flags,
            });
            Ok(())
        })?;
        if let Some(prev) = cur.take() {
            finish(&prev, &mut rows, bad_trip, &mut skip, self)?;
        }
        report.stop_times = n_rows;
        // Stay-seated continuations: consecutive trips of the same block and
        // service that end/start at the same stop (≤ 1 h layover) may be
        // continued without a change time (GTFS block_id semantics).
        let mut stay_rules: Vec<Rule> = Vec::new();
        block_ends.sort_unstable_by(|x, y| (&x.0, &x.1, x.3).cmp(&(&y.0, &y.1, y.3)));
        for w in block_ends.windows(2) {
            let (x, y) = (&w[0], &w[1]);
            if x.0 == y.0 && x.1 == y.1 && x.4 == y.2 && y.3 >= x.5 && y.3 - x.5 <= 3600 {
                stay_rules.push(Rule {
                    to_stop: y.2,
                    from_stop: x.4,
                    from_trip: x.6,
                    from_route: NONE,
                    to_trip: y.6,
                    to_route: NONE,
                    secs: 0,
                });
            }
        }
        skip.insert("block_stay_seated_rules".into(), stay_rules.len() as u64);
        report.trips = n_trips;
        report.routes = patterns.len() as u64;

        // transfers.txt
        let mut change: Vec<(u32, u32)> = Vec::new();
        let mut footpaths = Vec::new();
        let mut forbidden = Vec::new();
        let mut rules = stay_rules;
        let mut tr = BTreeMap::<String, u64>::new();
        let motis_mode = self.opts.motis_footpaths;
        let walk = self.opts.walk_speed_m_per_s;
        let gtfs_of = |id: &[u8]| trips.get(id).map(|m| m.gtfs);
        feed.rows("transfers.txt", |c, r| {
            let ty = match field(r, c.get("transfer_type")) {
                b"" => 0,
                v => std::str::from_utf8(v)?
                    .parse::<u8>()
                    .context("transfer_type")?,
            };
            let (fs, ts) = (
                field(r, c.get("from_stop_id")),
                field(r, c.get("to_stop_id")),
            );
            let (Some(&a), Some(&b)) = (stop_idx.get(fs), stop_idx.get(ts)) else {
                *tr.entry("unknown_or_missing_stop".into()).or_default() += 1;
                return Ok(());
            };
            let mtt: Option<u32> = std::str::from_utf8(field(r, c.get("min_transfer_time")))
                .ok()
                .and_then(|v| v.parse().ok());
            let (ft, tt_) = (
                field(r, c.get("from_trip_id")),
                field(r, c.get("to_trip_id")),
            );
            let (fr, rt) = (
                field(r, c.get("from_route_id")),
                field(r, c.get("to_route_id")),
            );
            let specific = !(ft.is_empty() && tt_.is_empty() && fr.is_empty() && rt.is_empty());
            let key = |k: &str| format!("{}type{ty}{k}", if specific { "specific_" } else { "" });
            // MOTIS comparison mode: nigiri folds trip/route-specific min-time
            // rows into the stop's (minimum) transfer time and ignores
            // specific forbidden rows.
            if motis_mode && specific && ty == 2 {
                if a == b {
                    change.push((a, mtt.unwrap_or(0)));
                } else {
                    footpaths.push((a, b, mtt.unwrap_or(0)));
                }
                *tr.entry(key("_motis_mode_folded")).or_default() += 1;
                return Ok(());
            }
            if motis_mode && specific && ty == 3 {
                *tr.entry(key("_motis_mode_ignored")).or_default() += 1;
                return Ok(());
            }
            match (ty, specific) {
                (2..=4, true) => {
                    let resolve = |v: &[u8], f: &dyn Fn(&[u8]) -> Option<u32>| -> Option<u32> {
                        if v.is_empty() { Some(NONE) } else { f(v) }
                    };
                    let (Some(from_trip), Some(to_trip), Some(from_route), Some(to_route)) = (
                        resolve(ft, &gtfs_of),
                        resolve(tt_, &gtfs_of),
                        resolve(fr, &|v| route_idx.get(v).copied()),
                        resolve(rt, &|v| route_idx.get(v).copied()),
                    ) else {
                        *tr.entry(key("_unknown_trip_or_route")).or_default() += 1;
                        return Ok(());
                    };
                    // 4 = stay seated: continuing on the given trip needs no change time
                    let secs = match ty {
                        3 => FORBIDDEN,
                        4 => 0,
                        _ => mtt.unwrap_or(0),
                    };
                    rules.push(Rule {
                        to_stop: b,
                        from_stop: a,
                        from_trip,
                        from_route,
                        to_trip,
                        to_route,
                        secs,
                    });
                    *tr.entry(key("_enforced")).or_default() += 1;
                }
                (2, false) if a == b => {
                    change.push((a, mtt.unwrap_or(0)));
                    *tr.entry(key("_same_stop_change_time")).or_default() += 1;
                }
                (3, false) if a == b => {
                    change.push((a, FORBIDDEN));
                    *tr.entry(key("_same_stop_forbidden")).or_default() += 1;
                }
                (0 | 2, false) => {
                    let geo = match (
                        stops[(a - stop_base) as usize]
                            .lat
                            .zip(stops[(a - stop_base) as usize].lon),
                        stops[(b - stop_base) as usize]
                            .lat
                            .zip(stops[(b - stop_base) as usize].lon),
                    ) {
                        (Some(p), Some(q)) => Some((haversine(p, q) / walk).ceil() as u32),
                        _ => None,
                    };
                    let secs = if ty == 2 { mtt.or(geo) } else { geo.max(mtt) };
                    match secs {
                        Some(s) => {
                            footpaths.push((a, b, s));
                            *tr.entry(key("_footpath")).or_default() += 1;
                        }
                        None => *tr.entry(key("_no_time_no_coords")).or_default() += 1,
                    }
                }
                (3, false) => {
                    forbidden.push((a, b));
                    *tr.entry(key("_forbidden_pair")).or_default() += 1;
                }
                // 0 specific ("recommended"), 1 (timed), 4 without trips, 5: not exploited.
                // Ignoring them can only miss connections, never create invalid ones.
                _ => *tr.entry(key("_not_exploited")).or_default() += 1,
            }
            Ok(())
        })?;
        report.transfer_rules = tr;
        report.skipped = skip;
        report.imported = true;
        Ok(FeedData {
            report,
            stops,
            stop_tz,
            change,
            route_infos,
            gtfs_trip_ids,
            gtfs_trip_headsigns,
            patterns,
            footpaths,
            forbidden,
            rules,
            tz_names: std::mem::take(&mut self.tz_names),
            bitfield_table: std::mem::take(&mut self.bitfield_table),
            max_time: self.max_time,
        })
    }
}

impl Builder {
    fn commit(&mut self, d: FeedData) -> FeedReport {
        match self.commit_inner(d) {
            Ok(r) => r,
            Err(e) => panic!("committing a validated feed failed (snapshot unusable): {e:#}"),
        }
    }

    fn commit_inner(&mut self, d: FeedData) -> Result<FeedReport> {
        // feed-local → global indices
        let base = self.stops.len() as u32;
        let info_base = self.route_info_type.len() as u32;
        let gtfs_base = self.n_gtfs_trips;
        let tz_map = d
            .tz_names
            .iter()
            .map(|t| self.tz_index(t))
            .collect::<Result<Vec<u16>>>()?;
        let words = self.words;
        let bits_map: Vec<u32> = d
            .bitfield_table
            .chunks_exact(words)
            .map(|b| self.bitfield(b))
            .collect();
        self.max_time = self.max_time.max(d.max_time);
        let feed_no = self.feeds.len() as u32;
        self.stop_feed
            .extend(std::iter::repeat_n(feed_no, d.stops.len()));
        self.stops.extend(d.stops);
        self.stop_tz
            .extend(d.stop_tz.iter().map(|&t| tz_map[t as usize]));
        self.change
            .resize(self.stops.len(), self.opts.default_change_seconds);
        let mut explicit: HashMap<u32, u32> = HashMap::new();
        let motis = self.opts.motis_footpaths;
        for (s, secs) in d.change {
            let s = s + base;
            // several rows for one stop: the strictest one wins (MOTIS mode:
            // the shortest, in whole minutes, as nigiri does)
            if motis {
                let secs = if secs == FORBIDDEN {
                    secs
                } else {
                    secs / 60 * 60
                };
                let e = explicit.entry(s).or_insert(u32::MAX);
                *e = (*e).min(secs);
            } else {
                let e = explicit.entry(s).or_insert(0);
                *e = (*e).max(secs);
            }
        }
        for (s, secs) in explicit {
            self.change[s as usize] = secs;
        }
        for ri in d.route_infos {
            self.route_info_id.push(&ri.id)?;
            self.route_info_line.push(&ri.line)?;
            self.route_info_type.push(ri.route_type);
            self.route_info_color.push(&ri.color)?;
            self.route_info_agency.push(&ri.agency)?;
        }
        for id in &d.gtfs_trip_ids {
            self.gtfs_trip_id.push(id)?;
        }
        for h in d.gtfs_trip_headsigns {
            let i = if h.is_empty() {
                0
            } else if let Some(&i) = self.headsign_idx.get(&h) {
                i
            } else {
                let i = self.headsign_idx.len() as u32 + 1;
                self.headsigns.push(&h)?;
                self.headsign_idx.insert(h, i);
                i
            };
            self.gtfs_trip_headsign.push(i)?;
        }
        self.n_gtfs_trips += d.gtfs_trip_ids.len() as u32;
        let mut routes = 0u64;
        for p in d.patterns {
            let stops: Vec<u32> = p.stops.iter().map(|&s| s + base).collect();
            let mut trips = p.trips;
            for t in &mut trips {
                t.bits = bits_map[t.bits as usize];
                t.gtfs += gtfs_base;
            }
            for route in split_fifo(trips) {
                self.write_route(
                    &stops,
                    &p.flags,
                    tz_map[p.tz as usize],
                    p.info + info_base,
                    route,
                )?;
                routes += 1;
            }
        }
        self.footpaths.extend(
            d.footpaths
                .into_iter()
                .map(|(a, b, t)| (a + base, b + base, t)),
        );
        self.forbidden
            .extend(d.forbidden.into_iter().map(|(a, b)| (a + base, b + base)));
        let opt = |x: u32, off: u32| if x == NONE { NONE } else { x + off };
        self.rules.extend(d.rules.into_iter().map(|r| Rule {
            to_stop: r.to_stop + base,
            from_stop: r.from_stop + base,
            from_trip: opt(r.from_trip, gtfs_base),
            from_route: opt(r.from_route, info_base),
            to_trip: opt(r.to_trip, gtfs_base),
            to_route: opt(r.to_route, info_base),
            secs: r.secs,
        }));
        let mut r = d.report;
        r.routes = routes;
        Ok(r)
    }

    fn write_route(
        &mut self,
        stops: &[u32],
        flags: &[u8],
        tz: u16,
        info: u32,
        trips: Vec<Trip>,
    ) -> Result<()> {
        let n = stops.len();
        self.route_stops.extend_from_slice(stops);
        self.route_flags.extend_from_slice(flags);
        self.route_stop_off.push(self.route_stops.len() as u64);
        let mut days = vec![0u64; self.words];
        for t in &trips {
            let b = t.bits as usize * self.words;
            for (w, x) in days.iter_mut().zip(&self.bitfield_table[b..b + self.words]) {
                *w |= x;
            }
        }
        let days = self.bitfield(&days);
        self.route_days.push(days);
        for t in &trips {
            self.trip_bits.push(t.bits)?;
            self.trip_gtfs.push(t.gtfs)?;
        }
        let last = *self.route_trip_off.last().unwrap();
        self.route_trip_off.push(last + trips.len() as u32);
        for pos in 0..n {
            self.dep
                .extend(trips.iter().map(|t| t.times[2 * pos + 1]))?;
            self.arr.extend(trips.iter().map(|t| t.times[2 * pos]))?;
        }
        self.route_time_off.push(self.dep.len());
        self.route_tz.push(tz);
        self.route_info.push(info);
        self.route_max_time.push(
            trips
                .iter()
                .map(|t| *t.times.last().unwrap())
                .max()
                .unwrap_or(0),
        );
        for pos in 0..n - 1 {
            let ride = trips
                .iter()
                .map(|t| t.times[2 * pos + 2] - t.times[2 * pos + 1])
                .min()
                .unwrap_or(0);
            if stops[pos] != stops[pos + 1] {
                self.lb_edges.push((stops[pos], stops[pos + 1], ride));
            }
        }
        Ok(())
    }

    /// Writes all remaining arrays, the catalogue and `snapshot.json`.
    pub fn finish(mut self, t0: std::time::Instant) -> Result<SnapshotMeta> {
        let dir = self.dir.clone();
        let mut manifest = std::mem::take(&mut self.manifest);
        let m = &mut manifest;
        let n_stops = self.stops.len();
        let n_routes = self.route_tz.len();
        // stop → (route, position)
        let mut sr: Vec<(u32, u32, u32)> = Vec::with_capacity(self.route_stops.len());
        for r in 0..n_routes {
            let (a, b) = (
                self.route_stop_off[r] as usize,
                self.route_stop_off[r + 1] as usize,
            );
            for (pos, &s) in self.route_stops[a..b].iter().enumerate() {
                sr.push((s, r as u32, pos as u32));
            }
        }
        sr.sort_unstable();
        let mut served = vec![false; n_stops];
        let off = csr_offsets(n_stops, sr.iter().map(|e| e.0));
        for e in &sr {
            served[e.0 as usize] = true;
        }
        // Days on which any trip serves the stop (union over its routes):
        // lets the router answer "no service" without searching.
        let mut stop_days = vec![crate::build::NONE; n_stops];
        {
            let words = self.words;
            let mut acc = vec![0u64; words];
            let mut i = 0;
            while i < sr.len() {
                let s = sr[i].0;
                acc.iter_mut().for_each(|w| *w = 0);
                while i < sr.len() && sr[i].0 == s {
                    let b = self.route_days[sr[i].1 as usize] as usize * words;
                    for (w, x) in acc.iter_mut().zip(&self.bitfield_table[b..b + words]) {
                        *w |= x;
                    }
                    i += 1;
                }
                let acc2 = acc.clone();
                stop_days[s as usize] = self.bitfield(&acc2);
            }
        }
        write_all(&dir, "stop_route_off", &off, m)?;
        write_all(
            &dir,
            "stop_route",
            &sr.iter().map(|e| e.1).collect::<Vec<_>>(),
            m,
        )?;
        write_all(
            &dir,
            "stop_route_pos",
            &sr.iter().map(|e| e.2).collect::<Vec<_>>(),
            m,
        )?;
        drop(sr);
        for (s, v) in self.stops.iter_mut().zip(&served) {
            s.served = *v;
        }

        // Footpaths: generated links (served stops within the radius), the
        // feeds' stop-level rules, then a transitive closure capped at
        // `max_footpath_secs`; forbidden pairs are removed.
        // Unserved stops (stations, entrances, stops of other periods) are
        // walkable intermediate points: they take part in linking and the
        // closure, but only footpaths between served stops are written.
        let walkable: Vec<bool> = self.stops.iter().map(|s| s.lat.is_some()).collect();
        let mut generated = Vec::new();
        if self.opts.link_radius_m > 0.0 {
            link_nearby(
                &self.stops,
                &walkable,
                self.opts.link_radius_m,
                self.opts.walk_speed_m_per_s,
                &mut generated,
            );
        }
        // Station links (as nigiri's "metas"): within a feed, stops with the
        // same name ≤ 500 m apart, a parent station and its children ≤ 2 km,
        // and children of the same parent ≤ 500 m are connected by footpaths,
        // whatever the generic radius is (platforms of large stations).
        let station = station_links(&self.stops, &walkable, &self.stop_feed);
        let motis = self.opts.motis_footpaths;
        let speed = self.opts.walk_speed_m_per_s;
        generated.extend(station.iter().map(|&(a, b, d)| {
            let secs = if motis {
                ((d / 1.5) / 60.0).ceil().max(2.0) as u32 * 60
            } else {
                (d / speed).ceil() as u32
            };
            (a, b, secs)
        }));
        let mut explicit = std::mem::take(&mut self.footpaths);
        if self.opts.motis_footpaths {
            generated.retain(|_| false);
            let metas = motis_metas(&self.stops, &walkable, &self.stop_feed, &station);
            generated.extend(
                metas
                    .iter()
                    .map(|&(a, b, d)| (a, b, ((d / 1.5) / 60.0).ceil().max(2.0) as u32 * 60)),
            );
            let (feed, change) = (&self.stop_feed, &self.change);
            let mut near = Vec::new();
            link_nearby(&self.stops, &walkable, 300.0, 1.0, &mut near);
            near.retain_mut(|(a, b, d)| {
                let dist = *d as f64; // speed 1.0 → seconds == metres (rounded up)
                if feed[*a as usize] == feed[*b as usize] {
                    if dist > 100.0 {
                        return false;
                    }
                    *d = (((dist / 1.5) / 60.0).ceil().max(2.0) * 60.0) as u32;
                } else {
                    let walk = ((dist / 1.5) / 60.0).round() * 60.0;
                    let ct = |s: u32| {
                        if change[s as usize] == FORBIDDEN {
                            0
                        } else {
                            change[s as usize]
                        }
                    };
                    *d = (walk as u32).max(ct(*a)).max(ct(*b));
                }
                true
            });
            generated.extend(near);
            for e in explicit.iter_mut() {
                e.2 = e.2 / 60 * 60;
            }
            let have: HashSet<(u32, u32)> = explicit.iter().map(|e| (e.0, e.1)).collect();
            let rev: Vec<_> = explicit
                .iter()
                .filter(|e| !have.contains(&(e.1, e.0)))
                .map(|e| (e.1, e.0, e.2))
                .collect();
            explicit.extend(rev);
        }
        let explicit_pairs: HashSet<(u32, u32)> = explicit.iter().map(|e| (e.0, e.1)).collect();
        let foot = close_footpaths(
            n_stops,
            &served,
            generated,
            explicit,
            &self.forbidden,
            self.opts.max_footpath_secs,
        );
        let foot_off = csr_offsets(n_stops, foot.iter().map(|e| e.0));
        write_all(&dir, "foot_off", &foot_off, m)?;
        write_all(
            &dir,
            "foot_to",
            &foot.iter().map(|e| e.1).collect::<Vec<_>>(),
            m,
        )?;
        write_all(
            &dir,
            "foot_secs",
            &foot
                .iter()
                .map(|e| {
                    if explicit_pairs.contains(&(e.0, e.1)) {
                        e.2 | FOOT_EXPLICIT
                    } else {
                        e.2
                    }
                })
                .collect::<Vec<_>>(),
            m,
        )?;
        let n_footpaths = foot.len();

        // Lower-bound graph, stored reversed (incoming edges per stop).
        let mut lb = std::mem::take(&mut self.lb_edges);
        lb.extend(foot.iter().copied());
        drop(foot);
        let mut lb: Vec<(u32, u32, u32)> = lb.into_iter().map(|(a, b, w)| (b, a, w)).collect();
        lb.sort_unstable();
        lb.dedup_by(|x, y| x.0 == y.0 && x.1 == y.1); // keeps the smallest weight (sorted)
        write_all(
            &dir,
            "lb_in_off",
            &csr_offsets(n_stops, lb.iter().map(|e| e.0)),
            m,
        )?;
        write_all(
            &dir,
            "lb_in_from",
            &lb.iter().map(|e| e.1).collect::<Vec<_>>(),
            m,
        )?;
        write_all(
            &dir,
            "lb_in_secs",
            &lb.iter().map(|e| e.2).collect::<Vec<_>>(),
            m,
        )?;
        drop(lb);

        // Trip/route-specific transfer rules, indexed by boarding stop.
        let mut rules = std::mem::take(&mut self.rules);
        rules.sort_unstable_by_key(|r| (r.to_stop, r.from_stop));
        write_all(
            &dir,
            "rule_off",
            &csr_offsets(n_stops, rules.iter().map(|r| r.to_stop)),
            m,
        )?;
        for (name, f) in [
            (
                "rule_from_stop",
                (|r: &Rule| r.from_stop) as fn(&Rule) -> u32,
            ),
            ("rule_from_trip", |r| r.from_trip),
            ("rule_from_route", |r| r.from_route),
            ("rule_to_trip", |r| r.to_trip),
            ("rule_to_route", |r| r.to_route),
            ("rule_secs", |r| r.secs),
        ] {
            write_all(&dir, name, &rules.iter().map(f).collect::<Vec<_>>(), m)?;
        }

        write_all(&dir, "stop_change", &self.change, m)?;
        write_all(&dir, "stop_tz", &self.stop_tz, m)?;
        write_all(&dir, "route_stop_off", &self.route_stop_off, m)?;
        write_all(&dir, "route_stops", &self.route_stops, m)?;
        write_all(&dir, "route_flags", &self.route_flags, m)?;
        write_all(&dir, "route_trip_off", &self.route_trip_off, m)?;
        write_all(&dir, "route_time_off", &self.route_time_off, m)?;
        write_all(&dir, "route_tz", &self.route_tz, m)?;
        write_all(&dir, "route_info", &self.route_info, m)?;
        write_all(&dir, "route_max_time", &self.route_max_time, m)?;
        write_all(&dir, "route_days", &self.route_days, m)?;
        write_all(&dir, "stop_days", &stop_days, m)?;
        write_all(&dir, "route_info_type", &self.route_info_type, m)?;
        write_all(&dir, "bitfields", &self.bitfield_table, m)?;
        let n_trips = self.trip_bits.len() as usize;
        self.trip_bits.finish(m)?;
        self.trip_gtfs.finish(m)?;
        self.dep.finish(m)?;
        self.arr.finish(m)?;
        self.route_info_id.finish(m)?;
        self.route_info_line.finish(m)?;
        self.route_info_color.finish(m)?;
        self.route_info_agency.finish(m)?;
        self.gtfs_trip_id.finish(m)?;
        self.gtfs_trip_headsign.finish(m)?;
        self.headsigns.finish(m)?;
        let cat_bytes = catalog::write(&self.stops, &dir.join("catalog.bin"))?;
        let _ = cat_bytes;

        let mut warnings = Vec::new();
        let failed: Vec<_> = self
            .feeds
            .iter()
            .filter(|f| !f.imported)
            .map(|f| f.name.clone())
            .collect();
        if !failed.is_empty() {
            warnings.push(format!(
                "{} feed(s) not imported: {}",
                failed.len(),
                failed.join(", ")
            ));
        }
        let empty: Vec<_> = self
            .feeds
            .iter()
            .filter(|f| f.imported && f.trips == 0)
            .map(|f| f.name.clone())
            .collect();
        if !empty.is_empty() {
            warnings.push(format!(
                "{} feed(s) have no trips in the horizon: {}",
                empty.len(),
                empty.join(", ")
            ));
        }
        let flex: u64 = self
            .feeds
            .iter()
            .map(|f| {
                f.skipped
                    .get("flex_or_unknown_stop_trips")
                    .copied()
                    .unwrap_or(0)
            })
            .sum();
        if flex > 0 {
            warnings.push(format!(
                "{flex} GTFS-Flex/unknown-stop trip(s) are not routed"
            ));
        }
        let ignored: u64 = self
            .feeds
            .iter()
            .flat_map(|f| f.transfer_rules.iter())
            .filter(|(k, _)| k.ends_with("_not_exploited"))
            .map(|(_, v)| *v)
            .sum();
        if ignored > 0 {
            warnings.push(format!(
                "{ignored} timed/in-seat/recommended transfer rule(s) are not exploited (results valid, possibly incomplete)"
            ));
        }
        for feed in &self.feeds {
            for (key, count) in &feed.skipped {
                if key.starts_with("netex_") && *count > 0 {
                    warnings.push(format!("{}: {key} ({count})", feed.name));
                }
            }
        }
        let meta = SnapshotMeta {
            version: SNAPSHOT_VERSION,
            rustis_version: env!("CARGO_PKG_VERSION").into(),
            first_day: self.opts.first_day.to_string(),
            horizon_days: self.opts.horizon_days,
            bitfield_words: self.words as u32,
            timezones: self.tz_names,
            n_stops,
            n_routes,
            n_trips,
            n_gtfs_trips: self.n_gtfs_trips as usize,
            n_route_infos: self.route_info_type.len(),
            n_bitfields: self.bitfields.len(),
            n_footpaths,
            n_specific_rules: rules.len(),
            max_time: self.max_time,
            link_radius_m: self.opts.link_radius_m,
            walk_speed_m_per_s: self.opts.walk_speed_m_per_s,
            max_footpath_secs: self.opts.max_footpath_secs,
            default_change_seconds: self.opts.default_change_seconds,
            exclude_booking_stops: self.opts.exclude_booking_stops,
            feeds: self.feeds,
            warnings,
            manifest,
            build_seconds: t0.elapsed().as_secs_f64(),
        };
        Ok(meta)
    }
}

/// Greedy split into routes whose trips never overtake each other, also when
/// trips of different service days are compared (shift by whole days).
fn split_fifo(mut trips: Vec<Trip>) -> Vec<Vec<Trip>> {
    trips.sort_by(|a, b| a.times.cmp(&b.times));
    let long = trips.iter().any(|t| *t.times.last().unwrap() >= 86_400);
    let mut routes: Vec<Vec<Trip>> = Vec::new();
    'trip: for t in trips {
        for r in routes.iter_mut() {
            let ok = if long {
                r.iter().all(|u| fifo_compatible(&u.times, &t.times))
            } else {
                // sorted input + "≤ at every event" is transitive
                r.last()
                    .unwrap()
                    .times
                    .iter()
                    .zip(&t.times)
                    .all(|(a, b)| a <= b)
            };
            if ok {
                r.push(t);
                continue 'trip;
            }
        }
        routes.push(vec![t]);
    }
    routes
}

/// No overtaking for any whole-day shift: the differences `t − u` over all
/// events must not straddle a multiple of 86400 s.
fn fifo_compatible(u: &[u32], t: &[u32]) -> bool {
    let (mut lo, mut hi) = (i64::MAX, i64::MIN);
    for (a, b) in u.iter().zip(t) {
        let d = i64::from(*b) - i64::from(*a);
        lo = lo.min(d);
        hi = hi.max(d);
    }
    // exists k with lo < k·86400 < hi ?
    let k = lo.div_euclid(86_400) + 1;
    !(k * 86_400 < hi && k * 86_400 > lo)
}

pub(crate) fn csr_offsets(n: usize, keys: impl Iterator<Item = u32>) -> Vec<u32> {
    let mut off = vec![0u32; n + 1];
    for k in keys {
        off[k as usize + 1] += 1;
    }
    for i in 0..n {
        off[i + 1] += off[i];
    }
    off
}

pub fn haversine(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (la1, lo1, la2, lo2) = (
        a.0.to_radians(),
        a.1.to_radians(),
        b.0.to_radians(),
        b.1.to_radians(),
    );
    let h = ((la2 - la1) / 2.0).sin().powi(2)
        + la1.cos() * la2.cos() * ((lo2 - lo1) / 2.0).sin().powi(2);
    2.0 * 6_371_000.0 * h.sqrt().asin()
}

/// nigiri's "metas" (comparison mode): within a feed, every stop reachable
/// from `s` over hops of ≤ 100 m, same-name links and parent/child relations
/// is linked to `s` if it is ≤ 500 m away (≤ 2 km for parent/child), with the
/// direct distance. `station` are the same-name/parent/sibling links.
fn motis_metas(
    stops: &[Stop],
    served: &[bool],
    feed: &[u32],
    station: &[(u32, u32, f64)],
) -> Vec<(u32, u32, f64)> {
    let n = stops.len();
    let mut close = Vec::new();
    link_nearby(stops, served, 100.0, 1.0, &mut close);
    let mut adj: Vec<Vec<u32>> = vec![Vec::new(); n];
    for &(a, b, _) in &close {
        if feed[a as usize] == feed[b as usize] {
            adj[a as usize].push(b);
        }
    }
    for &(a, b, _) in station {
        adj[a as usize].push(b);
    }
    let coord = |i: u32| stops[i as usize].lat.zip(stops[i as usize].lon);
    let direct: HashSet<(u32, u32)> = station
        .iter()
        .filter(|e| e.2 > 500.0)
        .map(|e| (e.0, e.1))
        .collect();
    let mut out = Vec::new();
    let mut seen: HashSet<u32> = HashSet::new();
    let mut todo: Vec<u32> = Vec::new();
    for s in 0..n as u32 {
        let Some(cs) = coord(s) else { continue };
        if !served[s as usize] || adj[s as usize].is_empty() {
            continue;
        }
        seen.clear();
        todo.clear();
        seen.insert(s);
        todo.push(s);
        while let Some(u) = todo.pop() {
            for &v in &adj[u as usize] {
                if seen.contains(&v) {
                    continue;
                }
                let Some(cv) = coord(v) else { continue };
                // prune: only nodes within the 500 m result radius (plus a
                // margin for chains) can qualify; far parent/child pairs are
                // added directly below
                if haversine(cs, cv) > 600.0 {
                    continue;
                }
                seen.insert(v);
                todo.push(v);
            }
        }
        for &t in &seen {
            if t == s {
                continue;
            }
            let d = haversine(cs, coord(t).unwrap());
            if d <= 500.0 {
                out.push((s, t, d));
            }
        }
    }
    out.extend(
        station
            .iter()
            .filter(|e| direct.contains(&(e.0, e.1)))
            .copied(),
    );
    out
}

/// Same-feed station links (see the caller): `(from, to, metres)`, both
/// directions, served stops only.
fn station_links(stops: &[Stop], served: &[bool], feed: &[u32]) -> Vec<(u32, u32, f64)> {
    let coord = |i: usize| stops[i].lat.zip(stops[i].lon);
    let mut by_name: HashMap<(u32, &str), Vec<u32>> = HashMap::new();
    let mut id_index: HashMap<&str, u32> = HashMap::new();
    for (i, s) in stops.iter().enumerate() {
        id_index.insert(s.stop_id.as_str(), i as u32);
        if served[i] && coord(i).is_some() && !s.name.is_empty() {
            by_name
                .entry((feed[i], s.name.as_str()))
                .or_default()
                .push(i as u32);
        }
    }
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (i, s) in stops.iter().enumerate() {
        if let Some(p) = s.parent_station.as_deref().and_then(|p| id_index.get(p))
            && coord(i).is_some()
        {
            children.entry(*p).or_default().push(i as u32);
        }
    }
    let mut out: HashSet<(u32, u32)> = HashSet::new();
    let mut res = Vec::new();
    let mut add = |a: u32, b: u32, max: f64, res: &mut Vec<(u32, u32, f64)>| {
        if a == b || !served[a as usize] || !served[b as usize] {
            return;
        }
        let (Some(p), Some(q)) = (coord(a as usize), coord(b as usize)) else {
            return;
        };
        let d = haversine(p, q);
        if d <= max && out.insert((a, b)) {
            res.push((a, b, d));
            if out.insert((b, a)) {
                res.push((b, a, d));
            }
        }
    };
    for v in by_name.values() {
        if v.len() < 2 {
            continue;
        }
        // sort by latitude: only stops within 500 m in latitude can match
        let mut v = v.clone();
        v.sort_by(|&a, &b| {
            coord(a as usize)
                .unwrap()
                .0
                .total_cmp(&coord(b as usize).unwrap().0)
        });
        for i in 0..v.len() {
            let la = coord(v[i] as usize).unwrap().0;
            for &b in &v[i + 1..] {
                if coord(b as usize).unwrap().0 - la > 500.0 / 111_000.0 {
                    break;
                }
                add(v[i], b, 500.0, &mut res);
            }
        }
    }
    for (&p, kids) in &children {
        for (k, &a) in kids.iter().enumerate() {
            if coord(p as usize).is_some() {
                add(p, a, 2000.0, &mut res);
            }
            if kids.len() <= 400 {
                for &b in &kids[k + 1..] {
                    add(a, b, 500.0, &mut res);
                }
            }
        }
    }
    res
}

/// Footpaths between served stops closer than `radius` (grid hashing).
fn link_nearby(
    stops: &[Stop],
    served: &[bool],
    radius: f64,
    speed: f64,
    out: &mut Vec<(u32, u32, u32)>,
) {
    let cell_lat = radius / 111_000.0;
    let mut grid: HashMap<(i32, i32), Vec<u32>> = HashMap::new();
    let key = |lat: f64, lon: f64| {
        let cl = cell_lat / lat.to_radians().cos().max(0.05);
        ((lat / cell_lat).floor() as i32, (lon / cl).floor() as i32)
    };
    for (i, s) in stops.iter().enumerate() {
        if let (true, Some(lat), Some(lon)) = (served[i], s.lat, s.lon) {
            grid.entry(key(lat, lon)).or_default().push(i as u32);
        }
    }
    for (i, s) in stops.iter().enumerate() {
        let (true, Some(lat), Some(lon)) = (served[i], s.lat, s.lon) else {
            continue;
        };
        let (ky, kx) = key(lat, lon);
        // neighbouring cells; longitude cell width differs slightly between rows
        for dy in -1..=1 {
            for dx in -2..=2 {
                let Some(v) = grid.get(&(ky + dy, kx + dx)) else {
                    continue;
                };
                for &j in v {
                    if j as usize == i {
                        continue;
                    }
                    let t = &stops[j as usize];
                    let d = haversine((lat, lon), (t.lat.unwrap(), t.lon.unwrap()));
                    if d <= radius {
                        out.push((i as u32, j, (d / speed).ceil() as u32));
                    }
                }
            }
        }
    }
}

/// Transitive closure of all footpaths (generated links and transfers.txt
/// pairs) up to `cap` seconds (Dijkstra from every stop). A pair with its own
/// transfers.txt row keeps exactly that duration (strictest row wins) even if
/// a chain would be shorter; forbidden pairs (type 3) are removed.
fn close_footpaths(
    n: usize,
    served: &[bool],
    generated: Vec<(u32, u32, u32)>,
    explicit: Vec<(u32, u32, u32)>,
    forbidden: &HashSet<(u32, u32)>,
    cap: u32,
) -> Vec<(u32, u32, u32)> {
    let mut fixed: HashMap<(u32, u32), u32> = HashMap::new();
    for (a, b, w) in explicit {
        let e = fixed.entry((a, b)).or_insert(0);
        *e = (*e).max(w);
    }
    let mut edges: Vec<(u32, u32, u32)> = generated
        .into_iter()
        .filter(|e| !fixed.contains_key(&(e.0, e.1)))
        .chain(fixed.iter().map(|(&(a, b), &w)| (a, b, w)))
        .filter(|e| e.0 != e.1 && !forbidden.contains(&(e.0, e.1)))
        .collect();
    edges.sort_unstable();
    edges.dedup_by(|x, y| x.0 == y.0 && x.1 == y.1);
    let off = csr_offsets(n, edges.iter().map(|e| e.0));
    let mut out = Vec::with_capacity(edges.len() + fixed.len());
    let mut explicit_from: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
    for (&(a, b), &w) in &fixed {
        if a != b && !forbidden.contains(&(a, b)) {
            explicit_from.entry(a).or_default().push((b, w));
        }
    }
    let mut dist: HashMap<u32, u32> = HashMap::new();
    let mut heap = std::collections::BinaryHeap::new();
    for s in 0..n {
        let (a, b) = (off[s] as usize, off[s + 1] as usize);
        let ex = explicit_from.get(&(s as u32));
        if a == b && ex.is_none() {
            continue;
        }
        dist.clear();
        heap.clear();
        heap.push(std::cmp::Reverse((0u32, s as u32)));
        dist.insert(s as u32, 0);
        while let Some(std::cmp::Reverse((d, u))) = heap.pop() {
            if dist.get(&u).is_some_and(|&x| x < d) {
                continue;
            }
            let (a, b) = (off[u as usize] as usize, off[u as usize + 1] as usize);
            for e in &edges[a..b] {
                let nd = d + e.2;
                if (nd <= cap || u as usize == s) && dist.get(&e.1).is_none_or(|&x| nd < x) {
                    dist.insert(e.1, nd);
                    heap.push(std::cmp::Reverse((nd, e.1)));
                }
            }
        }
        for &(t, w) in ex.into_iter().flatten() {
            dist.insert(t, w); // explicit pair rule replaces the generated time
        }
        let mut v: Vec<(u32, u32, u32)> = dist
            .iter()
            // a footpath between two unserved stops is never used (walks are
            // not chained; endpoints of a walk are a source, a target or a
            // stop with service)
            .filter(|&(&t, _)| {
                t as usize != s
                    && (served[s] || served[t as usize])
                    && !forbidden.contains(&(s as u32, t))
            })
            .map(|(&t, &d)| (s as u32, t, d))
            .collect();
        v.sort_unstable();
        out.extend(v);
    }
    out
}
