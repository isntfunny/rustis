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

const RAIL: [&str; 7] = [
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
        let id = ids.rustis_id(place);
        if snap.stop_timezone(&id).is_none() {
            return Err(Error::invalid(format!("{side}Place: unknown stop {place}")));
        }
        vec![Endpoint {
            stop_id: id,
            walk_seconds: 0,
        }]
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
        v["displayName"] = json!(line);
        v["routeType"] = json!(l.route_type);
        v["tripId"] = json!(l.trip_id.as_deref().map(|t| ids.motis_id(t)));
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
    if q.arrive_by == Some(true) {
        return Err(Error::invalid("arriveBy=true is not supported"));
    }
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
    let local = time.to_zoned(tz.clone());
    let midnight = local
        .date()
        .at(0, 0, 0, 0)
        .to_zoned(tz)
        .map_err(|e| Error::invalid(e.to_string()))?;
    let departure = (time.as_second() - midnight.timestamp().as_second()).max(0) as u32;
    let req = PlanRequest {
        date: local.date().to_string(),
        departure,
        from,
        to,
        max_transfers: q
            .max_transfers
            .unwrap_or(DEFAULT_MAX_TRANSFERS)
            .min(MAX_TRANSFERS),
        max_results: if modes.is_some() { MAX_RESULTS } else { wanted },
        max_travel_seconds: MAX_TRAVEL_SECS,
        alternatives_within_seconds: SEARCH_WINDOW_SECS,
    };
    let allow_flex = modes
        .as_ref()
        .is_some_and(|m| m.iter().any(|x| x == "FLEX" || x == "ODM"));
    let resp = snap.plan_with_flex(&req, allow_flex)?;
    let mut sorted: Vec<&Itinerary> = resp.itineraries.iter().collect();
    // MOTIS lists the journeys of the window by departure
    sorted.sort_by_key(|it| (it.departure, it.arrival));
    let its: Vec<Value> = sorted
        .into_iter()
        // walking alone belongs to `direct` (directModes), never to `itineraries`
        .filter(|it| it.legs.iter().any(|l| l.kind == LegKind::Transit))
        .filter(|it| {
            (!it.legs.iter().any(|l| l.mode.as_deref() == Some("flex"))
                || modes
                    .as_ref()
                    .is_some_and(|m| m.iter().any(|x| x == "FLEX" || x == "ODM")))
                && modes.as_ref().is_none_or(|m| {
                    it.legs
                        .iter()
                        .filter(|l| l.kind == LegKind::Transit)
                        .all(|l| {
                            if l.mode.as_deref() == Some("flex") {
                                m.iter().any(|x| x == "FLEX" || x == "ODM")
                            } else {
                                allowed(m, mode(l.route_type.unwrap_or(u16::MAX)))
                            }
                        })
                })
        })
        .take(wanted)
        .map(|it| itinerary_json(&ids, it, start, end))
        .collect();
    out["itineraries"] = Value::Array(its);
    Ok(out)
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
