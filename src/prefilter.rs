//! Streaming pre-filter for the import.
//!
//! `gtfs-structures` materialises the whole feed (all calendar days, shapes)
//! in memory, so import RAM grew with the feed's validity period rather than
//! with the two service days a snapshot contains. This pass streams the CSV
//! files and writes a reduced GTFS directory that only keeps trips whose
//! service is active on one of the snapshot days (same rule as Vulture's
//! `is_service_active`: a `calendar_dates` entry for the day wins, otherwise
//! the `calendar` row decides) plus their `stop_times`/`frequencies`.
//! `shapes.txt` and files the router never reads are dropped.
use anyhow::{Context, Result, ensure};
use chrono::{Datelike, NaiveDate};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufWriter, Read};
use std::path::Path;

/// `stop_times.txt` columns the importer reads.
const STOP_TIME_COLUMNS: [&str; 8] = [
    "trip_id",
    "arrival_time",
    "departure_time",
    "stop_id",
    "stop_sequence",
    "pickup_type",
    "drop_off_type",
    "timepoint",
];

/// Files copied unchanged (all small, all read by the importer).
const COPY: [&str; 5] = [
    "agency.txt",
    "routes.txt",
    "calendar.txt",
    "calendar_dates.txt",
    "feed_info.txt",
];

#[derive(Debug, Default)]
pub struct PrefilterStats {
    pub trips_total: u64,
    pub trips_kept: u64,
    pub stop_times_total: u64,
    pub stop_times_kept: u64,
    pub stops_total: u64,
    pub stops_kept: u64,
    /// Active GTFS-Flex trips removed (only with `exclude_flex`).
    pub flex_trips_removed: u64,
    /// Active frequency-based trips removed (only with `exclude_frequency`).
    pub frequency_trips_removed: u64,
}

enum Source {
    Dir(std::path::PathBuf),
    Zip(zip::ZipArchive<File>),
}

impl Source {
    fn open(input: &Path) -> Result<Self> {
        if input.is_dir() {
            Ok(Self::Dir(input.to_owned()))
        } else {
            Ok(Self::Zip(
                zip::ZipArchive::new(File::open(input)?).context("opening GTFS zip")?,
            ))
        }
    }
    /// Streams a member (matched by base name) to `f`; `None` if absent.
    fn with<T>(
        &mut self,
        name: &str,
        f: impl FnOnce(&mut dyn Read) -> Result<T>,
    ) -> Result<Option<T>> {
        match self {
            Self::Dir(d) => {
                let p = d.join(name);
                if !p.exists() {
                    return Ok(None);
                }
                Ok(Some(f(&mut File::open(p)?)?))
            }
            Self::Zip(z) => {
                let idx = (0..z.len()).find(|&i| {
                    z.name_for_index(i).is_some_and(|n| {
                        n == name || (n.rsplit('/').next() == Some(name) && !n.contains("__MACOSX"))
                    })
                });
                let Some(i) = idx else { return Ok(None) };
                let mut entry = z.by_index(i)?;
                Ok(Some(f(&mut entry)?))
            }
        }
    }
}

fn reader(r: &mut dyn Read) -> csv::Reader<&mut dyn Read> {
    csv::ReaderBuilder::new().flexible(true).from_reader(r)
}

fn column(headers: &csv::ByteRecord, name: &str) -> Option<usize> {
    headers.iter().position(|h| {
        let h = std::str::from_utf8(h).unwrap_or("").trim();
        h.trim_start_matches('\u{feff}') == name
    })
}

fn field(rec: &csv::ByteRecord, i: usize) -> &str {
    rec.get(i)
        .and_then(|b| std::str::from_utf8(b).ok())
        .unwrap_or("")
        .trim()
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y%m%d").ok()
}

