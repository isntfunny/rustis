//! Query validation, the Vulture RAPTOR call and leg reconstruction.
//!
//! Vulture returns each journey as `(route, alight stop)` steps. Boarding stops
//! and the walks between alighting and the next boarding are not part of that
//! plan, and Vulture's own `Journey::with_timing` only follows one footpath hop.
//! Rustis therefore rebuilds legs itself: from the current stop/time it runs a
//! small Dijkstra over the same footpath graph (respecting closure semantics),
//! picks the boarding stop and the first usable trip with the earliest arrival,
//! and finally checks the result against RAPTOR's label. Journeys that cannot
//! be rebuilt consistently are counted and omitted, never returned half-broken.
use crate::Error;
use crate::model::*;
use crate::storage::Snapshot;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use vulture::{Duration, Journey, RouteIdx, SecondOfDay, StopIdx, Timetable};

/// Footpath search bound for reconstruction (stops settled per Dijkstra).
const MAX_WALK_SEARCH: usize = 10_000;
const MAX_ENDPOINT_WALK: u32 = 3 * 3600;

impl Snapshot {
    pub fn service_date(&self) -> &str {
        &self.meta.service_date
    }

    pub fn plan(&self, q: &PlanRequest) -> Result<PlanResponse, Error> {
        if q.date != self.meta.service_date {
            return Err(Error::invalid(format!(
                "date {} is not covered; this snapshot serves {} (with trips continuing into the next day)",
                q.date, self.meta.service_date
            )));
        }
        if q.departure >= SNAPSHOT_HORIZON_SECONDS {
            return Err(Error::invalid(format!(
                "departure must be < {SNAPSHOT_HORIZON_SECONDS}"
            )));
        }
        if q.max_transfers > MAX_TRANSFERS {
            return Err(Error::invalid(format!(
                "max_transfers must be ≤ {MAX_TRANSFERS}"
            )));
        }
        if q.max_results == 0 || q.max_results > MAX_RESULTS {
            return Err(Error::invalid(format!(
                "max_results must be in 1..={MAX_RESULTS}"
            )));
        }
        let origins = self.endpoints("from", &q.from)?;
        let targets = self.endpoints("to", &q.to)?;
        let origin_walk: HashMap<StopIdx, u32> = origins.iter().map(|&(s, d)| (s, d.0)).collect();

        let t0 = std::time::Instant::now();
        let journeys = {
            let mut cache = self.caches.checkout();
            self.tt
                .query()
                .from(&origins)
                .to(&targets)
                .max_transfers(q.max_transfers + 1) // Vulture counts boardings.
                .depart_at(SecondOfDay(q.departure))
                .run_with_cache(&mut cache)
        };

        let t_raptor = t0.elapsed();
        let mut omitted = 0;
        let mut found = Vec::new();
        for j in &journeys {
            match self.reconstruct(j, q.departure, origin_walk[&j.origin]) {
                Some(it) if it.arrival <= j.arrival().0 => found.push(it),
                _ => omitted += 1,
            }
        }
        // Pareto filter across all origin/target pairs: earlier arrival or fewer
        // transfers; among equals prefer the later departure.
        found.sort_by_key(|i| (i.arrival, i.transfers, Reverse(i.departure)));
        let mut itineraries: Vec<Itinerary> = Vec::new();
        for it in found {
            if !itineraries
                .iter()
                .any(|k| k.arrival <= it.arrival && k.transfers <= it.transfers)
            {
                itineraries.push(it);
            }
        }
        itineraries.truncate(q.max_results);
        if std::env::var_os("RUSTIS_TIMING").is_some() {
            eprintln!(
                "timing: raptor {:?} ({} journeys), reconstruction {:?}",
                t_raptor,
                journeys.len(),
                t0.elapsed() - t_raptor
            );
        }
        let any_booking = itineraries.iter().any(|i| i.booking_required);
        Ok(PlanResponse {
            service_date: self.meta.service_date.clone(),
            timezone: self.meta.timezone.clone(),
            itineraries,
            omitted_unreconstructable: omitted,
            warnings: {
                let mut w = self.warnings();
                if any_booking {
                    w.push(
                        "BOOKING: itineraries marked booking_required use stops where boarding or alighting \
                         must be arranged (GTFS pickup/drop_off 2/3); service is not guaranteed without it."
                            .into(),
                    );
                }
                w
            },
        })
    }

