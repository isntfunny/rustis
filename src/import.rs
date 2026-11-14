//! Offline GTFS import into an immutable snapshot directory.
//!
//! Calendar evaluation, pattern grouping and non-overtaking route splitting
//! are delegated to Vulture's `GtfsTimetable`; this module only checks the
//! feed against the supported subset and persists the prepared timetable.
use crate::format::{
    FLAG_DROP_OFF_BOOKING, FLAG_NO_DROP_OFF, FLAG_NO_PICKUP, FLAG_PICKUP_BOOKING, Sec, Writer,
};
use crate::model::{METADATA_VERSION, Metadata, RouteInfo, Stop};
use anyhow::{Context, Result, bail, ensure};
use gtfs_structures::Gtfs;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use vulture::gtfs::{GtfsTimetable, OvernightDays};
use vulture::{RouteIdx, StopIdx, Timetable, TripIdx};

#[derive(Clone, Debug)]
pub struct ImportOptions {
    /// Service date (`YYYY-MM-DD`). The snapshot also contains the next day.
    pub date: String,
    /// Add straight-line walking footpaths between stops within this radius.
    pub walk_radius_m: Option<f64>,
    pub walk_speed_m_per_s: f64,
    /// Drop transfer rows whose omission provably only removes connections
    /// (timed/in-seat/recommended, incl. trip/route-specific ones). Rules that
    /// restrict transfers (specific min-time/forbidden) are always rejected.
    pub drop_relaxing_transfers: bool,
    /// Apply trip/route-specific minimum-time (type 2) and forbidden (type 3)
    /// rules to every trip at those stops: only ever tightens existing times or
    /// removes footpaths, never creates one.
    pub generalize_restricting_transfers: bool,
    /// Minimum change time at a stop without its own same-stop rule.
    pub default_change_seconds: u32,
    /// Treat pickup/drop_off type 2/3 (booking/arrangement) as not available
    /// instead of routing them with a `booking_required` mark.
    pub exclude_booking_stops: bool,
    /// Stream-filter the feed to the snapshot days before parsing (lower
    /// import RAM). Disable only for comparison.
    pub prefilter: bool,
    /// Remove GTFS-Flex trips (stop_times without stop_id / with pickup
    /// windows) instead of failing; snapshot is marked incomplete.
    pub exclude_flex_trips: bool,
    /// Remove frequency-based trips (frequencies.txt) instead of failing.
    pub exclude_frequency_trips: bool,
}

impl ImportOptions {
    pub fn for_date(date: &str) -> Self {
        Self {
            date: date.into(),
            walk_radius_m: None,
            walk_speed_m_per_s: 1.2,
            drop_relaxing_transfers: false,
            generalize_restricting_transfers: false,
            default_change_seconds: 0,
            exclude_booking_stops: false,
            prefilter: true,
            exclude_flex_trips: false,
            exclude_frequency_trips: false,
        }
    }
}

#[derive(Debug)]
pub struct ImportReport {
    pub stops: usize,
    pub raptor_routes: usize,
    pub trips: usize,
    pub timetable_bytes: u64,
    pub metadata_bytes: u64,
    pub notes: Vec<String>,
}

/// Convenience wrapper with default options.
pub fn import_gtfs(input: &Path, dest: &Path, date: &str) -> Result<ImportReport> {
    import_gtfs_with(input, dest, &ImportOptions::for_date(date))
}