/// Writes the reduced feed into the new directory `out`.
/// GTFS-Flex rows (zones, location groups, pickup windows) are detected in
/// `stop_times.txt`; their trips fail the import unless `exclude_flex`.
pub fn prefilter(
    input: &Path,
    out: &Path,
    days: &[NaiveDate],
    exclude_flex: bool,
    exclude_frequency: bool,
    bbox: Option<[f64; 4]>,
) -> Result<PrefilterStats> {
    fs::create_dir(out).with_context(|| format!("creating {}", out.display()))?;
    let mut src = Source::open(input)?;
    let mut stats = PrefilterStats::default();

    // calendar_dates entries on the snapshot days: (service, day) -> added?
    let mut exceptions: HashMap<(String, NaiveDate), bool> = HashMap::new();
    src.with("calendar_dates.txt", |r| {
        let mut rdr = reader(r);
        let h = rdr.byte_headers()?.clone();
        let (s, d, e) = (
            column(&h, "service_id"),
            column(&h, "date"),
            column(&h, "exception_type"),
        );
        let (Some(s), Some(d), Some(e)) = (s, d, e) else {
            return Ok(());
        };
        for rec in rdr.byte_records() {
            let rec = rec?;
            if let Some(day) = parse_date(field(&rec, d)).filter(|x| days.contains(x)) {
                exceptions.insert((field(&rec, s).to_owned(), day), field(&rec, e) == "1");
            }
        }
        Ok(())
    })?;
    // calendar rules evaluated for the snapshot days.
    let mut calendar_active: HashMap<String, Vec<bool>> = HashMap::new();
    src.with("calendar.txt", |r| {
        let mut rdr = reader(r);
        let h = rdr.byte_headers()?.clone();
        let wd = [
            "monday",
            "tuesday",
            "wednesday",
            "thursday",
            "friday",
            "saturday",
            "sunday",
        ]
        .map(|n| column(&h, n));
        let (s, a, b) = (
            column(&h, "service_id"),
            column(&h, "start_date"),
            column(&h, "end_date"),
        );
        let (Some(s), Some(a), Some(b)) = (s, a, b) else {
            return Ok(());
        };
        for rec in rdr.byte_records() {
            let rec = rec?;
            let (start, end) = (parse_date(field(&rec, a)), parse_date(field(&rec, b)));
            let flags = days
                .iter()
                .map(|day| {
                    let in_range =
                        matches!((start, end), (Some(x), Some(y)) if x <= *day && *day <= y);
                    let col = wd[day.weekday().num_days_from_monday() as usize];
                    in_range && col.is_some_and(|c| field(&rec, c) == "1")
                })
                .collect();
            calendar_active.insert(field(&rec, s).to_owned(), flags);
        }
        Ok(())
    })?;
    let active = |service: &str| {
        days.iter().enumerate().any(
            |(k, day)| match exceptions.get(&(service.to_owned(), *day)) {
                Some(&added) => added,
                None => calendar_active.get(service).is_some_and(|f| f[k]),
            },
        )
    };

    let mut kept: HashSet<Vec<u8>> = HashSet::new();
    let found = src.with("trips.txt", |r| {
        let mut rdr = reader(r);
        let h = rdr.byte_headers()?.clone();
        let (s, t) = (column(&h, "service_id"), column(&h, "trip_id"));
        let (Some(s), Some(t)) = (s, t) else {
            anyhow::bail!("trips.txt needs service_id and trip_id")
        };
        for rec in rdr.byte_records() {
            let rec = rec?;
            stats.trips_total += 1;
            if active(field(&rec, s)) {
                kept.insert(rec.get(t).unwrap_or_default().to_vec());
            }
        }
        Ok(())
    })?;
    ensure!(found.is_some(), "feed has no trips.txt");

    if let Some([min_lat, min_lon, max_lat, max_lon]) = bbox {
        // Regional partition: keep (whole) trips that serve at least one stop
        // inside the box.
        let mut inside: HashSet<Vec<u8>> = HashSet::new();
        src.with("stops.txt", |r| {
            let mut rdr = reader(r);
            let h = rdr.byte_headers()?.clone();
            let (i, la, lo) = (
                column(&h, "stop_id"),
                column(&h, "stop_lat"),
                column(&h, "stop_lon"),
            );
            let (Some(i), Some(la), Some(lo)) = (i, la, lo) else {
                anyhow::bail!("stops.txt needs stop_id/lat/lon")
            };
            for rec in rdr.byte_records() {
                let rec = rec?;
                let (lat, lon): (f64, f64) = (
                    field(&rec, la).parse().unwrap_or(f64::NAN),
                    field(&rec, lo).parse().unwrap_or(f64::NAN),
                );
                if (min_lat..=max_lat).contains(&lat) && (min_lon..=max_lon).contains(&lon) {
                    inside.insert(rec.get(i).unwrap_or_default().to_vec());
                }
            }
            Ok(())
        })?;
        let mut touching: HashSet<Vec<u8>> = HashSet::new();
        src.with("stop_times.txt", |r| {
            let mut rdr = reader(r);
            let h = rdr.byte_headers()?.clone();
            let (t, st) = (column(&h, "trip_id"), column(&h, "stop_id"));
            let (Some(t), Some(st)) = (t, st) else {
                anyhow::bail!("stop_times.txt needs trip_id/stop_id")
            };
            for rec in rdr.byte_records() {
                let rec = rec?;
                let trip = rec.get(t).unwrap_or_default();
                if kept.contains(trip)
                    && inside.contains(rec.get(st).unwrap_or_default())
                    && !touching.contains(trip)
                {
                    touching.insert(trip.to_vec());
                }
            }
            Ok(())
        })?;
        kept.retain(|t| touching.contains(t));
    }
    stats.trips_kept = kept.len() as u64;
    src.with("trips.txt", |r| {
        let mut rdr = reader(r);
        let h = rdr.byte_headers()?.clone();
        let t = column(&h, "trip_id").context("trip_id")?;
        let mut w = csv::Writer::from_writer(BufWriter::new(File::create(out.join("trips.txt"))?));
        w.write_byte_record(&h)?;
        for rec in rdr.byte_records() {
            let rec = rec?;
            if kept.contains(rec.get(t).unwrap_or_default()) {
                w.write_byte_record(&rec)?;
            }
        }
        w.flush()?;
        Ok(())
    })?;

    let mut flex: HashSet<Vec<u8>> = HashSet::new();
    let mut served: HashSet<Vec<u8>> = HashSet::new();
    let mut freq: HashSet<Vec<u8>> = HashSet::new();
    for name in ["stop_times.txt", "frequencies.txt"] {
        let is_st = name == "stop_times.txt";
        let found = src.with(name, |r| {
            let mut rdr = reader(r);
            let h = rdr.byte_headers()?.clone();
            let t = column(&h, "trip_id").with_context(|| format!("{name} needs trip_id"))?;
            let stop = column(&h, "stop_id");
            let flex_cols: Vec<usize> = [
                "location_group_id",
                "location_id",
                "start_pickup_drop_off_window",
                "end_pickup_drop_off_window",
            ]
            .iter()
            .filter_map(|c| column(&h, c))
            .collect();
            let mut w = csv::Writer::from_writer(BufWriter::new(File::create(out.join(name))?));
            // stop_times: only the columns the importer reads (no headsign strings).
            let proj: Vec<usize> = if is_st {
                STOP_TIME_COLUMNS
                    .iter()
                    .filter_map(|c| column(&h, c))
                    .collect()
            } else {
                (0..h.len()).collect()
            };
            let pick = |rec: &csv::ByteRecord| -> csv::ByteRecord {
                proj.iter()
                    .map(|&i| rec.get(i).unwrap_or_default())
                    .collect()
            };
            w.write_byte_record(&pick(&h))?;
            for rec in rdr.byte_records() {
                let rec = rec?;
                if is_st {
                    stats.stop_times_total += 1;
                }
                if kept.contains(rec.get(t).unwrap_or_default()) {
                    if !is_st {
                        freq.insert(rec.get(t).unwrap_or_default().to_vec());
                    }
                    if is_st {
                        stats.stop_times_kept += 1;
                        if let Some(i) = stop {
                            let id = rec.get(i).unwrap_or_default();
                            if !served.contains(id) {
                                served.insert(id.to_vec());
                            }
                        }
                        let no_stop = stop.is_none_or(|i| field(&rec, i).is_empty());
                        if no_stop || flex_cols.iter().any(|&i| !field(&rec, i).is_empty()) {
                            flex.insert(rec.get(t).unwrap_or_default().to_vec());
                        }
                    }
                    w.write_byte_record(&pick(&rec))?;
                }
            }
            w.flush()?;
            Ok(())
        })?;
        ensure!(found.is_some() || !is_st, "feed has no stop_times.txt");
    }

    ensure!(
        flex.is_empty() || exclude_flex,
        "feed contains {} GTFS-Flex trips (zones/location groups/pickup windows) active on the snapshot days; \
         not supported (--exclude-flex-trips removes them and marks the snapshot incomplete)",
        flex.len()
    );
    ensure!(
        freq.is_empty() || exclude_frequency,
        "feed contains {} frequency-based trips active on the snapshot days; not supported \
         (--exclude-frequency-trips removes them and marks the snapshot incomplete)",
        freq.len()
    );
    stats.flex_trips_removed = flex.len() as u64;
    stats.frequency_trips_removed = freq.difference(&flex).count() as u64;
    flex.extend(freq);
    if !flex.is_empty() {
        for name in ["trips.txt", "stop_times.txt", "frequencies.txt"] {
            let path = out.join(name);
            if !path.exists() {
                continue;
            }
            let tmp = out.join(format!("{name}.tmp"));
            {
                let mut rdr = csv::ReaderBuilder::new().flexible(true).from_path(&path)?;
                let h = rdr.byte_headers()?.clone();
                let t = column(&h, "trip_id").context("trip_id")?;
                let mut w = csv::Writer::from_writer(BufWriter::new(File::create(&tmp)?));
                w.write_byte_record(&h)?;
                for rec in rdr.byte_records() {
                    let rec = rec?;
                    if !flex.contains(rec.get(t).unwrap_or_default()) {
                        w.write_byte_record(&rec)?;
                    }
                }
                w.flush()?;
            }
            fs::rename(&tmp, &path)?;
        }
    }

    for name in COPY {
        src.with(name, |r| {
            let mut f = BufWriter::new(File::create(out.join(name))?);
            std::io::copy(r, &mut f)?;
            Ok(())
        })?;
    }

    // Stops: served ones, endpoints of transfer rows touching a served stop
    // (keeps walking chains through unserved stops), and their parents.
    let mut keep = served;
    let mut transfer_ends: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    src.with("transfers.txt", |r| {
        let mut rdr = reader(r);
        let h = rdr.byte_headers()?.clone();
        let (Some(f), Some(t)) = (column(&h, "from_stop_id"), column(&h, "to_stop_id")) else {
            return Ok(());
        };
        for rec in rdr.byte_records() {
            let rec = rec?;
            transfer_ends.push((
                rec.get(f).unwrap_or_default().to_vec(),
                rec.get(t).unwrap_or_default().to_vec(),
            ));
        }
        Ok(())
    })?;
    let touching: Vec<Vec<u8>> = transfer_ends
        .iter()
        .filter(|(a, b)| keep.contains(a) || keep.contains(b))
        .flat_map(|(a, b)| [a.clone(), b.clone()])
        .collect();
    keep.extend(touching);
    drop(transfer_ends);
    let mut parents: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    src.with("stops.txt", |r| {
        let mut rdr = reader(r);
        let h = rdr.byte_headers()?.clone();
        let (Some(i), Some(p)) = (column(&h, "stop_id"), column(&h, "parent_station")) else {
            return Ok(());
        };
        for rec in rdr.byte_records() {
            let rec = rec?;
            let parent = rec.get(p).unwrap_or_default();
            if !parent.is_empty() {
                parents.insert(rec.get(i).unwrap_or_default().to_vec(), parent.to_vec());
            }
        }
        Ok(())
    })?;
    for _ in 0..2 {
        let add: Vec<Vec<u8>> = keep
            .iter()
            .filter_map(|s| parents.get(s).cloned())
            .collect();
        keep.extend(add);
    }
    let filter_file =
        |src: &mut Source, name: &str, cols: &[&str], stats: &mut PrefilterStats| -> Result<()> {
            src.with(name, |r| {
                let mut rdr = reader(r);
                let h = rdr.byte_headers()?.clone();
                let idx: Vec<usize> = cols.iter().filter_map(|c| column(&h, c)).collect();
                let mut w = csv::Writer::from_writer(BufWriter::new(File::create(out.join(name))?));
                w.write_byte_record(&h)?;
                for rec in rdr.byte_records() {
                    let rec = rec?;
                    let ids: Vec<&[u8]> = idx
                        .iter()
                        .map(|&i| rec.get(i).unwrap_or_default())
                        .collect();
                    if name == "stops.txt" {
                        stats.stops_total += 1;
                    }
                    // Rows with missing ids are kept so the importer reports them.
                    if ids.iter().all(|id| id.is_empty() || keep.contains(*id))
                        || ids.iter().any(|id| id.is_empty())
                    {
                        if name == "stops.txt" {
                            stats.stops_kept += 1;
                        }
                        w.write_byte_record(&rec)?;
                    }
                }
                w.flush()?;
                Ok(())
            })?;
            Ok(())
        };
    filter_file(&mut src, "stops.txt", &["stop_id"], &mut stats)?;
    filter_file(
        &mut src,
        "transfers.txt",
        &["from_stop_id", "to_stop_id"],
        &mut stats,
    )?;
    Ok(stats)
}
