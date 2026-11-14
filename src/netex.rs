//! Object-at-a-time NeTEx reader. First pass resolves shared objects; second
//! pass converts journeys directly to the existing feed-local routing model.
use super::*;
use quick_xml::{Reader, events::Event};
use std::fs::File;
use std::io::{BufRead, BufReader};

#[derive(Default, Debug)]
struct Node {
    tag: String,
    id: String,
    reference: String,
    text: String,
    children: Vec<Node>,
}
impl Node {
    fn child(&self, tag: &str) -> Option<&Node> {
        self.children.iter().find(|n| n.tag == tag)
    }
    fn value(&self, tag: &str) -> &str {
        self.child(tag).map_or("", |n| n.text.trim())
    }
    fn reference(&self, tag: &str) -> &str {
        self.child(tag).map_or("", |n| n.reference.as_str())
    }
    fn find(&self, tag: &str) -> Option<&Node> {
        self.children
            .iter()
            .find_map(|n| if n.tag == tag { Some(n) } else { n.find(tag) })
    }
    fn deep_value(&self, tag: &str) -> &str {
        self.find(tag).map_or("", |n| n.text.trim())
    }
}

fn objects(
    input: impl BufRead,
    wanted: &[&str],
    mut emit: impl FnMut(Node) -> Result<()>,
) -> Result<()> {
    let mut reader = Reader::from_reader(input);
    reader.config_mut().expand_empty_elements = true;
    let mut buf = Vec::new();
    let mut stack: Vec<Node> = Vec::new();
    let mut root = false;
    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(e) => {
                let tag = std::str::from_utf8(e.local_name().as_ref())?.to_owned();
                if tag == "PublicationDelivery" {
                    root = true;
                }
                if !stack.is_empty() || wanted.contains(&tag.as_str()) {
                    let mut n = Node {
                        tag,
                        ..Default::default()
                    };
                    for attr in e.attributes() {
                        let a = attr?;
                        match a.key.as_ref() {
                            b"id" => {
                                n.id = a.decode_and_unescape_value(reader.decoder())?.into_owned()
                            }
                            b"ref" => {
                                n.reference =
                                    a.decode_and_unescape_value(reader.decoder())?.into_owned()
                            }
                            _ => {}
                        }
                    }
                    ensure!(stack.len() < 128, "NeTEx object nesting limit");
                    stack.push(n);
                }
            }
            Event::Text(e) => {
                if let Some(n) = stack.last_mut() {
                    n.text.push_str(&e.xml_content()?);
                }
            }
            Event::GeneralRef(e) => {
                if let Some(n) = stack.last_mut() {
                    let name = e.decode()?;
                    let escaped = format!("&{name};");
                    n.text.push_str(&quick_xml::escape::unescape(&escaped)?);
                }
            }
            Event::CData(e) => {
                if let Some(n) = stack.last_mut() {
                    n.text.push_str(&e.decode()?);
                }
            }
            Event::End(_) => {
                if let Some(n) = stack.pop() {
                    if let Some(parent) = stack.last_mut() {
                        parent.children.push(n);
                    } else {
                        emit(n)?;
                    }
                }
            }
            Event::DocType(_) => bail!("NeTEx DTDs are unsupported"),
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    ensure!(root && stack.is_empty(), "invalid NeTEx publication");
    Ok(())
}
fn scan(path: &Path, wanted: &[&str], mut emit: impl FnMut(Node) -> Result<()>) -> Result<()> {
    if path.is_dir() {
        let mut paths = std::fs::read_dir(path)?
            .map(|e| Ok(e?.path()))
            .collect::<Result<Vec<_>>>()?;
        paths.sort();
        for p in paths
            .into_iter()
            .filter(|p| p.extension().is_some_and(|x| x == "xml"))
        {
            objects(BufReader::new(File::open(p)?), wanted, &mut emit)?;
        }
    } else {
        let mut z = zip::ZipArchive::new(File::open(path)?)?;
        for i in 0..z.len() {
            let f = z.by_index(i)?;
            if f.name().ends_with(".xml") {
                objects(BufReader::new(f), wanted, &mut emit)?;
            }
        }
    }
    Ok(())
}
struct Point {
    id: String,
    stop: String,
    flags: u8,
}
struct JourneyPattern {
    line: String,
    points: Vec<Point>,
}
fn stop(n: &Node, parent: Option<String>, name: &str) -> Stop {
    let lat = n
        .deep_value("Latitude")
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && v.abs() <= 90.0);
    let lon = n
        .deep_value("Longitude")
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && v.abs() <= 180.0);
    Stop {
        stop_id: ns(name, &n.id),
        name: n.value("Name").into(),
        lat,
        lon,
        parent_station: parent,
        served: false,
    }
}
fn time(n: &Node, kind: &str) -> Result<Option<u32>> {
    let Some(base) = parse_time(n.value(&format!("{kind}Time")).as_bytes())? else {
        return Ok(None);
    };
    let off = n.value(&format!("{kind}DayOffset"));
    let days = if off.is_empty() {
        0
    } else {
        off.parse::<u32>()?
    };
    Ok(Some(
        base.checked_add(days.checked_mul(86400).context("day offset overflow")?)
            .context("time overflow")?,
    ))
}
fn mode(n: &Node) -> u16 {
    match n.value("TransportMode") {
        "bus" => 3,
        "coach" => 200,
        "tram" => 0,
        "metro" => 1,
        "water" => 4,
        _ => match n.deep_value("RailSubmode") {
            "highSpeedRail" => 101,
            "longDistance" => 102,
            "nightRail" => 105,
            "regionalRail" => 106,
            "suburbanRailway" => 109,
            _ => 2,
        },
    }
}