    fn endpoints(&self, side: &str, eps: &[Endpoint]) -> Result<Vec<(StopIdx, Duration)>, Error> {
        if eps.is_empty() || eps.len() > MAX_ENDPOINTS {
            return Err(Error::invalid(format!(
                "`{side}` needs 1..={MAX_ENDPOINTS} stops"
            )));
        }
        let mut best: Vec<(StopIdx, Duration)> = Vec::new();
        for e in eps {
            let s = *self.stop_by_id.get(&e.stop_id).ok_or_else(|| {
                Error::invalid(format!("unknown stop_id {:?} in `{side}` (not in this snapshot or not served on its days)", e.stop_id))
            })?;
            if e.walk_seconds > MAX_ENDPOINT_WALK {
                return Err(Error::invalid(format!(
                    "walk_seconds must be ≤ {MAX_ENDPOINT_WALK}"
                )));
            }
            match best.iter_mut().find(|(b, _)| *b == s) {
                Some(b) => b.1 = Duration(b.1.0.min(e.walk_seconds)),
                None => best.push((s, Duration(e.walk_seconds))),
            }
        }
        Ok(best)
    }

    /// Shortest footpath times from `start`. With a closed relation, only
    /// direct edges are allowed, mirroring the search semantics.
    fn walk_tree(&self, start: StopIdx) -> HashMap<StopIdx, (u32, Option<StopIdx>)> {
        let mut dist: HashMap<StopIdx, (u32, Option<StopIdx>)> =
            HashMap::from([(start, (0, None))]);
        if self.tt.footpaths_are_transitively_closed() {
            for (to, secs) in self.tt.footpaths(start) {
                if to != start && dist.get(&to).is_none_or(|d| secs < d.0) {
                    dist.insert(to, (secs, Some(start)));
                }
            }
            return dist;
        }
        let mut heap = BinaryHeap::from([Reverse((0u32, start))]);
        let mut settled = 0;
        while let Some(Reverse((d, s))) = heap.pop() {
            if dist[&s].0 < d {
                continue;
            }
            settled += 1;
            if settled > MAX_WALK_SEARCH {
                break;
            }
            for (to, secs) in self.tt.footpaths(s) {
                let nd = d.saturating_add(secs);
                if dist.get(&to).is_none_or(|x| nd < x.0) {
                    dist.insert(to, (nd, Some(s)));
                    heap.push(Reverse((nd, to)));
                }
            }
        }
        dist
    }

    /// Walk legs from the tree root to `to`, starting at `start_time`.
    fn walk_legs(
        &self,
        tree: &HashMap<StopIdx, (u32, Option<StopIdx>)>,
        to: StopIdx,
        start_time: u32,
    ) -> Vec<Leg> {
        let mut path = vec![to];
        let mut cur = to;
        while let Some((_, Some(p))) = tree.get(&cur) {
            path.push(*p);
            cur = *p;
        }
        path.reverse();
        path.windows(2)
            .map(|w| {
                let (a, b) = (w[0], w[1]);
                let (dep, arr) = (start_time + tree[&a].0, start_time + tree[&b].0);
                self.leg(
                    LegKind::Walk,
                    Some(a),
                    Some(b),
                    dep,
                    arr,
                    None,
                    vec![self.coord(a), self.coord(b)],
                )
            })
            .collect()
    }

