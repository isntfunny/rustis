//! Read-only inventory of many GTFS feeds (sizes, row counts, validity,
//! time zones, transfer/pickup semantics, frequencies, GTFS-Flex markers).
//! Every member is streamed; memory stays bounded by small per-feed sets.
use crate::feed::{Feed, field, parse_date, parse_time, text};
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{BuildHasher, RandomState};
use std::path::Path;

fn bump(m: &mut BTreeMap<String, u64>, k: &[u8]) {
    *m.entry(String::from_utf8_lossy(k).into_owned())
        .or_default() += 1;
}

/// Scans one feed. Errors are reported in the result instead of aborting.
pub fn scan_feed(path: &Path) -> Value {
    let t0 = std::time::Instant::now();
    let mut out = json!({ "file": path.file_name().map(|f| f.to_string_lossy().into_owned()) });
    match scan_inner(path, &mut out) {
        Ok(()) => {}
        Err(e) => out["error"] = json!(format!("{e:#}")),
    }
    out["scan_s"] = json!((t0.elapsed().as_secs_f64() * 10.0).round() / 10.0);
    out
}

fn scan_inner(path: &Path, out: &mut Value) -> Result<()> {
    out["zip_bytes"] = json!(std::fs::metadata(path)?.len());
    let mut feed = Feed::open(path)?;
    if let Feed::Zip(z) = &mut feed {
        let mut sizes = BTreeMap::new();
        for i in 0..z.len() {
            let f = z.by_index_raw(i)?;
            if !f.is_dir() {
                let n = f.name().rsplit('/').next().unwrap_or("").to_owned();
                sizes.insert(n, f.size());
            }
        }
        out["members"] = json!(sizes);
    }
    let members = feed.members();
    out["flex_files"] = json!(
        members
            .iter()
            .filter(|m| matches!(
                m.as_str(),
                "locations.geojson"
                    | "location_groups.txt"
                    | "location_group_stops.txt"
                    | "booking_rules.txt"
                    | "stop_areas.txt"
            ))
            .collect::<Vec<_>>()
    );
    let mut counts: BTreeMap<&str, u64> = BTreeMap::new();

    let mut tz = BTreeSet::new();
    feed.rows("agency.txt", |c, r| {
        tz.insert(text(r, c.get("agency_timezone")));
        Ok(())
    })?;
    out["agency_tz"] = json!(tz);

    let (mut stop_tz, mut loc_types, mut parents, mut n, mut no_coord) =
        (BTreeSet::new(), BTreeMap::new(), 0u64, 0u64, 0u64);
    feed.rows("stops.txt", |c, r| {
        n += 1;
        let t = field(r, c.get("stop_timezone"));
        if !t.is_empty() {
            stop_tz.insert(String::from_utf8_lossy(t).into_owned());
        }
        let lt = field(r, c.get("location_type"));
        bump(&mut loc_types, if lt.is_empty() { b"0" } else { lt });
        parents += u64::from(!field(r, c.get("parent_station")).is_empty());
        no_coord += u64::from(field(r, c.get("stop_lat")).is_empty());
        Ok(())
    })?;
    counts.insert("stops", n);
    out["stop_tz"] = json!(stop_tz);
    out["location_types"] = json!(loc_types);
    out["stops_with_parent"] = json!(parents);
    out["stops_without_coord"] = json!(no_coord);

    let mut route_types = BTreeMap::new();
    n = 0;
    feed.rows("routes.txt", |c, r| {
        n += 1;
        bump(&mut route_types, field(r, c.get("route_type")));
        Ok(())
    })?;
    counts.insert("routes", n);
    out["route_types"] = json!(route_types);

    // Calendar: validity range and active days per service id.
    let (mut lo, mut hi) = (i32::MAX, i32::MIN);
    let mut service_days: HashMap<Vec<u8>, i64> = HashMap::new();
    n = 0;
    feed.rows("calendar.txt", |c, r| {
        n += 1;
        let (s, e) = (field(r, c.get("start_date")), field(r, c.get("end_date")));
        if let (Ok(s), Ok(e)) = (parse_date(s), parse_date(e))
            && e >= s
        {
            lo = lo.min(s);
            hi = hi.max(e);
            let wd: i64 = [
                "monday",
                "tuesday",
                "wednesday",
                "thursday",
                "friday",
                "saturday",
                "sunday",
            ]
            .iter()
            .map(|d| i64::from(field(r, c.get(d)) == b"1"))
            .sum();
            *service_days
                .entry(field(r, c.get("service_id")).to_vec())
                .or_default() += (i64::from(e - s) + 1) * wd / 7;
        }
        Ok(())
    })?;
    counts.insert("calendar", n);
    n = 0;
    feed.rows("calendar_dates.txt", |c, r| {
        n += 1;
        if let Ok(d) = parse_date(field(r, c.get("date"))) {
            let add = field(r, c.get("exception_type")) == b"1";
            if add {
                lo = lo.min(d);
                hi = hi.max(d);
            }
            *service_days
                .entry(field(r, c.get("service_id")).to_vec())
                .or_default() += if add { 1 } else { -1 };
        }
        Ok(())
    })?;
    counts.insert("calendar_dates", n);
    if hi >= lo {
        out["service_range"] = json!([
            crate::feed::date_of(lo).to_string(),
            crate::feed::date_of(hi).to_string()
        ]);
        out["service_range_days"] = json!(hi - lo + 1);
    }

    // Trips: count and approximate trip-days (Σ active days of their service).
    let hasher = RandomState::new();
    let mut trip_days = 0i64;
    let mut trip_service: HashMap<u64, i64> = HashMap::new();
    n = 0;
    feed.rows("trips.txt", |c, r| {
        n += 1;
        let d = service_days
            .get(field(r, c.get("service_id")))
            .copied()
            .unwrap_or(0)
            .max(0);
        trip_days += d;
        trip_service.insert(hasher.hash_one(field(r, c.get("trip_id"))), d);
        Ok(())
    })?;
    counts.insert("trips", n);
    drop(service_days);

    let mut freq_exact = BTreeMap::new();
    let mut freq_trips = HashSet::new();
    let mut freq_departures_per_day = 0u64;
    let mut freq_trip_days = 0i64;
    n = 0;
    feed.rows("frequencies.txt", |c, r| {
        n += 1;
        let e = field(r, c.get("exact_times"));
        bump(&mut freq_exact, if e.is_empty() { b"0" } else { e });
        let h = hasher.hash_one(field(r, c.get("trip_id")));
        let (s, e, w) = (
            parse_time(field(r, c.get("start_time"))).ok().flatten(),
            parse_time(field(r, c.get("end_time"))).ok().flatten(),
            std::str::from_utf8(field(r, c.get("headway_secs")))
                .ok()
                .and_then(|v| v.parse::<u64>().ok()),
        );
        if let (Some(s), Some(e), Some(w)) = (s, e, w)
            && w > 0
            && e > s
        {
            let k = (u64::from(e - s)).div_ceil(w);
            freq_departures_per_day += k;
            freq_trip_days += trip_service.get(&h).copied().unwrap_or(0) * k as i64;
        }
        freq_trips.insert(h);
        Ok(())
    })?;
    counts.insert("frequencies", n);
    out["frequency_exact_times"] = json!(freq_exact);
    out["frequency_trips"] = json!(freq_trips.len());
    out["frequency_departures_per_service_day"] = json!(freq_departures_per_day);

    let (mut tt, mut specific) = (BTreeMap::new(), BTreeMap::new());
    n = 0;
    feed.rows("transfers.txt", |c, r| {
        n += 1;
        let t = field(r, c.get("transfer_type"));
        let t = if t.is_empty() { &b"0"[..] } else { t };
        bump(&mut tt, t);
        if ["from_trip_id", "to_trip_id", "from_route_id", "to_route_id"]
            .iter()
            .any(|k| !field(r, c.get(k)).is_empty())
        {
            bump(&mut specific, t);
        }
        Ok(())
    })?;
    counts.insert("transfers", n);
    out["transfer_types"] = json!(tt);
    out["transfer_specific_by_type"] = json!(specific);

    // stop_times: rows, pickup/drop-off semantics, max hour, trip contiguity.
    let (mut pick, mut drop_) = (BTreeMap::new(), BTreeMap::new());
    let (mut max_h, mut no_time, mut flex_rows, mut regroups, mut events_days) =
        (0u32, 0u64, 0u64, 0u64, 0i64);
    let mut seen: HashSet<u64> = HashSet::new();
    let mut last: Option<u64> = None;
    let mut flex_cols = false;
    n = 0;
    feed.rows("stop_times.txt", |c, r| {
        if n == 0 {
            flex_cols = [
                "start_pickup_drop_off_window",
                "location_group_id",
                "location_id",
            ]
            .iter()
            .any(|k| c.get(k).is_some());
        }
        n += 1;
        let p = field(r, c.get("pickup_type"));
        if !p.is_empty() && p != b"0" {
            bump(&mut pick, p);
        }
        let d = field(r, c.get("drop_off_type"));
        if !d.is_empty() && d != b"0" {
            bump(&mut drop_, d);
        }
        let mut t = field(r, c.get("departure_time"));
        if t.is_empty() {
            t = field(r, c.get("arrival_time"));
        }
        match parse_time(t) {
            Ok(Some(s)) => max_h = max_h.max(s / 3600),
            _ => no_time += 1,
        }
        if field(r, c.get("stop_id")).is_empty()
            || !field(r, c.get("start_pickup_drop_off_window")).is_empty()
        {
            flex_rows += 1;
        }
        let h = hasher.hash_one(field(r, c.get("trip_id")));
        if last != Some(h) {
            if !seen.insert(h) {
                regroups += 1;
            }
            last = Some(h);
        }
        events_days += trip_service.get(&h).copied().unwrap_or(0);
        Ok(())
    })?;
    counts.insert("stop_times", n);
    out["pickup_types"] = json!(pick);
    out["drop_off_types"] = json!(drop_);
    out["max_hour"] = json!(max_h);
    out["stop_times_without_time"] = json!(no_time);
    out["stop_times_flex_rows"] = json!(flex_rows);
    out["stop_times_flex_columns"] = json!(flex_cols);
    out["stop_times_trip_regroups"] = json!(regroups);
    out["trip_days"] = json!(trip_days);
    out["frequency_trip_days"] = json!(freq_trip_days);
    out["stop_time_days"] = json!(events_days);
    out["counts"] = json!(counts);
    Ok(())
}