// NeTEx times are wall-clock times, unlike GTFS's noon-minus-12h basis.
// Split only on days whose UTC conversion differs (DST, including overnight).
fn time_variants(
    times: &[u32],
    bits: &[u64],
    dates: &[(jiff::civil::Date, i64, i64)],
    tz: &jiff::tz::TimeZone,
    before_padding: &mut u64,
) -> Result<Vec<(Vec<u32>, Vec<u64>)>> {
    let mut variants = HashMap::<Vec<u32>, Vec<u64>>::new();
    let last_offset = *times.last().context("empty times")? as usize / 86400;
    ensure!(last_offset < 7, "NeTEx journey exceeds seven days");
    for day in 0..dates.len() - 7 {
        if bits[day / 64] & (1u64 << (day % 64)) == 0 {
            continue;
        }
        let (date, base, midnight) = dates[day];
        let ordinary = midnight == base
            && (0..=last_offset + 1)
                .all(|offset| dates[day + offset].2 == midnight + offset as i64 * 86400);
        let (actual, index) = if ordinary {
            (times.to_vec(), day)
        } else {
            let utc = times
                .iter()
                .map(|&t| {
                    date.at(0, 0, 0, 0)
                        .checked_add(jiff::Span::new().seconds(t as i64))?
                        .to_zoned(tz.clone())
                        .map(|z| z.timestamp().as_second())
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            if utc[0] < base && day == 0 {
                *before_padding += 1;
                continue;
            }
            let index = if utc[0] < base { day - 1 } else { day };
            let actual = utc
                .into_iter()
                .map(|t| u32::try_from(t - dates[index].1).context("negative converted NeTEx time"))
                .collect::<Result<Vec<_>>>()?;
            ensure!(
                actual.windows(2).all(|w| w[0] <= w[1]),
                "decreasing NeTEx UTC times"
            );
            (actual, index)
        };
        let out = variants
            .entry(actual)
            .or_insert_with(|| vec![0; bits.len()]);
        out[index / 64] |= 1u64 << (index % 64);
    }
    Ok(variants.into_iter().collect())
}

pub(super) fn read(p: &mut Parser<'_>, spec: &FeedSpec) -> Result<FeedData> {
    let mut stops = Vec::new();
    let mut stop_idx = HashMap::<String, u32>::new();
    let mut scheduled = HashMap::<String, Stop>::new();
    let mut assignments = HashMap::<String, (String, String)>::new();
    let mut periods = HashMap::<String, Vec<u64>>::new();
    let mut days = HashMap::<String, Vec<String>>::new();
    let mut jps = HashMap::<String, JourneyPattern>::new();
    let mut lines = HashMap::<String, (String, u16)>::new();
    let mut timezone = String::new();
    let mut report = FeedReport {
        name: spec.name.clone(),
        file: spec.path.display().to_string(),
        ..Default::default()
    };
    let first = p.opts.first_day.checked_sub(jiff::Span::new().days(1))?;
    let mut range: Option<(jiff::civil::Date, jiff::civil::Date)> = None;
    scan(
        &spec.path,
        &[
            "StopPlace",
            "ScheduledStopPoint",
            "PassengerStopAssignment",
            "UicOperatingPeriod",
            "AvailabilityCondition",
            "DayTypeAssignment",
            "ServiceJourneyPattern",
            "Line",
            "FrameDefaults",
            "ServiceJourneyInterchange",
        ],
        |n| {
            match n.tag.as_str() {
                "FrameDefaults" => {
                    let t = n.deep_value("TimeZone");
                    let t = if t.is_empty()
                        && n.deep_value("TimeZoneOffset") == "+1"
                        && n.deep_value("SummerTimeZoneOffset") == "+2"
                    {
                        "Europe/Paris"
                    } else {
                        t
                    };
                    if !t.is_empty() {
                        ensure!(
                            timezone.is_empty() || timezone == t,
                            "conflicting NeTEx time zones"
                        );
                        timezone = t.into();
                    }
                }
                "StopPlace" => {
                    if !stop_idx.contains_key(&n.id) {
                        stop_idx.insert(n.id.clone(), stops.len() as u32);
                        stops.push(stop(&n, None, &spec.name));
                    }
                    if let Some(quays) = n.child("quays") {
                        for q in &quays.children {
                            if q.tag != "Quay" || stop_idx.contains_key(&q.id) {
                                continue;
                            }
                            let mut s = stop(q, Some(ns(&spec.name, &n.id)), &spec.name);
                            let parent = &stops[stop_idx[&n.id] as usize];
                            if s.name.is_empty() {
                                s.name = parent.name.clone();
                            }
                            if s.lat.is_none() || s.lon.is_none() {
                                s.lat = parent.lat;
                                s.lon = parent.lon;
                            }
                            stop_idx.insert(q.id.clone(), stops.len() as u32);
                            stops.push(s);
                        }
                    }
                }
                "ScheduledStopPoint" => {
                    scheduled.insert(n.id.clone(), stop(&n, None, &spec.name));
                }
                "PassengerStopAssignment" => {
                    assignments.insert(
                        n.reference("ScheduledStopPointRef").into(),
                        (
                            n.reference("QuayRef").into(),
                            n.reference("StopPlaceRef").into(),
                        ),
                    );
                }
                "UicOperatingPeriod" | "AvailabilityCondition" => {
                    if n.value("ValidDayBits").is_empty() {
                        return Ok(());
                    }
                    let from = n
                        .value("FromDate")
                        .get(..10)
                        .context("missing FromDate")?
                        .parse::<jiff::civil::Date>()?;
                    let to = n
                        .value("ToDate")
                        .get(..10)
                        .context("missing ToDate")?
                        .parse::<jiff::civil::Date>()?;
                    ensure!(from <= to, "reversed operating period");
                    let bits = n.value("ValidDayBits").as_bytes();
                    let len = from.until(to)?.get_days() as usize + 1;
                    ensure!(
                        bits.len() >= len && bits.iter().all(|b| matches!(b, b'0' | b'1')),
                        "invalid ValidDayBits"
                    );
                    let mut out = vec![0u64; p.words];
                    for i in 0..=p.opts.horizon_days {
                        let d = first.checked_add(jiff::Span::new().days(i as i64))?;
                        if d >= from && d <= to && bits[from.until(d)?.get_days() as usize] == b'1'
                        {
                            out[i as usize / 64] |= 1u64 << (i % 64);
                        }
                    }
                    range = Some(range.map_or((from, to), |(a, b)| (a.min(from), b.max(to))));
                    periods.insert(n.id, out);
                }
                "DayTypeAssignment" => {
                    ensure!(
                        n.value("IsAvailable") != "false" && n.child("Date").is_none(),
                        "unsupported NeTEx date/exclusion assignment"
                    );
                    days.entry(n.reference("DayTypeRef").into())
                        .or_default()
                        .push(n.reference("OperatingPeriodRef").into());
                }
                "Line" => {
                    let label = [n.value("ShortName"), n.value("PublicCode"), n.value("Name")]
                        .into_iter()
                        .find(|s| !s.is_empty())
                        .unwrap_or("")
                        .to_owned();
                    let ty = mode(&n);
                    lines.insert(n.id, (label, ty));
                }
                "ServiceJourneyPattern" => {
                    let seq = n
                        .child("pointsInSequence")
                        .context("journey pattern without points")?;
                    let points = seq
                        .children
                        .iter()
                        .map(|x| {
                            let pass = matches!(
                                x.value("StopUse"),
                                "passthrough" | "noBoardingOrAlighting"
                            );
                            Point {
                                id: x.id.clone(),
                                stop: x.reference("ScheduledStopPointRef").into(),
                                flags: if pass {
                                    3
                                } else {
                                    u8::from(matches!(x.value("ForBoarding"), "false" | "0"))
                                        | (u8::from(matches!(
                                            x.value("ForAlighting"),
                                            "false" | "0"
                                        )) << 1)
                                },
                            }
                        })
                        .collect();
                    let line = n
                        .find("LineRef")
                        .map_or("", |x| x.reference.as_str())
                        .to_owned();
                    jps.insert(n.id, JourneyPattern { line, points });
                }
                "ServiceJourneyInterchange" => {
                    *report
                        .skipped
                        .entry("netex_interchanges_not_supported".into())
                        .or_default() += 1;
                }
                _ => {}
            }
            Ok(())
        },
    )?;
    ensure!(!timezone.is_empty(), "NeTEx default time zone missing");
    let tz = p.tz_index(&timezone)?;
    let zone = jiff::tz::TimeZone::get(&timezone)?;
    let dates = (0..p.opts.horizon_days + 8)
        .map(|i| {
            let d = first.checked_add(jiff::Span::new().days(i as i64))?;
            Ok((
                d,
                noon_minus_12h(&zone, d)?,
                d.at(0, 0, 0, 0)
                    .to_zoned(zone.clone())?
                    .timestamp()
                    .as_second(),
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut resolved = HashMap::new();
    for (id, (q, s)) in assignments {
        if let Some(&i) = stop_idx.get(&q).or_else(|| stop_idx.get(&s)) {
            resolved.insert(id, i);
        }
    }
    for (id, s) in scheduled {
        resolved.entry(id).or_insert_with(|| {
            let i = stops.len() as u32;
            stops.push(s);
            i
        });
    }
    let mut route_infos = Vec::new();
    let mut info_idx = HashMap::new();
    let mut patterns = Vec::<Pattern>::new();
    let mut pattern_idx = HashMap::new();
    let mut gtfs_trip_ids = Vec::new();
    scan(&spec.path, &["ServiceJourney"], |n| {
        report.gtfs_trips += 1;
        let mut bits = vec![0u64; p.words];
        let refs = if let Some(v) = n.child("validityConditions") {
            vec![v.reference("AvailabilityConditionRef").to_owned()]
        } else {
            let mut refs = Vec::new();
            for d in &n
                .child("dayTypes")
                .context("missing journey dayTypes")?
                .children
            {
                refs.extend(
                    days.get(&d.reference)
                        .with_context(|| format!("unknown day type {}", d.reference))?
                        .iter()
                        .cloned(),
                );
            }
            refs
        };
        ensure!(!refs.is_empty(), "journey without operating period");
        for r in refs {
            let source = periods
                .get(&r)
                .with_context(|| format!("unknown operating period {r}"))?;
            for (b, s) in bits.iter_mut().zip(source) {
                *b |= *s;
            }
        }
        if bits.iter().all(|b| *b == 0) {
            *report
                .skipped
                .entry("trips_not_running_in_horizon".into())
                .or_default() += 1;
            return Ok(());
        }
        let jp_ref = if n.reference("JourneyPatternRef").is_empty() {
            n.reference("ServiceJourneyPatternRef")
        } else {
            n.reference("JourneyPatternRef")
        };
        let jp = jps
            .get(jp_ref)
            .with_context(|| format!("unknown journey pattern {jp_ref}"))?;
        let mut times = vec![None; jp.points.len()];
        for (position, t) in n
            .child("passingTimes")
            .context("missing passingTimes")?
            .children
            .iter()
            .enumerate()
        {
            let id = if t.reference("PointInJourneyPatternRef").is_empty() {
                t.reference("StopPointInJourneyPatternRef")
            } else {
                t.reference("PointInJourneyPatternRef")
            };
            // SNCF reuses point IDs on loop journeys. Prefer the matching
            // occurrence at the published position, as nigiri does.
            let i = if jp.points.get(position).is_some_and(|x| x.id == id)
                && times[position].is_none()
            {
                position
            } else {
                jp.points
                    .iter()
                    .enumerate()
                    .position(|(i, x)| x.id == id && times[i].is_none())
                    .context("passing time references unknown or already timed point")?
            };
            let a = time(t, "Arrival")?;
            let d = time(t, "Departure")?;
            times[i] = a.or(d).zip(d.or(a));
        }
        ensure!(times.len() >= 2, "journey needs at least two stops");
        ensure!(
            times.iter().flatten().count() >= 2,
            "journey needs two timed stops"
        );
        let mut previous = times.iter().flatten().next().unwrap().0;
        let mut flat = Vec::new();
        for t in times {
            let (a, d) = match t {
                Some(pair) => pair,
                None => {
                    *report
                        .skipped
                        .entry("netex_untimed_stops_filled_like_nigiri".into())
                        .or_default() += 1;
                    (previous, previous)
                }
            };
            previous = d;
            ensure!(
                a <= d && flat.last().is_none_or(|prev| *prev <= a),
                "decreasing NeTEx times in {}",
                n.id
            );
            flat.extend([a, d]);
        }
        let seq = jp
            .points
            .iter()
            .map(|x| {
                resolved
                    .get(&x.stop)
                    .copied()
                    .with_context(|| format!("unknown scheduled stop {}", x.stop))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut flags: Vec<u8> = jp.points.iter().map(|x| x.flags).collect();
        flags[0] |= FLAG_NO_DROP_OFF;
        *flags.last_mut().unwrap() |= FLAG_NO_PICKUP;
        let line = if n.reference("LineRef").is_empty() {
            jp.line.as_str()
        } else {
            n.reference("LineRef")
        };
        let (label, mut ty) = lines
            .get(line)
            .cloned()
            .unwrap_or_else(|| (line.into(), mode(&n)));
        if !n.value("TransportMode").is_empty() {
            ty = mode(&n);
        }
        if spec.name == "it-trenitalia" && ty == 2 {
            ty = match label.as_str() {
                "FR" | "FA" => 101,
                "FB" | "IC" | "EC" | "EXP" => 102,
                "ICN" | "ECN" | "EN" => 105,
                "RV" | "REG" => 106,
                "MET" | "SFM" => 109,
                _ => ty,
            };
        }
        let key = (line.to_owned(), label.clone(), ty);
        let info = *info_idx.entry(key).or_insert_with(|| {
            let i = route_infos.len() as u32;
            route_infos.push((ns(&spec.name, line), label, ty));
            i
        });
        let key = (seq.clone(), flags.clone(), info);
        let pi = *pattern_idx.entry(key).or_insert_with(|| {
            let i = patterns.len();
            patterns.push(Pattern {
                stops: seq,
                flags,
                tz,
                info,
                trips: Vec::new(),
            });
            i
        });
        let gtfs = gtfs_trip_ids.len() as u32;
        gtfs_trip_ids.push(ns(&spec.name, &n.id));
        report.stop_times += (flat.len() / 2) as u64;
        for (times, days) in time_variants(
            &flat,
            &bits,
            &dates,
            &zone,
            report
                .skipped
                .entry("netex_before_padding_service_days".into())
                .or_default(),
        )? {
            p.max_time = p.max_time.max(*times.last().unwrap());
            patterns[pi].trips.push(Trip {
                times,
                bits: p.bitfield(&days),
                gtfs,
            });
            report.trips += 1;
        }
        Ok(())
    })?;
    report.imported = true;
    report.timezone = timezone;
    report.stops = stops.len() as u64;
    report.routes = patterns.len() as u64;
    report.service_range = range.map(|(a, b)| [a.to_string(), b.to_string()]);
    report.skipped.insert(
        "netex_presentation_and_reservations_not_supported".into(),
        report.trips,
    );
    Ok(FeedData {
        stop_tz: vec![tz; stops.len()],
        report,
        stops,
        change: Vec::new(),
        route_infos,
        gtfs_trip_ids,
        patterns,
        footpaths: Vec::new(),
        forbidden: Vec::new(),
        rules: Vec::new(),
        tz_names: std::mem::take(&mut p.tz_names),
        bitfield_table: std::mem::take(&mut p.bitfield_table),
        max_time: p.max_time,
    })
}
