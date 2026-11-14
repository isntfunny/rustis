//! Public request/response types and persisted metadata.
use serde::{Deserialize, Serialize};

/// Hard request bounds. Each query allocates scratch proportional to
/// `stops × (max_transfers + 1)`, so these bounds are part of the RAM budget.
pub const MAX_ENDPOINTS: usize = 64;
pub const MAX_TRANSFERS: u8 = 8;
pub const MAX_RESULTS: usize = 10;
pub const MAX_BBOX_RESULTS: usize = 1000;
/// Snapshots hold the service date plus the following day (`OvernightDays(1)`).
pub const SNAPSHOT_HORIZON_SECONDS: u32 = 2 * 86_400;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stop {
    pub stop_id: String,
    pub name: String,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub parent_station: Option<String>,
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
    /// Service date (`YYYY-MM-DD`); must equal the snapshot's service date.
    pub date: String,
    /// Earliest departure from the rider's position, in seconds since the
    /// service date's local midnight. Values ≥ 86400 address the following
    /// night/day (GTFS-style `25:10:00` = 90600).
    pub departure: u32,
    pub from: Vec<Endpoint>,
    pub to: Vec<Endpoint>,
    /// Number of vehicle changes (0 = direct only).
    #[serde(default = "default_transfers")]
    pub max_transfers: u8,
    #[serde(default = "default_results")]
    pub max_results: usize,
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

#[derive(Debug, Serialize)]
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
    pub route_id: Option<String>,
    pub line: Option<String>,
    pub mode: Option<String>,
    /// Boarding needs a phone booking / driver arrangement (pickup_type 2/3).
    pub pickup_booking_required: bool,
    /// Alighting needs a phone booking / driver arrangement (drop_off_type 2/3).
    pub drop_off_booking_required: bool,
    /// `[lat, lon]` of every served stop along the leg; not a vehicle track.
    pub geometry: Vec<[f64; 2]>,
    pub geometry_source: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Stats {
    pub snapshot_dir: String,
    pub service_date: String,
    pub timezone: String,
    pub stops: usize,
    pub routes: usize,
    pub raptor_routes: usize,
    pub trips: usize,
    pub footpaths: usize,
    /// `strict`, or `conservative (…)` when relaxing transfer rules were dropped.
    pub transfer_rules: String,
    pub timetable_file_bytes: u64,
    /// Portion of the timetable file kept mapped (not decoded into heap).
    pub mapped_bytes: u64,
    pub metadata_bytes: u64,
    pub process_memory: Option<crate::memory::ProcessMemory>,
}

/// GTFS route data shared by all RAPTOR routes split from it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RouteInfo {
    pub route_id: String,
    pub line: String,
    pub mode: String,
}

pub(crate) const METADATA_VERSION: u32 = 2;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Metadata {
    pub version: u32,
    pub rustis_version: String,
    pub service_date: String,
    pub timezone: String,
    pub source: String,
    /// Whether footpaths may be chained (false) or are a closed relation (true).
    pub footpaths_closed: bool,
    pub walk_radius_m: Option<f64>,
    pub n_raptor_routes: usize,
    pub n_trips: usize,
    pub timetable_bytes: u64,
    /// FNV-1a 64 over the whole timetable file (checked by `verify`).
    pub timetable_fnv64: String,
    pub import_notes: Vec<String>,
    /// `strict`: every transfers.txt rule is enforced. `conservative`: relaxing
    /// rules were dropped (results valid, possibly incomplete).
    pub transfer_mode: String,
    pub dropped_transfer_rules: std::collections::BTreeMap<String, usize>,
    #[serde(default)]
    pub excluded_trips: std::collections::BTreeMap<String, usize>,
    pub default_change_seconds: u32,
    pub stops: Vec<Stop>,
    pub routes: Vec<RouteInfo>,
}
