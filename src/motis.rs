//! MOTIS v6 compatible subset: the endpoints and fields the known MOTIS
//! clients use, so Rustis can replace a MOTIS instance without client changes.
//!
//! * `GET /api/v6/map/stops?min=lat,lon&max=lat,lon`
//! * `GET|POST /api/v6/plan` with `fromPlace`/`toPlace` (stop id or `lat,lon`),
//!   `time`, `numItineraries`, `maxTransfers`, `radius`, `transitModes` and the
//!   POST body `{fromOffsets, toOffsets}` (client-computed first/last mile).
//!
//! Ids: MOTIS writes `<dataset>_<stop id>`, Rustis `<dataset>:<stop id>`.
//! Times are UTC (`…Z`), like MOTIS. Without a street network, coordinates
//! are connected to stops within `radius` by straight-line walks.
use crate::model::{
    Endpoint, Itinerary, Leg, LegKind, MAX_BBOX_RESULTS, MAX_ENDPOINTS, MAX_RESULTS, MAX_TRANSFERS,
    PlanRequest, Stop,
};
use crate::{Error, Snapshot};
use serde::Deserialize;
use serde_json::{Value, json};

/// Default walking radius around coordinates without offsets.
const DEFAULT_RADIUS_M: f64 = 400.0;
const MAX_RADIUS_M: f64 = 5_000.0;
const DEFAULT_ITINERARIES: usize = 5;
const DEFAULT_MAX_TRANSFERS: u8 = 7;
const SEARCH_WINDOW_SECS: u32 = 2 * 3600;
const MAX_TRAVEL_SECS: u32 = 86_400;

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanQuery {
    pub from_place: Option<String>,
    pub to_place: Option<String>,
    pub time: Option<String>,
    pub arrive_by: Option<bool>,
    pub num_itineraries: Option<usize>,
    pub max_transfers: Option<u8>,
    pub radius: Option<f64>,
    /// Comma-separated MOTIS modes.
    pub transit_modes: Option<String>,
    /// Rustis extension: only rides valid with the Deutschlandticket.
    pub deutschlandticket: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanBody {
    #[serde(default)]
    pub from_offsets: Vec<Offset>,
    #[serde(default)]
    pub to_offsets: Vec<Offset>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Offset {
    pub stop_id: String,
    /// Seconds.
    pub duration: u32,
}

#[derive(Debug, Deserialize)]
pub struct StopsQuery {
    pub min: String,
    pub max: String,
}

/// Id translation between MOTIS (`ds_id`) and Rustis (`ds:id`).
struct Ids {
    /// Dataset names, longest first (a name may be a prefix of another).
    feeds: Vec<String>,
}

impl Ids {
    fn new(snap: &Snapshot) -> Self {
        let mut feeds: Vec<String> = snap
            .meta()
            .feeds
            .iter()
            .map(|f| f.name.clone())
            .filter(|n| !n.is_empty())
            .collect();
        feeds.sort_by_key(|n| std::cmp::Reverse(n.len()));
        Self { feeds }
    }
    fn motis_id(&self, id: &str) -> String {
        for f in &self.feeds {
            if let Some(rest) = id
                .strip_prefix(f.as_str())
                .and_then(|r| r.strip_prefix(':'))
            {
                return format!("{f}_{rest}");
            }
        }
        id.to_owned()
    }
    fn rustis_id(&self, id: &str) -> String {
        for f in &self.feeds {
            if let Some(rest) = id
                .strip_prefix(f.as_str())
                .and_then(|r| r.strip_prefix('_'))
            {
                return format!("{f}:{rest}");
            }
        }
        id.to_owned()
    }
}

fn coord(s: &str) -> Option<[f64; 2]> {
    let mut it = s.split(',').map(|v| v.trim().parse::<f64>());
    let lat = it.next()?.ok()?;
    let lon = it.next()?.ok()?;
    // an optional third value is the level
    if it.next().is_some_and(|l| l.is_err()) || it.next().is_some() {
        return None;
    }
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some([lat, lon])
}

fn haversine(a: [f64; 2], b: [f64; 2]) -> f64 {
    let (la1, la2) = (a[0].to_radians(), b[0].to_radians());
    let dla = la2 - la1;
    let dlo = (b[1] - a[1]).to_radians();
    let h = (dla / 2.0).sin().powi(2) + la1.cos() * la2.cos() * (dlo / 2.0).sin().powi(2);
    2.0 * 6_371_008.8 * h.sqrt().asin()
}

/// Google encoded polyline of `[lat, lon]` points.
pub fn polyline(points: &[[f64; 2]], precision: u32) -> String {
    fn enc(out: &mut String, d: i64) {
        let mut v = if d < 0 { !(d << 1) } else { d << 1 } as u64;
        while v >= 0x20 {
            out.push((((v & 0x1f) | 0x20) as u8 + 63) as char);
            v >>= 5;
        }
        out.push((v as u8 + 63) as char);
    }
    let f = 10f64.powi(precision as i32);
    let mut out = String::new();
    let (mut plat, mut plon) = (0i64, 0i64);
    for p in points {
        let (lat, lon) = ((p[0] * f).round() as i64, (p[1] * f).round() as i64);
        enc(&mut out, lat - plat);
        enc(&mut out, lon - plon);
        (plat, plon) = (lat, lon);
    }
    out
}

/// MOTIS `Mode` of a GTFS (extended) route type, following nigiri's classes.
pub fn mode(route_type: u16) -> &'static str {
    match route_type {
        0 | 5 | 900..=999 => "TRAM",
        1 | 401 | 402 => "SUBWAY",
        400 | 403..=499 => "METRO",
        2 | 106 | 100 => "REGIONAL_RAIL",
        101 => "HIGHSPEED_RAIL",
        102..=104 => "LONG_DISTANCE",
        105 => "NIGHT_RAIL",
        107 | 108 | 110..=199 => "REGIONAL_RAIL",
        109 => "SUBURBAN",
        3 | 11 | 700..=799 | 800..=899 => "BUS",
        200..=299 => "COACH",
        4 | 1000..=1099 | 1200..=1299 => "FERRY",
        1100..=1199 => "AIRPLANE",
        6 | 1300..=1399 => "AERIAL_LIFT",
        7 | 1400..=1499 => "FUNICULAR",
        12 => "RAIL",
        _ => "OTHER",
    }
}

pub(crate) const RAIL: [&str; 7] = [
    "RAIL",
    "HIGHSPEED_RAIL",
    "LONG_DISTANCE",
    "NIGHT_RAIL",
    "REGIONAL_FAST_RAIL",
    "REGIONAL_RAIL",
    "SUBURBAN",
];

/// Whether `m` is allowed by a MOTIS `transitModes` list.
fn allowed(modes: &[String], m: &str) -> bool {
    modes.iter().any(|x| {
        x == m
            || x == "TRANSIT"
            || (x == "RAIL" && RAIL.contains(&m))
            || (x == "CABLE_CAR" && m == "AERIAL_LIFT")
    })
}

fn utc(rfc3339: &str) -> String {
    rfc3339
        .parse::<jiff::Timestamp>()
        .map(|t| t.strftime("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

fn coord_of(s: &Stop) -> Option<[f64; 2]> {
    Some([s.lat?, s.lon?])
}

/// Origin/destination of one side: offsets, a stop id or coordinates.
fn endpoints(
    snap: &Snapshot,
    ids: &Ids,
    place: Option<&str>,
    offsets: &[Offset],
    radius: f64,
    side: &str,
) -> Result<(Vec<Endpoint>, Option<[f64; 2]>), Error> {
    let place = place.unwrap_or("");
    let point = coord(place);
    let mut eps: Vec<Endpoint> = if !offsets.is_empty() {
        offsets
            .iter()
            .map(|o| Endpoint {
                stop_id: ids.rustis_id(&o.stop_id),
                walk_seconds: o.duration,
            })
            .filter(|e| snap.stop_timezone(&e.stop_id).is_some())
            .collect()
    } else if let Some(p) = point {
        let dlat = radius / 111_320.0;
        let dlon = radius / (111_320.0 * p[0].to_radians().cos().max(0.01));
        let speed = snap.walk_speed();
        let mut near: Vec<(f64, Stop)> = snap
            .stops_in_bbox(
                [p[0] - dlat, (p[1] - dlon).max(-180.0)],
                [p[0] + dlat, (p[1] + dlon).min(180.0)],
                MAX_BBOX_RESULTS,
                true,
            )?
            .into_iter()
            .filter_map(|s| Some((haversine(p, coord_of(&s)?), s)))
            .filter(|(d, _)| *d <= radius)
            .collect();
        near.sort_by(|a, b| a.0.total_cmp(&b.0));
        near.into_iter()
            .map(|(d, s)| Endpoint {
                stop_id: s.stop_id,
                walk_seconds: (d / speed).ceil() as u32,
            })
            .collect()
    } else if !place.is_empty() {
        // a stop stands for its whole station (platforms, duplicate feeds),
        // like MOTIS' station equivalence
        let id = ids.rustis_id(place);
        let Some(st) = snap.station(&id) else {
            return Err(Error::invalid(format!("{side}Place: unknown stop {place}")));
        };
        let tt = snap.tt();
        let mut v: Vec<Endpoint> =
            std::iter::once(id.clone())
                .chain(st.stops.iter().map(|&s| {
                    String::from_utf8_lossy(tt.catalog.id_bytes(s as usize)).into_owned()
                }))
                .collect::<Vec<_>>()
                .into_iter()
                .map(|stop_id| Endpoint {
                    stop_id,
                    walk_seconds: 0,
                })
                .collect();
        v.dedup_by(|a, b| a.stop_id == b.stop_id);
        v
    } else {
        return Err(Error::invalid(format!("{side}Place is required")));
    };
    eps.sort_by_key(|e| e.walk_seconds);
    eps.truncate(MAX_ENDPOINTS);
    Ok((eps, point))
}

fn place_json(
    ids: &Ids,
    stop: Option<&Stop>,
    name: &str,
    at: Option<[f64; 2]>,
    arrival: Option<&str>,
    departure: Option<&str>,
) -> Value {
    let c = stop.and_then(coord_of).or(at).unwrap_or([0.0, 0.0]);
    let mut v = json!({
        "name": stop.map_or(name, |s| s.name.as_str()),
        "lat": c[0],
        "lon": c[1],
        "vertexType": if stop.is_some() { "TRANSIT" } else { "NORMAL" },
    });
    if let Some(s) = stop {
        v["stopId"] = json!(ids.motis_id(&s.stop_id));
        if let Some(p) = &s.parent_station {
            v["parentId"] = json!(ids.motis_id(p));
        }
    }
    if let Some(a) = arrival {
        v["arrival"] = json!(a);
        v["scheduledArrival"] = json!(a);
    }
    if let Some(d) = departure {
        v["departure"] = json!(d);
        v["scheduledDeparture"] = json!(d);
    }
    v
}

fn leg_json(ids: &Ids, l: &Leg, start: Option<[f64; 2]>, end: Option<[f64; 2]>) -> Value {
    let (dep, arr) = (utc(&l.departure_time), utc(&l.arrival_time));
    let transit = l.kind == LegKind::Transit;
    let mode = if l.mode.as_deref() == Some("flex") {
        "FLEX"
    } else if transit {
        mode(l.route_type.unwrap_or(u16::MAX))
    } else {
        "WALK"
    };
    let from = match &l.from {
        Some(s) => place_json(ids, Some(s), "", None, None, Some(&dep)),
        None => place_json(ids, None, "START", start, None, Some(&dep)),
    };
    let to = match &l.to {
        Some(s) => place_json(ids, Some(s), "", None, Some(&arr), None),
        None => place_json(ids, None, "END", end, Some(&arr), None),
    };
    let mut geometry = l.geometry.clone();
    if !transit {
        // walks: straight line between the leg's ends (incl. the query points)
        geometry = [
            from["lat"].as_f64().zip(from["lon"].as_f64()),
            to["lat"].as_f64().zip(to["lon"].as_f64()),
        ]
        .into_iter()
        .flatten()
        .map(|(a, b)| [a, b])
        .collect();
    }
    let mut v = json!({
        "mode": mode,
        "from": from,
        "to": to,
        "duration": l.duration_seconds,
        "startTime": dep,
        "endTime": arr,
        "scheduledStartTime": dep,
        "scheduledEndTime": arr,
        "realTime": false,
        "scheduled": transit,
        "cancelled": false,
        "legGeometry": {
            "points": polyline(&geometry, 6),
            "precision": 6,
            "length": geometry.len(),
        },
    });
    if let Some(info) = &l.booking_info {
        v["bookingRequired"] = json!(true);
        v["bookingInfo"] = json!(info);
        v["warnings"] = json!([format!("BOOKING: {info}")]);
    }
    if transit {
        let line = l.line.clone().unwrap_or_default();
        v["interlineWithPreviousLeg"] = json!(false);
        v["headsign"] = json!(l.headsign);
        v["routeShortName"] = json!(line);
        v["displayName"] = json!(l.display_name.as_deref().unwrap_or(&line));
        if let Some(t) = &l.trip_short_name {
            v["tripShortName"] = json!(t);
        }
        if let Some(d) = l.deutschlandticket {
            v["deutschlandticket"] = json!(d);
        }
        v["routeType"] = json!(l.route_type);
        v["tripId"] = json!(l.trip_id.as_deref().map(|t| match &l.trip_ref {
            Some(r) => format!("{r}_{}", ids.motis_id(t)),
            None => ids.motis_id(t),
        }));
        v["routeId"] = json!(l.route_id.as_deref().map(|t| ids.motis_id(t)));
        if let Some(a) = &l.agency {
            v["agencyName"] = json!(a);
        }
        if let Some(c) = &l.color {
            v["routeColor"] = json!(c);
        }
        v["intermediateStops"] = Value::Array(
            l.intermediate_stops
                .iter()
                .map(|s| place_json(ids, Some(s), "", None, None, None))
                .collect(),
        );
    }
    v
}

fn itinerary_json(
    ids: &Ids,
    it: &Itinerary,
    start: Option<[f64; 2]>,
    end: Option<[f64; 2]>,
) -> Value {
    json!({
        "duration": it.duration_seconds,
        "startTime": utc(&it.departure_time),
        "endTime": utc(&it.arrival_time),
        "transfers": it.transfers,
        // Rustis extension: every ride is valid with the Deutschlandticket
        "deutschlandticket": it.legs.iter().filter(|l| l.kind == LegKind::Transit)
            .all(|l| l.deutschlandticket == Some(true)),
        "legs": merge_walks(&it.legs).iter().map(|l| leg_json(ids, l, start, end)).collect::<Vec<_>>(),
    })
}

/// Consecutive walks (access + footpath, footpath + egress) become one WALK leg,
/// as in MOTIS.
fn merge_walks(legs: &[Leg]) -> Vec<Leg> {
    let mut out: Vec<Leg> = Vec::with_capacity(legs.len());
    for l in legs {
        match out.last_mut() {
            Some(p) if p.kind != LegKind::Transit && l.kind != LegKind::Transit => {
                p.to = l.to.clone();
                p.arrival = l.arrival;
                p.arrival_time = l.arrival_time.clone();
                p.duration_seconds += l.duration_seconds;
                if l.kind == LegKind::Egress {
                    p.kind = LegKind::Egress;
                }
            }
            _ => out.push(l.clone()),
        }
    }
    out
}

/// `/api/v6/plan`.
pub fn plan(snap: &Snapshot, q: &PlanQuery, body: &PlanBody) -> Result<Value, Error> {
    let ids = Ids::new(snap);
    let radius = q.radius.unwrap_or(DEFAULT_RADIUS_M);
    if !(radius.is_finite() && radius > 0.0 && radius <= MAX_RADIUS_M) {
        return Err(Error::invalid(format!(
            "radius must be in (0, {MAX_RADIUS_M}]"
        )));
    }
    let (from, start) = endpoints(
        snap,
        &ids,
        q.from_place.as_deref(),
        &body.from_offsets,
        radius,
        "from",
    )?;
    let (to, end) = endpoints(
        snap,
        &ids,
        q.to_place.as_deref(),
        &body.to_offsets,
        radius,
        "to",
    )?;
    let time = match &q.time {
        Some(t) => t
            .parse::<jiff::Timestamp>()
            .map_err(|e| Error::invalid(format!("time: {e}")))?,
        None => jiff::Timestamp::now(),
    };
    let modes: Option<Vec<String>> = q.transit_modes.as_deref().map(|m| {
        m.split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect()
    });
    let wanted = q
        .num_itineraries
        .unwrap_or(DEFAULT_ITINERARIES)
        .clamp(1, MAX_RESULTS);

    let mut out = json!({
        "requestParameters": {},
        "debugOutput": {},
        "from": place_json(&ids, None, "START", start, None, None),
        "to": place_json(&ids, None, "END", end, None, None),
        "direct": [],
        "itineraries": [],
        "previousPageCursor": "",
        "nextPageCursor": "",
    });
    if from.is_empty() || to.is_empty() {
        return Ok(out);
    }
    let tz_name = snap
        .stop_timezone(&from[0].stop_id)
        .ok_or_else(|| Error::invalid("unknown origin stop"))?;
    let tz = jiff::tz::TimeZone::get(tz_name).map_err(|e| Error::invalid(e.to_string()))?;
    let allow_flex = modes
        .as_ref()
        .is_some_and(|m| m.iter().any(|x| x == "FLEX" || x == "ODM"));
    // MOTIS applies transitModes in the search: a journey restricted to
    // regional trains is found even if a faster ICE journey exists
    let type_ok = |t: u16| modes.as_ref().is_none_or(|m| allowed(m, mode(t)));
    // one forward search from `time`, arriving within `max_travel`
    let run = |time: jiff::Timestamp, arrive_by: bool| -> Result<Vec<Itinerary>, Error> {
        let local = time.to_zoned(tz.clone());
        let midnight = local
            .date()
            .at(0, 0, 0, 0)
            .to_zoned(tz.clone())
            .map_err(|e| Error::invalid(e.to_string()))?;
        let departure = (time.as_second() - midnight.timestamp().as_second()).max(0) as u32;
        let req = PlanRequest {
            date: local.date().to_string(),
            departure,
            from: from.clone(),
            to: to.clone(),
            max_transfers: q
                .max_transfers
                .unwrap_or(DEFAULT_MAX_TRANSFERS)
                .min(MAX_TRANSFERS),
            max_results: wanted,
            max_travel_seconds: MAX_TRAVEL_SECS,
            alternatives_within_seconds: SEARCH_WINDOW_SECS,
        };
        let resp = snap.plan_with_modes(
            &req,
            allow_flex,
            modes.is_some().then_some(&type_ok as &dyn Fn(u16) -> bool),
            q.deutschlandticket.unwrap_or(false),
            arrive_by,
        )?;
        Ok(resp
            .itineraries
            .into_iter()
            // walking alone belongs to `direct` (directModes), never to `itineraries`
            .filter(|it| it.legs.iter().any(|l| l.kind == LegKind::Transit))
            .filter(|it| {
                (!it.legs.iter().any(|l| l.mode.as_deref() == Some("flex")) || allow_flex)
                    && modes.as_ref().is_none_or(|m| {
                        it.legs
                            .iter()
                            .filter(|l| l.kind == LegKind::Transit)
                            .all(|l| {
                                if l.mode.as_deref() == Some("flex") {
                                    allow_flex
                                } else {
                                    allowed(m, mode(l.route_type.unwrap_or(u16::MAX)))
                                }
                            })
                    })
            })
            .collect())
    };
    // arriveBy: `time` is the deadline; the journeys leaving latest
    let mut sorted = run(time, q.arrive_by == Some(true))?;
    // MOTIS lists the journeys of the window by departure
    sorted.sort_by_key(|it| (unix(&it.departure_time), unix(&it.arrival_time)));
    let its: Vec<Value> = sorted
        .iter()
        .take(wanted)
        .map(|it| itinerary_json(&ids, it, start, end))
        .collect();
    out["itineraries"] = Value::Array(its);
    Ok(out)
}

fn unix(rfc3339: &str) -> i64 {
    rfc3339
        .parse::<jiff::Timestamp>()
        .map_or(0, |t| t.as_second())
}

/// `/api/v6/map/stops`: served stops inside the box.
pub fn stops(snap: &Snapshot, q: &StopsQuery) -> Result<Value, Error> {
    let (Some(min), Some(max)) = (coord(&q.min), coord(&q.max)) else {
        return Err(Error::invalid("min and max must be \"lat,lon\""));
    };
    let ids = Ids::new(snap);
    let stops = snap.stops_in_bbox(
        [min[0].min(max[0]), min[1].min(max[1])],
        [min[0].max(max[0]), min[1].max(max[1])],
        MAX_BBOX_RESULTS,
        true,
    )?;
    Ok(Value::Array(
        stops
            .iter()
            .map(|s| place_json(&ids, Some(s), "", None, None, None))
            .collect(),
    ))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeocodeQuery {
    pub text: String,
    /// Number of matches (default 10, at most 50).
    pub limit: Option<usize>,
    /// Only `STOP` is supported; other types give no results.
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// Only stations served by one of these modes (comma list, e.g. `RAIL`).
    pub mode: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StopTimesQuery {
    pub stop_id: String,
    pub time: Option<String>,
    /// Number of events (default 10, at most 200).
    pub n: Option<usize>,
    /// `true`: arrivals instead of departures.
    pub arrive_by: Option<bool>,
    /// Comma-separated MOTIS modes (e.g. `RAIL`); default all.
    pub mode: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TripQuery {
    pub trip_id: String,
    /// Service date for a bare GTFS trip id (default: today).
    pub date: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TripByNameQuery {
    pub name: String,
    /// Service date `YYYY-MM-DD` (default: today, Europe/Berlin).
    pub date: Option<String>,
    /// Only trips calling at this stop/station.
    pub stop_id: Option<String>,
    /// At most this many trips (default 10, at most 50).
    pub limit: Option<usize>,
}

fn ts(snap: &Snapshot, rel: u32) -> String {
    jiff::Timestamp::from_second(snap.unix(rel))
        .map(|t| t.strftime("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

fn query_time(snap: &Snapshot, time: Option<&str>) -> Result<u32, Error> {
    let t = match time {
        Some(t) => t
            .parse::<jiff::Timestamp>()
            .map_err(|e| Error::invalid(format!("time: {e}")))?,
        None => jiff::Timestamp::now(),
    };
    snap.rel(t.as_second())
        .ok_or_else(|| Error::invalid("time is outside the snapshot horizon"))
}

fn service_date(date: Option<&str>) -> Result<jiff::civil::Date, Error> {
    match date {
        Some(d) => jiff::civil::Date::strptime("%Y-%m-%d", d)
            .map_err(|_| Error::invalid("date must be YYYY-MM-DD")),
        None => Ok(jiff::Timestamp::now()
            .to_zoned(jiff::tz::TimeZone::get("Europe/Berlin").unwrap_or(jiff::tz::TimeZone::UTC))
            .date()),
    }
}

/// A stop as MOTIS place, with times (relative seconds) and track.
fn stop_place(snap: &Snapshot, ids: &Ids, s: u32, arr: Option<u32>, dep: Option<u32>) -> Value {
    let tt = snap.tt();
    let stop = tt.catalog.stop(s as usize, tt.served(s as usize));
    let (a, d) = (arr.map(|t| ts(snap, t)), dep.map(|t| ts(snap, t)));
    let mut v = place_json(ids, Some(&stop), "", None, a.as_deref(), d.as_deref());
    let track = tt.stop_platform.get(s as usize);
    if !track.is_empty() {
        v["track"] = json!(track);
        v["scheduledTrack"] = json!(track);
    }
    v
}

fn station_place(snap: &Snapshot, ids: &Ids, st: &crate::board::Station) -> Value {
    let tt = snap.tt();
    let mut v = stop_place(snap, ids, st.rep, None, None);
    if v["lat"] == json!(0.0)
        && v["lon"] == json!(0.0)
        && let Some(c) = st.stops.iter().find_map(|&s| tt.catalog.coord(s as usize))
    {
        v["lat"] = json!(c[0]);
        v["lon"] = json!(c[1]);
    }
    v
}

/// Trip attributes shared by board entries, trips and name matches.
fn trip_fields(snap: &Snapshot, ids: &Ids, at: &crate::board::TripRef, v: &mut Value) {
    let tt = snap.tt();
    let r = at.route as usize;
    let info = tt.route_info[r] as usize;
    let g = at.gtfs(tt);
    let line = tt.route_info_line.get(info);
    let display = tt.gtfs_trip_display.get(g);
    let short = tt.gtfs_trip_short.get(g);
    let h = tt.gtfs_trip_headsign[g] as usize;
    let last = tt.n_route_stops(r) - 1;
    let headsign = if h != 0 {
        tt.headsigns.get(h).to_owned()
    } else {
        tt.catalog.name(tt.route_stop(r, last) as usize)
    };
    v["mode"] = json!(mode(tt.route_info_type[info]));
    v["realTime"] = json!(false);
    v["headsign"] = json!(headsign);
    v["routeShortName"] = json!(line);
    v["displayName"] = json!(if display.is_empty() { line } else { display });
    if !short.is_empty() {
        v["tripShortName"] = json!(short);
    }
    v["routeType"] = json!(tt.route_info_type[info]);
    v["routeId"] = json!(ids.motis_id(tt.route_info_id.get(info)));
    v["tripId"] = json!(format!(
        "{}_{}",
        snap.trip_ref(at),
        ids.motis_id(tt.gtfs_trip_id.get(g))
    ));
    let agency = tt.route_info_agency.get(info);
    if !agency.is_empty() {
        v["agencyName"] = json!(agency);
    }
    let color = tt.route_info_color.get(info);
    if !color.is_empty() {
        v["routeColor"] = json!(color);
    }
    v["cancelled"] = json!(false);
    v["tripCancelled"] = json!(false);
    v["source"] = json!("");
}

/// `GET /api/v1/geocode?text=…`: stations only (one per station).
pub fn geocode(snap: &Snapshot, q: &GeocodeQuery) -> Result<Value, Error> {
    let ids = Ids::new(snap);
    let limit = q.limit.unwrap_or(10).clamp(1, 50);
    if q.kind
        .as_deref()
        .is_some_and(|k| !k.eq_ignore_ascii_case("STOP"))
    {
        return Ok(Value::Array(Vec::new()));
    }
    let modes = match &q.mode {
        None => u32::MAX,
        Some(m) => {
            let list: Vec<String> = m.split(',').map(|x| x.trim().to_uppercase()).collect();
            crate::search::mode_mask(|x| allowed(&list, x))
        }
    };
    Ok(Value::Array(
        snap.geocode(&q.text, limit, modes)
            .iter()
            .map(|(st, score)| {
                let p = station_place(snap, &ids, st);
                let areas: Vec<Value> = st
                    .area
                    .iter()
                    .map(|a| json!({"name": a, "adminLevel": 8, "matched": false, "default": true}))
                    .collect();
                json!({
                    "type": "STOP",
                    "id": p["stopId"],
                    "name": p["name"],
                    "lat": p["lat"],
                    "lon": p["lon"],
                    "level": 0.0,
                    "score": score,
                    "tokens": [],
                    "areas": areas,
                })
            })
            .collect(),
    ))
}

fn resolve_station(snap: &Snapshot, ids: &Ids, id: &str) -> Result<crate::board::Station, Error> {
    snap.station(&ids.rustis_id(id))
        .ok_or_else(|| Error::invalid(format!("unknown stop {id}")))
}

/// `GET /api/v6/stoptimes?stopId=…&time=…&n=…[&arriveBy=true]`: departures
/// (arrivals) of the whole station the stop belongs to; schedule only.
pub fn stoptimes(snap: &Snapshot, q: &StopTimesQuery) -> Result<Value, Error> {
    let ids = Ids::new(snap);
    let st = resolve_station(snap, &ids, &q.stop_id)?;
    let t = query_time(snap, q.time.as_deref())?;
    let n = q.n.unwrap_or(10).clamp(1, 200);
    let arrivals = q.arrive_by == Some(true);
    let tt = snap.tt();
    // the same ride from duplicate feeds is shown once
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let modes: Option<Vec<String>> = q.mode.as_deref().map(|m| {
        m.split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect()
    });
    let type_ok = |t: u16| modes.as_ref().is_none_or(|m| allowed(m, mode(t)));
    for e in snap.board(&st.stops, t, 2 * n, arrivals, &type_ok) {
        let (a, d) = e.at.times(tt, e.pos as usize);
        let last = tt.n_route_stops(e.at.route as usize) - 1;
        let mut v = json!({});
        trip_fields(snap, &ids, &e.at, &mut v);
        if !seen.insert((
            e.time,
            v["displayName"].to_string(),
            v["headsign"].to_string(),
        )) {
            continue;
        }
        let first = e.pos == 0;
        let is_last = e.pos as usize == last;
        v["place"] = stop_place(
            snap,
            &ids,
            e.stop,
            (!first).then_some(a),
            (!is_last).then_some(d),
        );
        v["pickupDropoffType"] = json!("NORMAL");
        let (end_arr, _) = e.at.times(tt, last);
        v["tripTo"] = stop_place(
            snap,
            &ids,
            tt.route_stop(e.at.route as usize, last),
            Some(end_arr),
            None,
        );
        let dt = snap.dticket();
        v["deutschlandticket"] = json!(if arrivals {
            dt.allows_ride(e.at.route, e.pos as usize - 1, e.pos as usize)
        } else {
            dt.allows_ride(e.at.route, e.pos as usize, e.pos as usize + 1)
        });
        out.push(v);
        if out.len() == n {
            break;
        }
    }
    Ok(json!({
        "place": station_place(snap, &ids, &st),
        "stopTimes": out,
        "previousPageCursor": "",
        "nextPageCursor": "",
    }))
}

/// Splits `YYYYMMDD_HH:MM_<id>` (MOTIS trip id); a bare id has no date/time.
fn parse_trip_id(id: &str) -> (Option<jiff::civil::Date>, Option<u32>, &str) {
    let mut it = id.splitn(3, '_');
    if let (Some(d), Some(hm), Some(rest)) = (it.next(), it.next(), it.next())
        && d.len() == 8
        && let Ok(date) = jiff::civil::Date::strptime("%Y%m%d", d)
        && let Some((h, m)) = hm.split_once(':')
        && let (Ok(h), Ok(m)) = (h.parse::<u32>(), m.parse::<u32>())
    {
        return (Some(date), Some(h * 60 + m), rest);
    }
    (None, None, id)
}

/// `GET /api/v6/trip?tripId=…`: the whole trip as a one-leg itinerary.
pub fn trip(snap: &Snapshot, q: &TripQuery) -> Result<Value, Error> {
    let ids = Ids::new(snap);
    let (date, hhmm, gtfs) = parse_trip_id(&q.trip_id);
    let date = match date {
        Some(d) => d,
        None => service_date(q.date.as_deref())?,
    };
    let at = snap
        .find_trip(&ids.rustis_id(gtfs), date, hhmm)
        .ok_or_else(|| Error::invalid(format!("unknown trip {} on {date}", q.trip_id)))?;
    let tt = snap.tt();
    let r = at.route as usize;
    let last = tt.n_route_stops(r) - 1;
    let (_, dep) = at.times(tt, 0);
    let (arr, _) = at.times(tt, last);
    let stops: Vec<Value> = (0..=last)
        .map(|p| {
            let (a, d) = at.times(tt, p);
            stop_place(
                snap,
                &ids,
                tt.route_stop(r, p),
                (p > 0).then_some(a),
                (p < last).then_some(d),
            )
        })
        .collect();
    let geometry: Vec<[f64; 2]> = (0..=last)
        .filter_map(|p| tt.catalog.coord(tt.route_stop(r, p) as usize))
        .collect();
    let mut leg = json!({
        "from": stops[0],
        "to": stops[last],
        "duration": arr - dep,
        "startTime": ts(snap, dep),
        "endTime": ts(snap, arr),
        "scheduledStartTime": ts(snap, dep),
        "scheduledEndTime": ts(snap, arr),
        "scheduled": true,
        "interlineWithPreviousLeg": false,
        "intermediateStops": stops[1..last],
        "legGeometry": {
            "points": polyline(&geometry, 6),
            "precision": 6,
            "length": geometry.len(),
        },
        "deutschlandticket": snap.dticket().allows_ride(at.route, 0, last),
    });
    trip_fields(snap, &ids, &at, &mut leg);
    Ok(json!({
        "duration": arr - dep,
        "startTime": ts(snap, dep),
        "endTime": ts(snap, arr),
        "transfers": 0,
        "legs": [leg],
    }))
}

/// `GET /api/v1/trip-by-name?name=ICE%20930&date=…[&stopId=…]` (Rustis
/// extension): trips of the service date by train name or number.
pub fn trip_by_name(snap: &Snapshot, q: &TripByNameQuery) -> Result<Value, Error> {
    let ids = Ids::new(snap);
    if crate::search::name_key(&q.name).is_empty() {
        return Err(Error::invalid("name must contain letters or digits"));
    }
    let date = service_date(q.date.as_deref())?;
    let station = match &q.stop_id {
        Some(id) => Some(resolve_station(snap, &ids, id)?),
        None => None,
    };
    let limit = q.limit.unwrap_or(10).clamp(1, 50);
    let tt = snap.tt();
    let trips = snap.trips_by_name(
        &q.name,
        date,
        station.as_ref().map(|s| s.stops.as_slice()),
        limit,
    );
    Ok(json!({
        "trips": trips.iter().map(|(at, pos)| {
            let r = at.route as usize;
            let last = tt.n_route_stops(r) - 1;
            let mut v = json!({
                "serviceDate": date.to_string(),
                "from": stop_place(snap, &ids, tt.route_stop(r, 0), None, Some(at.times(tt, 0).1)),
                "to": stop_place(snap, &ids, tt.route_stop(r, last), Some(at.times(tt, last).0), None),
            });
            if let Some(p) = pos {
                let p = *p as usize;
                let (a, d) = at.times(tt, p);
                v["stop"] = stop_place(snap, &ids, tt.route_stop(r, p), (p > 0).then_some(a), (p < last).then_some(d));
            }
            trip_fields(snap, &ids, at, &mut v);
            v
        }).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn polyline_matches_reference() {
        // Google's documented example, precision 5
        let pts = [[38.5, -120.2], [40.7, -120.95], [43.252, -126.453]];
        assert_eq!(polyline(&pts, 5), "_p~iF~ps|U_ulLnnqC_mqNvxq`@");
    }

    #[test]
    fn coordinates_and_modes() {
        assert_eq!(coord("48.2,16.37"), Some([48.2, 16.37]));
        assert_eq!(coord("48.2,16.37,0"), Some([48.2, 16.37]));
        assert_eq!(coord("de-VBB_de:11000:900003201"), None);
        assert_eq!(mode(109), "SUBURBAN");
        assert!(allowed(&["RAIL".into()], "REGIONAL_RAIL"));
        assert!(!allowed(&["BUS".into(), "TRAM".into()], "FERRY"));
    }
}
