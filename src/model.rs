//! Public request/response types and persisted metadata.
use serde::{Deserialize, Serialize};

/// Hard request bounds (per-query scratch grows with the explored region and
/// the number of rounds, so these bounds are part of the RAM budget).
pub const MAX_ENDPOINTS: usize = 64;
pub const MAX_TRANSFERS: u8 = 8;
pub const MAX_RESULTS: usize = 10;
pub const MAX_BBOX_RESULTS: usize = 1000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stop {
    pub stop_id: String,
    pub name: String,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub parent_station: Option<String>,
    /// Whether any trip of the snapshot horizon stops here (the catalogue
    /// also lists unserved stops, stations and entrances).
    #[serde(default)]
    pub served: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub stop_id: String,
    /// Walking time between the rider's real position and this stop.
    #[serde(default)]
    pub walk_seconds: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanRequest {
    /// Local date (`YYYY-MM-DD`) in the time zone of the first `from` stop;
    /// must lie inside the snapshot horizon.
    pub date: String,
    /// Earliest departure from the rider's position, in seconds since local
    /// midnight of `date` (values ≥ 86400 address the following days).
    /// All times in the response use the same reference.
    pub departure: u32,
    pub from: Vec<Endpoint>,
    pub to: Vec<Endpoint>,
    /// Number of vehicle changes (0 = direct only).
    #[serde(default = "default_transfers")]
    pub max_transfers: u8,
    #[serde(default = "default_results")]
    pub max_results: usize,
    /// Journeys arriving later than this after `departure` are not searched.
    #[serde(default = "default_max_travel")]
    pub max_travel_seconds: u32,
    /// Further alternatives must leave the origin at most this long after the
    /// first (earliest-departing) result.
    #[serde(default = "default_interval")]
    pub alternatives_within_seconds: u32,
}
fn default_interval() -> u32 {
    3600
}
fn default_max_travel() -> u32 {
    86_400
}
fn default_transfers() -> u8 {
    3
}
fn default_results() -> usize {
    3
}

#[derive(Debug, Serialize)]
pub struct PlanResponse {
    pub service_date: String,
    pub timezone: String,
    pub itineraries: Vec<Itinerary>,
    /// Journeys found by RAPTOR whose legs could not be reconstructed
    /// consistently; they are omitted instead of returned half-broken.
    pub omitted_unreconstructable: usize,
    /// Non-empty when the snapshot is known to be incomplete (e.g. dropped
    /// transfer rules). Returned itineraries are still valid under the kept rules.
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Itinerary {
    /// When the rider must leave their origin position (includes access walk).
    pub departure: u32,
    /// When the rider reaches their destination position (includes egress walk).
    pub arrival: u32,
    pub departure_time: String,
    pub arrival_time: String,
    pub duration_seconds: u32,
    pub transfers: usize,
    /// True if any vehicle leg needs a booking/arrangement (GTFS pickup or
    /// drop_off type 2/3) at its boarding or alighting stop.
    pub booking_required: bool,
    pub legs: Vec<Leg>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LegKind {
    /// From the rider's origin position to the first stop (request offset).
    Access,
    /// Vehicle ride.
    Transit,
    /// Stop-to-stop footpath from the timetable's transfer graph.
    Walk,
    /// From the last stop to the rider's destination position (request offset).
    Egress,
}

#[derive(Clone, Debug, Serialize)]
pub struct Leg {
    pub kind: LegKind,
    /// `None` for the rider's own origin position (access legs).
    pub from: Option<Stop>,
    /// `None` for the rider's own destination position (egress legs).
    pub to: Option<Stop>,
    pub departure: u32,
    pub arrival: u32,
    pub departure_time: String,
    pub arrival_time: String,
    pub duration_seconds: u32,
    pub trip_id: Option<String>,
    /// `YYYYMMDD_HH:MM` (service date, first departure) – with `trip_id` the
    /// MOTIS-style trip reference.
    #[serde(skip)]
    pub trip_ref: Option<String>,
    pub route_id: Option<String>,
    pub line: Option<String>,
    /// GTFS `trip_short_name` (after the feed script), e.g. `ICE 119`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trip_short_name: Option<String>,
    /// Name to show for the vehicle (MOTIS `displayName`): the feed script's
    /// choice, else the line, else `trip_short_name`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub mode: Option<String>,
    /// Ride valid with the Deutschlandticket (transit legs; see `dticket`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deutschlandticket: Option<bool>,
    /// GTFS `route_type` (extended types kept).
    pub route_type: Option<u16>,
    /// `trip_headsign`, else the name of the trip's last stop.
    pub headsign: Option<String>,
    /// Operating agency name.
    pub agency: Option<String>,
    /// `route_color` (hex without `#`).
    pub color: Option<String>,
    /// Stops passed between boarding and alighting (transit legs).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub intermediate_stops: Vec<Stop>,
    /// Boarding needs a phone booking / driver arrangement (pickup_type 2/3).
    /// Operator notice/contact details for a flex ride (booking is not made by Rustis).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub booking_info: Option<String>,
    pub pickup_booking_required: bool,
    /// Alighting needs a phone booking / driver arrangement (drop_off_type 2/3).
    pub drop_off_booking_required: bool,
    /// `[lat, lon]` of every served stop along the leg; not a vehicle track.
    pub geometry: Vec<[f64; 2]>,
    pub geometry_source: &'static str,
    /// Realtime: the times above differ from the schedule below, or the
    /// feed confirmed them.
    #[serde(skip)]
    pub real_time: bool,
    /// Scheduled departure/arrival (ISO) of a realtime ride.
    #[serde(skip)]
    pub scheduled_times: Option<(String, String)>,
    /// Platform at boarding / alighting and the scheduled ones.
    #[serde(skip)]
    pub tracks: Option<LegTracks>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct LegTracks {
    pub from: Option<String>,
    pub scheduled_from: Option<String>,
    pub to: Option<String>,
    pub scheduled_to: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Stats {
    pub snapshot_dir: String,
    pub first_day: String,
    pub horizon_days: u32,
    pub feeds_imported: usize,
    pub feeds_failed: usize,
    pub stops: usize,
    pub routes: usize,
    pub trips: usize,
    pub gtfs_trips: usize,
    pub bitfields: usize,
    pub footpaths: usize,
    pub specific_transfer_rules: usize,
    pub disk_bytes: u64,
    /// Bytes reachable through read-only mappings (page cache, not heap).
    pub mapped_bytes: u64,
    pub process_memory: Option<crate::memory::ProcessMemory>,
}