    fn reconstruct(&self, j: &Journey, depart: u32, origin_walk: u32) -> Option<Itinerary> {
        let mut legs: Vec<Leg> = Vec::new();
        let mut stop = j.origin;
        let mut time = depart + origin_walk;
        for (leg_no, &(route, alight)) in j.plan.iter().enumerate() {
            let tree = self.walk_tree(stop);
            // (arrival, walk seconds, boarding stop, board pos, alight pos, trip)
            let mut best = None;
            for (&s, &(walk, _)) in &tree {
                let Some(&(_, bpos)) = self
                    .tt
                    .get_routes_serving_stop(s)
                    .iter()
                    .find(|(r, _)| *r == route)
                else {
                    continue;
                };
                let Some(apos) = self.tt.position_after(route, bpos, alight) else {
                    continue;
                };
                // Real rules: re-boarding at the stop just alighted needs its
                // minimum change time; a walked change needs the footpath time.
                let ready = if s == stop && leg_no > 0 {
                    time + self.tt.change_secs(s)
                } else {
                    time + walk
                };
                let Some(trip) = self
                    .tt
                    .first_usable_trip(route, SecondOfDay(ready), bpos, apos)
                else {
                    continue;
                };
                let arr = self.tt.raw_arrival(trip, apos);
                let key = (arr, walk);
                if best.as_ref().is_none_or(|b: &((u32, u32), _)| key < b.0) {
                    best = Some((key, (s, bpos, apos, trip)));
                }
            }
            let ((arrival, _), (board, bpos, apos, trip)) = best?;
            legs.extend(self.walk_legs(&tree, board, time));
            let dep = self.tt.get_departure_time(trip, bpos).0;
            let geometry = self
                .tt
                .route_stop_slice(route, bpos, apos)
                .iter()
                .filter_map(|&s| self.coord_opt(s))
                .collect();
            let mut leg = self.leg(
                LegKind::Transit,
                Some(board),
                Some(alight),
                dep,
                arrival,
                Some(route),
                geometry,
            );
            leg.trip_id = Some(self.tt.trip_id(trip));
            leg.pickup_booking_required = self.tt.booking(trip, bpos).0;
            leg.drop_off_booking_required = self.tt.booking(trip, apos).1;
            legs.push(leg);
            stop = alight;
            time = arrival;
        }
        if stop != j.target {
            let tree = self.walk_tree(stop);
            tree.get(&j.target)?;
            legs.extend(self.walk_legs(&tree, j.target, time));
            time += tree[&j.target].0;
        }
        // Walks before the first vehicle happen as late as possible.
        let first_transit = legs.iter().position(|l| l.kind == LegKind::Transit)?;
        let mut end = legs[first_transit].departure;
        for leg in legs[..first_transit].iter_mut().rev() {
            let d = leg.duration_seconds;
            self.retime(leg, end - d, end);
            end -= d;
        }
        if origin_walk > 0 {
            let s = j.origin;
            legs.insert(
                0,
                self.leg(
                    LegKind::Access,
                    None,
                    Some(s),
                    end - origin_walk,
                    end,
                    None,
                    self.coord_opt(s).into_iter().collect(),
                ),
            );
        }
        let egress = j.target_walk.0;
        if egress > 0 {
            let s = j.target;
            legs.push(self.leg(
                LegKind::Egress,
                Some(s),
                None,
                time,
                time + egress,
                None,
                self.coord_opt(s).into_iter().collect(),
            ));
        }
        let departure = legs.first()?.departure;
        let arrival = legs.last()?.arrival;
        Some(Itinerary {
            departure,
            arrival,
            departure_time: self.iso(departure),
            arrival_time: self.iso(arrival),
            duration_seconds: arrival - departure,
            transfers: j.plan.len() - 1,
            booking_required: legs
                .iter()
                .any(|l| l.pickup_booking_required || l.drop_off_booking_required),
            legs,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn leg(
        &self,
        kind: LegKind,
        from: Option<StopIdx>,
        to: Option<StopIdx>,
        departure: u32,
        arrival: u32,
        route: Option<RouteIdx>,
        geometry: Vec<[f64; 2]>,
    ) -> Leg {
        let info = route.map(|r| self.route_info(r));
        Leg {
            kind,
            from: from.map(|s| self.stop(s).clone()),
            to: to.map(|s| self.stop(s).clone()),
            departure,
            arrival,
            departure_time: self.iso(departure),
            arrival_time: self.iso(arrival),
            duration_seconds: arrival - departure,
            trip_id: None,
            route_id: info.map(|i| i.route_id.clone()),
            line: info.map(|i| i.line.clone()),
            mode: info.map(|i| i.mode.clone()),
            pickup_booking_required: false,
            drop_off_booking_required: false,
            geometry,
            geometry_source: "stops",
        }
    }

    fn retime(&self, leg: &mut Leg, departure: u32, arrival: u32) {
        leg.departure = departure;
        leg.arrival = arrival;
        leg.departure_time = self.iso(departure);
        leg.arrival_time = self.iso(arrival);
    }

    fn coord_opt(&self, s: StopIdx) -> Option<[f64; 2]> {
        let st = self.stop(s);
        Some([st.lat?, st.lon?])
    }
    fn coord(&self, s: StopIdx) -> [f64; 2] {
        self.coord_opt(s).unwrap_or([f64::NAN, f64::NAN])
    }

    /// Local wall-clock time; DST windows are rejected at import, so midnight
    /// plus seconds is unambiguous.
    pub(crate) fn iso(&self, secs: u32) -> String {
        use chrono::TimeZone;
        let naive = self.service_date.and_hms_opt(0, 0, 0).expect("midnight")
            + chrono::Duration::seconds(i64::from(secs));
        self.tz
            .from_local_datetime(&naive)
            .earliest()
            .map(|t| t.to_rfc3339())
            .unwrap_or_else(|| format!("{naive} {}", self.meta.timezone))
    }

    /// Stops with coordinates inside the box that at least one route serves.
    pub fn stops_in_bbox(
        &self,
        min: [f64; 2],
        max: [f64; 2],
        limit: usize,
    ) -> Result<Vec<Stop>, Error> {
        let ok = min.iter().chain(&max).all(|v| v.is_finite())
            && (-90.0..=90.0).contains(&min[0])
            && (-90.0..=90.0).contains(&max[0])
            && (-180.0..=180.0).contains(&min[1])
            && (-180.0..=180.0).contains(&max[1])
            && min[0] <= max[0]
            && min[1] <= max[1];
        if !ok {
            return Err(Error::invalid("bbox must be finite lat/lon with min ≤ max"));
        }
        if limit == 0 || limit > MAX_BBOX_RESULTS {
            return Err(Error::invalid(format!(
                "limit must be in 1..={MAX_BBOX_RESULTS}"
            )));
        }
        Ok(self
            .meta
            .stops
            .iter()
            .enumerate()
            .filter(|(i, s)| {
                matches!((s.lat, s.lon), (Some(la), Some(lo)) if la >= min[0] && la <= max[0] && lo >= min[1] && lo <= max[1])
                    && !self.tt.get_routes_serving_stop(StopIdx::new(*i as u32)).is_empty()
            })
            .take(limit)
            .map(|(_, s)| s.clone())
            .collect())
    }

    pub fn stats(&self) -> Stats {
        Stats {
            snapshot_dir: self.dir.display().to_string(),
            service_date: self.meta.service_date.clone(),
            timezone: self.meta.timezone.clone(),
            stops: self.meta.stops.len(),
            routes: self.meta.routes.len(),
            raptor_routes: self.tt.n_routes(),
            trips: self.tt.n_trips(),
            footpaths: self.tt.n_footpaths(),
            transfer_rules: self.transfer_rules_summary(),
            timetable_file_bytes: self.meta.timetable_bytes,
            mapped_bytes: self.tt.mapped_bytes(),
            metadata_bytes: self.metadata_bytes,
            process_memory: crate::memory::ProcessMemory::current(),
        }
    }

    fn transfer_rules_summary(&self) -> String {
        if self.meta.transfer_mode == "strict" {
            "strict".into()
        } else {
            format!(
                "{}: dropped {:?}",
                self.meta.transfer_mode, self.meta.dropped_transfer_rules
            )
        }
    }

    /// Warnings attached to every plan response of an incomplete snapshot.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.meta.excluded_trips.is_empty() {
            out.push(format!(
                "INCOMPLETE: trips were excluded from this snapshot {:?} (GTFS-Flex / frequency-based service or a regional partition).",
                self.meta.excluded_trips
            ));
        }
        if self.meta.transfer_mode == "strict" {
            return out;
        }
        let n: usize = self.meta.dropped_transfer_rules.values().sum();
        out.push(format!(
            "INCOMPLETE: {n} transfer rules were not modelled exactly {:?}. Relaxing rules \
             (guaranteed/in-seat/recommended) were dropped and/or restricting trip/route-specific rules \
             were generalised to all trips. Itineraries obey every modelled rule, but connections are \
             missing; do not treat 'no result' or the ranking as authoritative.",
            self.meta.dropped_transfer_rules
        ));
        out
    }

    pub fn import_notes(&self) -> &[String] {
        &self.meta.import_notes
    }
}