/// Imports `input` (GTFS directory or ZIP) into the new directory `dest`.
/// The snapshot is assembled in a sibling temporary directory and renamed
/// into place only when complete; an existing `dest` is never touched.
pub fn import_gtfs_with(input: &Path, dest: &Path, opts: &ImportOptions) -> Result<ImportReport> {
    ensure!(
        !dest.exists(),
        "destination {} already exists; snapshots are immutable",
        dest.display()
    );
    if !opts.prefilter {
        return import_prepared(input, input, dest, opts, Vec::new(), BTreeMap::new());
    }
    let day: chrono::NaiveDate = chrono::NaiveDate::parse_from_str(&opts.date, "%Y-%m-%d")
        .context("date must be YYYY-MM-DD")?;
    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = dest
        .file_name()
        .context("destination needs a final path component")?;
    let work = parent.join(format!(
        ".{}.prefilter-{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    let result = crate::prefilter::prefilter(input, &work, &[day, day + chrono::Days::new(1)], opts.exclude_flex_trips,
        opts.exclude_frequency_trips,
    )
        .and_then(|st| {
            phase(&format!("prefilter done ({} stop_times kept)", st.stop_times_kept));
            let note = format!(
                "prefilter: kept {}/{} trips, {}/{} stop_times active on {} or the next day, all stops kept; shapes dropped",
                st.trips_kept, st.trips_total, st.stop_times_kept, st.stop_times_total, opts.date
            );
            let mut notes = vec![note];
            let mut excluded = BTreeMap::new();
            if st.flex_trips_removed > 0 {
                excluded.insert("GTFS-Flex trips".to_owned(), st.flex_trips_removed as usize);
            }
            if st.frequency_trips_removed > 0 {
                excluded.insert("frequency-based trips".to_owned(), st.frequency_trips_removed as usize);
            }
            if !excluded.is_empty() {
                notes.push(format!("EXCLUDED: {excluded:?}"));
            }
            import_prepared(&work, input, dest, opts, notes, excluded)
        });
    let _ = fs::remove_dir_all(&work);
    result
}

/// Import from `input` (possibly the pre-filtered copy of `source`).
fn import_prepared(
    input: &Path,
    source: &Path,
    dest: &Path,
    opts: &ImportOptions,
    pre_notes: Vec<String>,
    excluded: BTreeMap<String, usize>,
) -> Result<ImportReport> {
    ensure!(
        !dest.exists(),
        "destination {} already exists; snapshots are immutable",
        dest.display()
    );
    let service_date: jiff::civil::Date = opts.date.parse().context("date must be YYYY-MM-DD")?;
    if let Some(r) = opts.walk_radius_m {
        ensure!(
            r.is_finite() && r > 0.0 && r <= 2000.0,
            "walk radius must be in (0, 2000] metres"
        );
        ensure!(
            opts.walk_speed_m_per_s.is_finite() && opts.walk_speed_m_per_s > 0.1,
            "walk speed must be > 0.1 m/s"
        );
    }
    let mut rules = read_transfer_rules(
        input,
        opts.drop_relaxing_transfers,
        opts.generalize_restricting_transfers,
    )?;
    rules.excluded_trips = excluded;
    let mut notes = pre_notes;
    notes.extend(rules.notes.clone());

    let mut gtfs = Gtfs::new(input.to_str().context("GTFS path must be UTF-8")?)
        .with_context(|| format!("reading GTFS {}", input.display()))?;
    phase("gtfs parsed");
    apply_transfer_rules(&mut gtfs, &rules)?;
    let timezone = single_timezone(&gtfs)?;
    reject_dst_window(&timezone, &opts.date)?;
    for t in gtfs.trips.values() {
        ensure!(
            t.frequencies.is_empty(),
            "frequency-based trips are not supported (trip {})",
            t.id
        );
    }

    let mut tt = GtfsTimetable::new_with_overnight_days(&gtfs, service_date, OvernightDays(1))
        .context("preparing timetable with Vulture")?;
    if let Some(radius) = opts.walk_radius_m {
        tt = tt.with_walking_footpaths(&gtfs, radius, opts.walk_speed_m_per_s);
        notes.push(format!(
            "added straight-line walking footpaths within {radius} m at {} m/s (not chained)",
            opts.walk_speed_m_per_s
        ));
    }
    ensure!(
        tt.n_trips() > 0,
        "no trips are active on {} or the following day",
        opts.date
    );
    ensure!(
        tt.n_stops() < u32::MAX as usize
            && tt.n_routes() < u32::MAX as usize
            && tt.n_trips() < u32::MAX as usize,
        "dataset exceeds format limits"
    );

    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = dest
        .file_name()
        .context("destination needs a final path component")?;
    let staging: PathBuf = parent.join(format!(
        ".{}.importing-{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    phase("vulture timetable built");
    let ex = extract(&gtfs, &tt, timezone.clone());
    drop(gtfs);
    phase("gtfs graph released");
    fs::create_dir(&staging).with_context(|| format!("creating {}", staging.display()))?;
    let result = (|| {
        let report = write_snapshot(
            &ex,
            &tt,
            &staging,
            opts,
            &source.display().to_string(),
            notes,
            &rules,
        )?;
        ensure!(
            !dest.exists(),
            "destination {} appeared during import",
            dest.display()
        );
        fs::rename(&staging, dest).context("publishing snapshot")?;
        File::open(parent)?.sync_all()?;
        Ok(report)
    })();
    if result.is_err() {
        // Only the private staging directory is removed, never `dest`.
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

/// Import progress with current/peak RSS on stderr (import memory is the
/// limiting resource for large feeds).
fn phase(name: &str) {
    if let Some(m) = crate::memory::ProcessMemory::current() {
        eprintln!(
            "phase {name}: rss {} MiB, peak {} MiB",
            m.rss_kib / 1024,
            m.hwm_kib / 1024
        );
    }
}

/// `(allowed, booking_required)` for a GTFS pickup/drop-off type. Unknown
/// values are treated as not available (only removes options).
fn classify(code: u8, stats: &mut (usize, usize)) -> (bool, bool) {
    match code {
        0 => (true, false),
        1 => (false, false),
        2 | 3 => {
            stats.0 += 1;
            (true, true)
        }
        _ => {
            stats.1 += 1;
            (false, false)
        }
    }
}

fn pickup_code(t: &gtfs_structures::PickupDropOffType) -> u8 {
    use gtfs_structures::PickupDropOffType as P;
    match t {
        P::Regular => 0,
        P::NotAvailable => 1,
        P::ArrangeByPhone => 2,
        P::CoordinateWithDriver => 3,
        P::Unknown(_) => u8::MAX,
    }
}

/// Everything the export needs from the GTFS object graph, so the graph (the
/// largest import allocation) can be freed before the export arrays exist.
struct Extract {
    stops: Vec<Stop>,
    routes: HashMap<String, RouteInfo>,
    /// GTFS trip id → `[pickup code, drop-off code]` per stop time.
    pickup: HashMap<String, Vec<[u8; 2]>>,
    timezone: String,
}

fn extract(gtfs: &Gtfs, tt: &GtfsTimetable, timezone: String) -> Extract {
    let stops = (0..tt.n_stops())
        .map(|i| {
            let id = tt.stop_id(StopIdx::new(i as u32));
            let s = &gtfs.stops[id];
            Stop {
                stop_id: id.into(),
                name: s.name.clone().unwrap_or_else(|| id.into()),
                lat: s.latitude,
                lon: s.longitude,
                parent_station: s.parent_station.clone(),
            }
        })
        .collect();
    let mut routes = HashMap::new();
    for r in 0..tt.n_routes() {
        let id = tt.route_id(RouteIdx::new(r as u32));
        routes.entry(id.to_owned()).or_insert_with(|| {
            let raw = &gtfs.routes[id];
            RouteInfo {
                route_id: id.to_owned(),
                line: raw
                    .short_name
                    .clone()
                    .filter(|s| !s.is_empty())
                    .or_else(|| raw.long_name.clone())
                    .unwrap_or_else(|| id.to_owned()),
                mode: format!("{:?}", raw.route_type),
            }
        });
    }
    let mut pickup = HashMap::new();
    for t in 0..tt.n_trips() {
        let id = tt.trip_id(TripIdx::new(t as u32));
        pickup.entry(id.to_owned()).or_insert_with(|| {
            gtfs.trips[id]
                .stop_times
                .iter()
                .map(|st| [pickup_code(&st.pickup_type), pickup_code(&st.drop_off_type)])
                .collect()
        });
    }
    Extract {
        stops,
        routes,
        pickup,
        timezone,
    }
}

/// Vulture's default for a recommended transfer without a time.
const DEFAULT_FOOTPATH_SECONDS: u32 = 300;

/// Transfer rules as Rustis enforces them.
pub(crate) struct TransferRules {
    /// Distinct-stop footpaths with the strictest (largest) time per pair.
    footpaths: BTreeMap<(String, String), u32>,
    /// Same-stop minimum change times (largest per stop).
    same_stop: BTreeMap<String, u32>,
    /// Generalised trip/route-specific same-stop times: lower bound on the
    /// final change time (after rule/default resolution).
    same_stop_floor: BTreeMap<String, u32>,
    /// Distinct-stop `transfer_type 3` pairs.
    impossible: BTreeSet<(String, String)>,
    /// Dropped relaxing rule classes (only with `drop_relaxing_transfers`).
    pub(crate) dropped: BTreeMap<String, usize>,
    /// Trips removed before routing (e.g. GTFS-Flex).
    pub(crate) excluded_trips: BTreeMap<String, usize>,
    notes: Vec<String>,
}

/// `gtfs-structures` keeps transfer rows but drops their trip/route columns,
/// and Vulture turns every non-impossible row (incl. timed and in-seat rules)
/// into a stop-to-stop footpath with a 300 s default and never enforces
/// same-stop times. Rustis therefore parses `transfers.txt` itself and decides
/// per rule class:
///
/// * stop-level type 0/2 between distinct stops → footpath (strictest time);
/// * stop-level type 2 at one stop → minimum change time, enforced by the
///   timetable adapter (see `storage.rs`);
/// * stop-level type 3 between distinct stops → no footpath (also not from
///   `--walk-radius-m`; chained circumvention is rejected after export);
/// * everything the adapter cannot enforce fails the import. With
///   `drop_relaxing` only classes whose omission can merely *remove*
///   connections are dropped: types 0/1/4/5 (recommended, timed, in-seat
///   allowed/forbidden – Rustis never models in-seat continuation). Restricting
///   classes (trip/route-specific 2/3, same-stop 3) are always rejected because
///   dropping them could admit transfers the feed forbids.
fn read_transfer_rules(
    input: &Path,
    drop_relaxing: bool,
    generalize: bool,
) -> Result<TransferRules> {
    let mut rules = TransferRules {
        footpaths: BTreeMap::new(),
        same_stop: BTreeMap::new(),
        same_stop_floor: BTreeMap::new(),
        impossible: BTreeSet::new(),
        dropped: BTreeMap::new(),
        excluded_trips: BTreeMap::new(),
        notes: Vec::new(),
    };
    let Some(raw) = read_optional_member(input, "transfers.txt")? else {
        rules
            .notes
            .push("feed has no transfers.txt: no footpaths between distinct stops".into());
        return Ok(rules);
    };
    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(raw.as_slice());
    let headers: Vec<String> = rdr
        .headers()?
        .iter()
        .map(|h| h.trim().trim_start_matches('\u{feff}').to_owned())
        .collect();
    let col = |n: &str| headers.iter().position(|h| h == n);
    let specific: Vec<usize> = ["from_trip_id", "to_trip_id", "from_route_id", "to_route_id"]
        .iter()
        .filter_map(|c| col(c))
        .collect();
    let (ty, from, to, min) = (
        col("transfer_type"),
        col("from_stop_id"),
        col("to_stop_id"),
        col("min_transfer_time"),
    );
    let mut rows = 0usize;
    let mut specific_same: BTreeMap<String, u32> = BTreeMap::new();
    let mut specific_pair: BTreeMap<(String, String), u32> = BTreeMap::new();
    let mut specific_forbidden: BTreeSet<(String, String)> = BTreeSet::new();
    let mut generalised: BTreeMap<String, usize> = BTreeMap::new();
    let mut drop = |class: String, line: usize| -> Result<()> {
        ensure!(
            drop_relaxing,
            "transfers.txt line {line}: {class} transfers are not supported \
             (--drop-relaxing-transfers drops this class; results may then miss connections)"
        );
        *rules.dropped.entry(class).or_default() += 1;
        Ok(())
    };
    for (line, rec) in rdr.records().enumerate() {
        let line = line + 2;
        let rec = rec.with_context(|| format!("transfers.txt line {line}"))?;
        rows += 1;
        let field = |i: Option<usize>| i.and_then(|i| rec.get(i)).map(str::trim).unwrap_or("");
        let (f, t, kind) = (field(from), field(to), field(ty));
        let kind = if kind.is_empty() { "0" } else { kind };
        ensure!(
            matches!(kind, "0" | "1" | "2" | "3" | "4" | "5"),
            "transfers.txt line {line}: unknown transfer_type {kind}"
        );
        if specific.iter().any(|&i| !field(Some(i)).is_empty()) {
            if matches!(kind, "2" | "3") {
                ensure!(
                    generalize && !(kind == "3" && f == t),
                    "transfers.txt line {line}: trip/route-specific transfer_type {kind} restricts transfers \
                     and cannot be enforced; not supported{}",
                    if kind == "3" && f == t {
                        ""
                    } else {
                        " (--generalize-restricting-transfers applies it to all trips)"
                    }
                );
                ensure!(
                    !f.is_empty() && !t.is_empty(),
                    "transfers.txt line {line}: stop ids required"
                );
                if kind == "3" {
                    specific_forbidden.insert((f.to_owned(), t.to_owned()));
                } else {
                    let m: u32 = field(min).parse().with_context(|| {
                        format!(
                            "transfers.txt line {line}: transfer_type 2 requires min_transfer_time"
                        )
                    })?;
                    let slot = if f == t {
                        specific_same.entry(f.to_owned()).or_default()
                    } else {
                        specific_pair
                            .entry((f.to_owned(), t.to_owned()))
                            .or_default()
                    };
                    *slot = (*slot).max(m);
                }
                *generalised
                    .entry(format!(
                        "trip/route-specific type {kind} (generalised to all trips)"
                    ))
                    .or_default() += 1;
                continue;
            }
            drop(format!("trip/route-specific type {kind}"), line)?;
            continue;
        }
        match kind {
            "1" | "4" | "5" => {
                drop(format!("stop-level type {kind}"), line)?;
                continue;
            }
            _ => {}
        }
        ensure!(
            !f.is_empty() && !t.is_empty(),
            "transfers.txt line {line}: stop ids required"
        );
        let min_time = match field(min) {
            "" => None,
            v => Some(
                v.parse::<u32>()
                    .with_context(|| format!("transfers.txt line {line}: min_transfer_time"))?,
            ),
        };
        match (kind, f == t) {
            ("3", true) => bail!(
                "transfers.txt line {line}: same-stop transfer_type 3 at {f} cannot be enforced; not supported"
            ),
            ("3", false) => {
                rules.impossible.insert((f.to_owned(), t.to_owned()));
            }
            ("2", same) => {
                let m = min_time.with_context(|| {
                    format!("transfers.txt line {line}: transfer_type 2 requires min_transfer_time")
                })?;
                let slot = if same {
                    rules.same_stop.entry(f.to_owned()).or_default()
                } else {
                    rules
                        .footpaths
                        .entry((f.to_owned(), t.to_owned()))
                        .or_default()
                };
                *slot = (*slot).max(m);
            }
            // Recommended at one stop imposes nothing.
            (_, true) => {}
            (_, false) => {
                let m = min_time.unwrap_or(DEFAULT_FOOTPATH_SECONDS);
                let slot = rules
                    .footpaths
                    .entry((f.to_owned(), t.to_owned()))
                    .or_default();
                *slot = (*slot).max(m);
            }
        }
    }
    rules.dropped.extend(generalised);
    // Generalised specific rules only tighten what stop-level rules allow.
    let mut not_applied = 0usize;
    for (pair, m) in specific_pair {
        match rules.footpaths.get_mut(&pair) {
            Some(x) => *x = (*x).max(m),
            None => not_applied += 1,
        }
    }
    for pair in specific_forbidden {
        if rules.footpaths.remove(&pair).is_none() {
            not_applied += 1;
        }
    }
    rules.same_stop_floor = specific_same;
    if generalize {
        rules.notes.push(format!(
            "generalised {} specific same-stop times; {not_applied} specific walk rules without a stop-level footpath create none",
            rules.same_stop_floor.len()
        ));
    }
    // A forbidden pair wins over a footpath for the same pair.
    let conflicting = rules
        .impossible
        .iter()
        .filter(|p| rules.footpaths.remove(*p).is_some())
        .count();
    rules.notes.push(format!(
        "transfers.txt: {rows} rows; {} distinct-stop footpaths, {} same-stop minimum change times, \
         {} forbidden pairs ({conflicting} overriding a footpath row)",
        rules.footpaths.len(),
        rules.same_stop.len(),
        rules.impossible.len()
    ));
    if !rules.dropped.is_empty() {
        rules.notes.push(format!(
            "CONSERVATIVE: transfer rules not modelled exactly {:?} (relaxing dropped / restricting generalised); connections are missing",
            rules.dropped
        ));
    }
    Ok(rules)
}

/// Replaces the transfer lists `gtfs-structures` attached to stops with the
/// distinct-stop footpaths Rustis accepted (as explicit min-time edges).
fn apply_transfer_rules(gtfs: &mut Gtfs, rules: &TransferRules) -> Result<()> {
    for stop in gtfs.stops.values_mut() {
        if !stop.transfers.is_empty() {
            std::sync::Arc::make_mut(stop).transfers.clear();
        }
    }
    for id in rules
        .same_stop
        .keys()
        .chain(rules.impossible.iter().flat_map(|(a, b)| [a, b]))
    {
        ensure!(
            gtfs.stops.contains_key(id),
            "transfers.txt references unknown stop {id}"
        );
    }
    for ((f, t), secs) in &rules.footpaths {
        ensure!(
            gtfs.stops.contains_key(t),
            "transfers.txt references unknown stop {t}"
        );
        let stop = gtfs
            .stops
            .get_mut(f)
            .with_context(|| format!("transfers.txt references unknown stop {f}"))?;
        std::sync::Arc::make_mut(stop)
            .transfers
            .push(gtfs_structures::StopTransfer {
                to_stop_id: t.clone(),
                transfer_type: gtfs_structures::TransferType::MinTime,
                min_transfer_time: Some(*secs),
            });
    }
    Ok(())
}

/// Rejects forbidden pairs that the (non-closed) footpath search could still
/// connect by chaining permitted footpaths.
fn reject_chained_impossible(
    tt: &GtfsTimetable,
    adjacency: &[Vec<u32>],
    impossible: &BTreeSet<(String, String)>,
) -> Result<()> {
    if tt.footpaths_are_transitively_closed() {
        return Ok(());
    }
    for (a, b) in impossible {
        let (Some(a), Some(b)) = (tt.stop_idx(a), tt.stop_idx(b)) else {
            continue;
        };
        let mut seen = vec![false; adjacency.len()];
        let mut stack = vec![(a.get(), None::<u32>)];
        while let Some((s, via)) = stack.pop() {
            if std::mem::replace(&mut seen[s as usize], true) {
                continue;
            }
            if s == b.get() {
                let via = via
                    .map(|v| tt.stop_id(StopIdx::new(v)).to_owned())
                    .unwrap_or_default();
                bail!(
                    "transfer_type 3 {} -> {} is reachable via chained footpaths (through {via}); \
                     the router cannot enforce it",
                    tt.stop_id(a),
                    tt.stop_id(b)
                );
            }
            for &n in &adjacency[s as usize] {
                stack.push((n, Some(via.unwrap_or(n))));
            }
        }
    }
    Ok(())
}

fn read_optional_member(input: &Path, name: &str) -> Result<Option<Vec<u8>>> {
    if input.is_dir() {
        let p = input.join(name);
        return if p.exists() {
            Ok(Some(fs::read(p)?))
        } else {
            Ok(None)
        };
    }
    let mut zip = zip::ZipArchive::new(File::open(input)?).context("opening GTFS zip")?;
    let idx = (0..zip.len()).find(|&i| {
        zip.name_for_index(i)
            .is_some_and(|n| n == name || n.rsplit('/').next() == Some(name))
    });
    let Some(i) = idx else { return Ok(None) };
    let mut buf = Vec::new();
    zip.by_index(i)?.read_to_end(&mut buf)?;
    Ok(Some(buf))
}

fn single_timezone(gtfs: &Gtfs) -> Result<String> {
    let zones: BTreeSet<_> = gtfs.agencies.iter().map(|a| a.timezone.clone()).collect();
    ensure!(
        zones.len() == 1,
        "exactly one agency timezone is supported, found {zones:?}"
    );
    let tz = zones.into_iter().next().expect("one zone");
    tz.parse::<chrono_tz::Tz>()
        .map_err(|e| anyhow::anyhow!("invalid agency timezone {tz}: {e}"))?;
    if let Some(s) = gtfs.stops.values().find(|s| {
        s.timezone
            .as_deref()
            .is_some_and(|z| !z.is_empty() && z != tz)
    }) {
        bail!(
            "stop {} uses timezone {:?}; mixed timezones are not supported",
            s.id,
            s.timezone
        );
    }
    Ok(tz)
}

/// Snapshot times are seconds after local midnight of the service date. Across
/// a DST change that is not a wall-clock time, so such dates fail closed.
fn reject_dst_window(tz: &str, date: &str) -> Result<()> {
    use chrono::{Offset, TimeZone};
    let tz: chrono_tz::Tz = tz.parse().map_err(|e| anyhow::anyhow!("{e}"))?;
    let base = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")?;
    let mut offsets = BTreeSet::new();
    for day in 0..=3 {
        let d = base + chrono::Days::new(day);
        for h in [0, 12] {
            let t = tz
                .from_local_datetime(&d.and_hms_opt(h, 30, 0).expect("valid time"))
                .earliest()
                .context("nonexistent local time")?;
            offsets.insert(t.offset().fix().local_minus_utc());
        }
    }
    ensure!(
        offsets.len() == 1,
        "a DST transition falls within {date} + 2 days; not yet supported"
    );
    Ok(())
}

fn write_snapshot(
    ex: &Extract,
    tt: &GtfsTimetable,
    dir: &Path,
    opts: &ImportOptions,
    source: &str,
    notes: Vec<String>,
    rules: &TransferRules,
) -> Result<ImportReport> {
    let impossible = &rules.impossible;
    let (n_stops, n_routes, n_trips) = (tt.n_stops(), tt.n_routes(), tt.n_trips());
    let stops: Vec<Stop> = ex.stops.clone();

    let mut routes: Vec<RouteInfo> = Vec::new();
    let mut route_index: HashMap<String, u32> = HashMap::new();
    let mut w = Writer::default();
    let (mut route_stop_offsets, mut route_stops) = (vec![0u32], Vec::new());
    let (mut route_trip_offsets, mut route_cell_offsets, mut route_gtfs) =
        (vec![0u32], vec![0u64], Vec::new());
    let mut trip_route: Vec<u32> = Vec::with_capacity(n_trips);
    // Vulture trip of each exported trip, in exported (route-contiguous) order.
    let mut trip_order: Vec<TripIdx> = Vec::with_capacity(n_trips);
    // (booking cells, unknown-value cells)
    let mut booking_stats = (0usize, 0usize);
    let (mut arrivals, mut departures, mut flags) = (Vec::new(), Vec::new(), Vec::new());
    for r in 0..n_routes {
        let rid = RouteIdx::new(r as u32);
        let gtfs_id = tt.route_id(rid);
        let gi = *route_index.entry(gtfs_id.to_owned()).or_insert_with(|| {
            routes.push(ex.routes[gtfs_id].clone());
            routes.len() as u32 - 1
        });
        let ss = tt.get_stops_after(rid, 0);
        let n_pos = ss.len();
        // Per-trip flags for every stop position.
        let mut trip_flags: Vec<(TripIdx, Vec<u8>)> = Vec::new();
        for &t in tt.trips_on_route(rid) {
            let raw = &ex.pickup[tt.trip_id(t)];
            let fl = (0..n_pos as u32)
                .map(|p| {
                    let [pc, dc] = raw[p as usize];
                    let (pick, pick_book) = classify(pc, &mut booking_stats);
                    let (drop, drop_book) = classify(dc, &mut booking_stats);
                    let excl = opts.exclude_booking_stops;
                    let mut f = 0u8;
                    if !tt.pickup_allowed(t, p) || !pick || (pick_book && excl) {
                        f |= FLAG_NO_PICKUP;
                    }
                    if !tt.drop_off_allowed(t, p) || !drop || (drop_book && excl) {
                        f |= FLAG_NO_DROP_OFF;
                    }
                    if pick_book && !excl {
                        f |= FLAG_PICKUP_BOOKING;
                    }
                    if drop_book && !excl {
                        f |= FLAG_DROP_OFF_BOOKING;
                    }
                    f
                })
                .collect();
            trip_flags.push((t, fl));
        }
        // RAPTOR (Vulture) only scans the earliest catchable trip per route and
        // never a later one whose drop-off/pickup pattern differs. Splitting each
        // (already non-overtaking) route by its pickup/drop-off pattern makes the
        // earliest catchable trip dominant again. Order within a group is kept.
        let mut groups: Vec<(Vec<u8>, Vec<usize>)> = Vec::new();
        for (k, (_, fl)) in trip_flags.iter().enumerate() {
            let key: Vec<u8> = fl
                .iter()
                .map(|f| f & (FLAG_NO_PICKUP | FLAG_NO_DROP_OFF))
                .collect();
            match groups.iter_mut().find(|(g, _)| *g == key) {
                Some((_, v)) => v.push(k),
                None => groups.push((key, vec![k])),
            }
        }
        for (_, members) in groups {
            let new_route = route_gtfs.len() as u32;
            route_gtfs.push(gi);
            route_stops.extend(ss.iter().map(|s| s.get()));
            route_stop_offsets.push(route_stops.len() as u32);
            for &k in &members {
                trip_order.push(trip_flags[k].0);
                trip_route.push(new_route);
            }
            route_trip_offsets.push(trip_order.len() as u32);
            // Cells are stored [stop position][trip position], like Vulture.
            for p in 0..n_pos {
                for &k in &members {
                    let t = trip_flags[k].0;
                    arrivals.push(tt.get_arrival_time(t, p as u32).0);
                    departures.push(tt.get_departure_time(t, p as u32).0);
                    flags.push(trip_flags[k].1[p]);
                }
            }
            route_cell_offsets.push(arrivals.len() as u64);
        }
    }
    let n_export_routes = route_gtfs.len();
    ensure!(
        trip_order.len() == n_trips && {
            let mut seen = vec![false; n_trips];
            trip_order
                .iter()
                .all(|t| !std::mem::replace(&mut seen[t.get() as usize], true))
        },
        "importer did not export every prepared trip exactly once"
    );

    // Routes serving each stop (first position per route, like Vulture).
    let mut serving: Vec<Vec<(u32, u32)>> = vec![Vec::new(); n_stops];
    for r in 0..n_export_routes {
        let (a, b) = (
            route_stop_offsets[r] as usize,
            route_stop_offsets[r + 1] as usize,
        );
        for (p, &s) in route_stops[a..b].iter().enumerate() {
            let entry = &mut serving[s as usize];
            if !entry.iter().any(|&(x, _)| x == r as u32) {
                entry.push((r as u32, p as u32));
            }
        }
    }
    let (mut stop_route_offsets, mut stop_routes) = (vec![0u32], Vec::new());
    let (mut foot_offsets, mut foot_to, mut foot_secs) = (vec![0u32], Vec::new(), Vec::new());
    for (s, served) in serving.iter().enumerate() {
        let sid = StopIdx::new(s as u32);
        for &(r, p) in served {
            stop_routes.extend([r, p]);
        }
        stop_route_offsets.push((stop_routes.len() / 2) as u32);
        let from_id = tt.stop_id(sid);
        for &to in tt.get_footpaths_from(sid) {
            // Self-loops are no-ops for RAPTOR; walking edges generated by
            // `with_walking_footpaths` must not override transfer_type 3.
            if to == sid || impossible.contains(&(from_id.to_owned(), tt.stop_id(to).to_owned())) {
                continue;
            }
            foot_to.push(to.get());
            foot_secs.push(tt.get_transfer_time(sid, to).0);
        }
        foot_offsets.push(foot_to.len() as u32);
    }

    let mut id_offsets = vec![0u64];
    let mut id_bytes = Vec::new();
    for &t in &trip_order {
        id_bytes.extend_from_slice(tt.trip_id(t).as_bytes());
        id_offsets.push(id_bytes.len() as u64);
    }
    let footpaths = foot_to.len();
    let adjacency: Vec<Vec<u32>> = foot_offsets
        .windows(2)
        .map(|w| foot_to[w[0] as usize..w[1] as usize].to_vec())
        .collect();
    reject_chained_impossible(tt, &adjacency, impossible)?;
    let change_secs: Vec<u32> = (0..n_stops)
        .map(|s| {
            let id = tt.stop_id(StopIdx::new(s as u32));
            rules
                .same_stop
                .get(id)
                .copied()
                .unwrap_or(opts.default_change_seconds)
                .max(rules.same_stop_floor.get(id).copied().unwrap_or(0))
        })
        .collect();
    w.u32s(Sec::StopChangeSecs, change_secs);
    w.u32s(Sec::RouteStopOffsets, route_stop_offsets);
    w.u32s(Sec::RouteStops, route_stops);
    w.u32s(Sec::RouteTripOffsets, route_trip_offsets);
    w.u64s(Sec::RouteCellOffsets, route_cell_offsets);
    w.u32s(Sec::RouteGtfs, route_gtfs);
    w.u32s(Sec::StopRouteOffsets, stop_route_offsets);
    w.u32s(Sec::StopRoutes, stop_routes);
    w.u32s(Sec::FootOffsets, foot_offsets);
    w.u32s(Sec::FootTo, foot_to);
    w.u32s(Sec::FootSecs, foot_secs);
    w.u32s(Sec::TripRoute, trip_route);
    w.u32s(Sec::Arrivals, arrivals);
    w.u32s(Sec::Departures, departures);
    w.bytes(Sec::Flags, flags);
    w.u64s(Sec::TripIdOffsets, id_offsets);
    w.bytes(Sec::TripIdBytes, id_bytes);

    phase("export arrays built");
    let tt_path = dir.join("timetable.bin");
    let mut out = BufWriter::new(File::create(&tt_path)?);
    let timetable_bytes = w.write_to(&mut out, crate::format::MAGIC)?;
    out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    phase("timetable written");
    let fnv = {
        // Streamed: the file can be larger than the remaining memory budget.
        let mut f = std::io::BufReader::new(File::open(&tt_path)?);
        let mut h = crate::format::Fnv64::default();
        std::io::copy(&mut f, &mut h)?;
        h.0
    };

    let mut notes = notes;
    notes.push(format!(
        "{} stop events need booking (pickup/drop_off 2/3): {}; {} unknown pickup/drop_off values treated as not available",
        booking_stats.0,
        if opts.exclude_booking_stops { "excluded" } else { "routed, marked booking_required" },
        booking_stats.1
    ));
    notes.push(format!(
        "{footpaths} directed stop-to-stop footpaths exported"
    ));
    notes.push(format!(
        "minimum change time at a stop: same-stop rule, else {} s; also delays walking off after alighting there",
        opts.default_change_seconds
    ));
    let meta = Metadata {
        version: METADATA_VERSION,
        rustis_version: env!("CARGO_PKG_VERSION").into(),
        service_date: opts.date.clone(),
        timezone: ex.timezone.clone(),
        source: source.into(),
        footpaths_closed: tt.footpaths_are_transitively_closed(),
        walk_radius_m: opts.walk_radius_m,
        n_raptor_routes: n_export_routes,
        n_trips,
        timetable_bytes,
        timetable_fnv64: format!("{fnv:016x}"),
        import_notes: notes.clone(),
        transfer_mode: if rules.dropped.is_empty() {
            "strict".into()
        } else {
            "conservative".into()
        },
        dropped_transfer_rules: rules.dropped.clone(),
        excluded_trips: rules.excluded_trips.clone(),
        default_change_seconds: opts.default_change_seconds,
        stops,
        routes,
    };
    let meta_path = dir.join("metadata.json");
    let mut f = File::create(&meta_path)?;
    let json = serde_json::to_vec(&meta)?;
    f.write_all(&json)?;
    f.sync_all()?;
    for p in [&tt_path, &meta_path] {
        let mut perm = fs::metadata(p)?.permissions();
        perm.set_readonly(true);
        fs::set_permissions(p, perm)?;
    }
    File::open(dir)?.sync_all()?;
    Ok(ImportReport {
        stops: n_stops,
        raptor_routes: n_export_routes,
        trips: n_trips,
        timetable_bytes,
        metadata_bytes: json.len() as u64,
        notes,
    })
}
