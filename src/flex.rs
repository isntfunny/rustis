//! Conservative stop-based GTFS-Flex model. Times are GTFS-local seconds.
use crate::{
    feed::{Feed, field, text},
    model::Stop,
};
use anyhow::Result;
use std::collections::HashMap;

/// Straight-line maximum × 1.5 detour at 30 km/h, plus five minutes.
/// This is an estimate, not an operator travel-time guarantee.
pub const DETOUR: f64 = 1.5;
pub const SPEED: f64 = 30.0 / 3.6;
pub const BUFFER: u32 = 300;
/// Packed snapshot row: pickup start/end, dropoff start/end, duration,
/// calendar bitfield, timezone, route-info, pickup area, dropoff area, GTFS trip context.
pub const WIDTH: usize = 11;
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Service {
    pub pickup: Vec<u32>,
    pub dropoff: Vec<u32>,
    pub start: u32,
    pub end: u32,
    pub arrival_start: u32,
    pub arrival_end: u32,
    pub duration: u32,
    pub bits: u32,
    pub tz: u16,
    pub info: u32,
    pub gtfs: u32,
    pub booking: String,
}
pub(crate) fn areas(
    feed: &mut Feed,
    stops: &[Stop],
    ids: &HashMap<Box<[u8]>, u32>,
) -> Result<HashMap<String, Vec<u32>>> {
    let mut areas: HashMap<String, Vec<u32>> = HashMap::new();
    feed.rows("location_groups.txt", |c, r| {
        areas
            .entry(format!("g:{}", text(r, c.get("location_group_id"))))
            .or_default();
        Ok(())
    })?;
    feed.rows("location_group_stops.txt", |c, r| {
        if let Some(&s) = ids.get(field(r, c.get("stop_id")))
            && let Some(a) = areas.get_mut(&format!("g:{}", text(r, c.get("location_group_id"))))
        {
            a.push(s);
        }
        Ok(())
    })?;
    if let Some(bytes) = feed.bytes("locations.geojson")? {
        let json: serde_json::Value = serde_json::from_slice(&bytes)?;
        if let Some(features) = json["features"].as_array() {
            for f in features {
                let Some(id) = f["id"].as_str() else { continue };
                let g = &f["geometry"];
                let polygons: Vec<&serde_json::Value> = match g["type"].as_str() {
                    Some("Polygon") => vec![&g["coordinates"]],
                    Some("MultiPolygon") => g["coordinates"]
                        .as_array()
                        .map(|a| a.iter().collect())
                        .unwrap_or_default(),
                    _ => continue,
                };
                let mut members = Vec::new();
                for (s, stop) in stops.iter().enumerate() {
                    if let Some(p) = stop.lon.zip(stop.lat)
                        && polygons.iter().any(|poly| polygon(poly, p))
                    {
                        members.push(s as u32);
                    }
                }
                areas.insert(format!("z:{id}"), members);
            }
        }
    }
    for a in areas.values_mut() {
        a.sort_unstable();
        a.dedup();
    }
    Ok(areas)
}
fn ring(r: &serde_json::Value, p: (f64, f64)) -> bool {
    let Some(v) = r.as_array() else { return false };
    let pts: Option<Vec<(f64, f64)>> = v
        .iter()
        .map(|x| Some((x[0].as_f64()?, x[1].as_f64()?)))
        .collect();
    let Some(pts) = pts else { return false };
    if pts.len() < 4 {
        return false;
    }
    let mut inside = false;
    for w in pts.windows(2) {
        let ((x, y), (a, b)) = (w[0], w[1]);
        if (y > p.1) != (b > p.1) && p.0 < (a - x) * (p.1 - y) / (b - y) + x {
            inside = !inside;
        }
    }
    inside
}
fn polygon(poly: &serde_json::Value, p: (f64, f64)) -> bool {
    poly.as_array().is_some_and(|rings| {
        !rings.is_empty() && ring(&rings[0], p) && !rings[1..].iter().any(|r| ring(r, p))
    })
}
pub(crate) fn bookings(feed: &mut Feed) -> Result<HashMap<String, String>> {
    let mut b = HashMap::new();
    feed.rows("booking_rules.txt", |c, r| {
        let fields = [
            "booking_type",
            "prior_notice_duration_min",
            "prior_notice_duration_max",
            "prior_notice_last_day",
            "prior_notice_last_time",
            "prior_notice_start_day",
            "prior_notice_start_time",
            "phone_number",
            "booking_url",
            "message",
            "pickup_message",
            "drop_off_message",
        ];
        let desc = fields
            .iter()
            .filter_map(|k| {
                let v = text(r, c.get(k));
                (!v.is_empty()).then(|| format!("{k}={v}"))
            })
            .collect::<Vec<_>>()
            .join("; ");
        b.insert(text(r, c.get("booking_rule_id")), desc);
        Ok(())
    })?;
    Ok(b)
}
pub(crate) fn duration(a: &[u32], b: &[u32], stops: &[Stop]) -> Option<u32> {
    let mut max: f64 = 0.0;
    for &s in a {
        for &t in b {
            let x = &stops[s as usize];
            let y = &stops[t as usize];
            max = max.max(crate::build::haversine(
                x.lat.zip(x.lon)?,
                y.lat.zip(y.lon)?,
            ));
        }
    }
    Some((max * DETOUR / SPEED).ceil() as u32 + BUFFER)
}