/// Dataset list from a MOTIS `config.yml` (plain and YAML complex keys).
pub fn motis_datasets(config: &str) -> Vec<(String, String)> {
    let (mut out, mut name) = (Vec::new(), None::<String>);
    for line in config.lines() {
        if let Some(rest) = line.strip_prefix("    ? ") {
            name = Some(rest.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("    ")
            && !rest.starts_with(' ')
            && !rest.starts_with(':')
            && rest.trim_end().ends_with(':')
        {
            name = Some(rest.trim_end().trim_end_matches(':').to_owned());
        }
        let p = line
            .strip_prefix("      path:")
            .or_else(|| line.strip_prefix("    : path:"));
        if let (Some(p), Some(n)) = (p, &name) {
            out.push((n.clone(), p.trim().to_owned()));
        }
    }
    out
}

/// `script:` of each dataset of a MOTIS config, resolved against `dir`
/// (the config's directory), by dataset name.
pub fn motis_scripts(
    config: &str,
    dir: &Path,
) -> std::collections::HashMap<String, std::path::PathBuf> {
    let (mut out, mut name) = (std::collections::HashMap::new(), None::<String>);
    for line in config.lines() {
        if let Some(rest) = line.strip_prefix("    ? ") {
            name = Some(rest.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("    ")
            && !rest.starts_with(' ')
            && !rest.starts_with(':')
            && rest.trim_end().ends_with(':')
        {
            name = Some(rest.trim_end().trim_end_matches(':').to_owned());
        }
        let p = line
            .strip_prefix("      script:")
            .or_else(|| line.strip_prefix("    : script:"));
        if let (Some(p), Some(n)) = (p, &name) {
            out.insert(n.clone(), dir.join(p.trim().trim_matches(['"', '\''])));
        }
    }
    out
}

/// Scans every dataset of a MOTIS config with `jobs` threads, largest ZIP
/// first; appends one JSON line per feed to `out` as soon as it is done and
/// skips datasets already present there (resumable).
pub fn run(config: &Path, feed_dir: &Path, out: &Path, jobs: usize) -> Result<()> {
    use std::io::{BufRead, Write};
    let done: HashSet<String> = match std::fs::File::open(out) {
        Ok(f) => std::io::BufReader::new(f)
            .lines()
            .map_while(|l| l.ok())
            .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
            .filter_map(|v| v["name"].as_str().map(str::to_owned))
            .collect(),
        Err(_) => HashSet::new(),
    };
    let mut todo: Vec<(String, std::path::PathBuf, u64)> =
        motis_datasets(&std::fs::read_to_string(config)?)
            .into_iter()
            .filter(|(n, _)| !done.contains(n))
            .map(|(n, p)| {
                let f = feed_dir.join(Path::new(&p).file_name().unwrap_or_default());
                let size = std::fs::metadata(&f).map_or(0, |m| m.len());
                (n, f, size)
            })
            .collect();
    todo.sort_by_key(|t| std::cmp::Reverse(t.2));
    let total = todo.len();
    eprintln!("{} done, {total} to scan", done.len());
    let queue = std::sync::Mutex::new(todo.into_iter());
    let sink = std::sync::Mutex::new(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(out)?,
    );
    let finished = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..jobs.max(1) {
            s.spawn(|| {
                loop {
                    let Some((name, file, _)) = queue.lock().unwrap().next() else {
                        break;
                    };
                    let mut v = scan_feed(&file);
                    v["name"] = json!(name);
                    let line = serde_json::to_string(&v).unwrap();
                    let mut f = sink.lock().unwrap();
                    let _ = writeln!(f, "{line}").and_then(|_| f.flush());
                    let k = finished.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    eprintln!(
                        "[{k}/{total}] {name} stop_times={} {}s {}",
                        v["counts"]["stop_times"],
                        v["scan_s"],
                        v["error"].as_str().unwrap_or("")
                    );
                }
            });
        }
    });
    Ok(())
}
