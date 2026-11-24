//! Stations, departure/arrival boards and trip lookup on a snapshot.
//!
//! Trip references are `YYYYMMDD_HH:MM_<GTFS trip id>`: the service date, the
//! scheduled departure at the trip's first stop (local time of the service
//! day, may exceed 24:00) and the GTFS trip id – the MOTIS trip id format.
//! They stay valid across snapshot rebuilds as long as the feed keeps the trip.
use crate::Snapshot;
use crate::build::{FLAG_NO_DROP_OFF, FLAG_NO_PICKUP, NONE};
use crate::tt::Timetable;

/// One station: its representative stop and the served stops it consists of.
pub(crate) struct Station {
    pub rep: u32,
    pub stops: Vec<u32>,
    /// Town, if known.
    pub area: Option<String>,
}

/// A trip on one service day.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TripRef {
    pub route: u32,
    /// Global snapshot trip index.
    pub trip: u32,
    /// Service day (bit index).
    pub day: u32,
}

/// A stop event of a trip (departure or arrival).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Event {
    pub at: TripRef,
    pub pos: u32,
    pub stop: u32,
    /// Relative seconds.
    pub time: u32,
}

/// Board look-ahead beyond the first day.
const MAX_BOARD_DAYS: u32 = 7;

impl TripRef {
    fn j(&self, tt: &Timetable) -> usize {
        (self.trip - tt.route_trip_off[self.route as usize]) as usize
    }
    fn base(&self, tt: &Timetable) -> u32 {
        tt.base[tt.route_tz[self.route as usize] as usize][self.day as usize]
    }
    /// (arrival, departure) at `pos`, relative seconds.
    pub fn times(&self, tt: &Timetable, pos: usize) -> (u32, u32) {
        let (r, j, b) = (self.route as usize, self.j(tt), self.base(tt));
        (b + tt.arrs(r, pos)[j], b + tt.deps(r, pos)[j])
    }
    pub fn gtfs(&self, tt: &Timetable) -> usize {
        tt.trip_gtfs[self.trip as usize] as usize
    }
}

fn route_of(tt: &Timetable, trip: u32) -> u32 {
    (tt.route_trip_off.partition_point(|&o| o <= trip) - 1) as u32
}

/// Service date of day bit `d` (bit 0 = `first_day − 1`).
fn date_of(tt: &Timetable, d: u32) -> jiff::civil::Date {
    let first = jiff::civil::Date::strptime("%Y-%m-%d", &tt.meta.first_day).expect("first_day");
    first + jiff::Span::new().days(i64::from(d) - 1)
}

fn day_of(tt: &Timetable, date: jiff::civil::Date) -> Option<u32> {
    let first = jiff::civil::Date::strptime("%Y-%m-%d", &tt.meta.first_day).ok()?;
    let d = (date - first).get_days() as i64 + 1;
    (0..tt.days as i64).contains(&d).then_some(d as u32)
}

impl Snapshot {
    /// The station a stop id belongs to (the stop alone if not grouped).
    pub(crate) fn station(&self, id: &str) -> Option<Station> {
        let tt = self.tt();
        let s = tt.catalog.lookup(id)? as u32;
        let g = tt.ix.stop_group[s as usize];
        Some(if g == NONE {
            Station {
                rep: s,
                stops: vec![s],
                area: None,
            }
        } else {
            self.group_station(g)
        })
    }

    fn group_station(&self, g: u32) -> Station {
        let tt = self.tt();
        let a = tt.ix.group_area[g as usize];
        Station {
            rep: tt.ix.group_rep[g as usize],
            stops: tt.ix.members(g).to_vec(),
            area: (a != NONE).then(|| tt.ix.area_name.get(a as usize).to_owned()),
        }
    }

    /// Stations matching free text, best first: (station, score).
    pub(crate) fn geocode(&self, text: &str, limit: usize) -> Vec<(Station, f64)> {
        let tt = self.tt();
        tt.ix
            .geocode(text, limit)
            .into_iter()
            .map(|(g, score)| (self.group_station(g), score))
            .collect()
    }

    /// Relative seconds of a Unix time, if inside the snapshot horizon.
    pub(crate) fn rel(&self, unix: i64) -> Option<u32> {
        u32::try_from(unix - self.tt().origin).ok()
    }

    pub(crate) fn unix(&self, rel: u32) -> i64 {
        i64::from(rel) + self.tt().origin
    }

    /// The next `n` departures (or arrivals) at `stops` from `t` on, in time
    /// order. Events at a trip's last stop (first stop) and where boarding
    /// (alighting) is not allowed are left out.
    pub(crate) fn board(&self, stops: &[u32], t: u32, n: usize, arrivals: bool) -> Vec<Event> {
        let tt = self.tt();
        let mut out: Vec<Event> = Vec::new();
        let mut pairs: Vec<(u32, u32, u32)> = Vec::new(); // (stop, route, pos)
        for &s in stops {
            let (a, b) = (
                tt.stop_route_off[s as usize] as usize,
                tt.stop_route_off[s as usize + 1] as usize,
            );
            for k in a..b {
                let (r, p) = (tt.stop_route[k], tt.stop_route_pos[k]);
                let last = tt.n_route_stops(r as usize) as u32 - 1;
                let f = tt.route_flag(r as usize, p as usize);
                let skip = if arrivals {
                    p == 0 || f & FLAG_NO_DROP_OFF != 0
                } else {
                    p == last || f & FLAG_NO_PICKUP != 0
                };
                if !skip {
                    pairs.push((s, r, p));
                }
            }
        }
        // days from the earliest one a trip still running at `t` can belong to
        // first the days a trip running at `t` can belong to, then one more
        // day at a time until `n` events are found
        let mut extra: Option<u32> = None;
        loop {
            for &(s, r, p) in &pairs {
                let ru = r as usize;
                let tz = tt.route_tz[ru] as usize;
                let base = &tt.base[tz];
                let d_t = base.partition_point(|&b| b <= t).saturating_sub(1) as u32;
                let span = tt.route_max_time[ru] / 86_400 + 1;
                let days = match extra {
                    None => d_t.saturating_sub(span)..=d_t + 1,
                    Some(k) => d_t + k..=d_t + k,
                };
                let times = if arrivals {
                    tt.arrs(ru, p as usize)
                } else {
                    tt.deps(ru, p as usize)
                };
                let trip0 = tt.route_trip_off[ru];
                for day in days {
                    if day as usize >= tt.days {
                        break;
                    }
                    let b = base[day as usize];
                    let from = if t > b {
                        times.partition_point(|&x| x < t - b)
                    } else {
                        0
                    };
                    let mut taken = 0;
                    for (j, &x) in times.iter().enumerate().skip(from) {
                        if taken >= n {
                            break;
                        }
                        let trip = trip0 + j as u32;
                        if tt.active(tt.trip_bits[trip as usize], day as usize) {
                            taken += 1;
                            out.push(Event {
                                at: TripRef {
                                    route: r,
                                    trip,
                                    day,
                                },
                                pos: p,
                                stop: s,
                                time: b + x,
                            });
                        }
                    }
                }
            }
            out.sort_by_key(|e| (e.time, e.at.trip, e.pos));
            if out.len() >= n {
                break;
            }
            let k = extra.map_or(2, |k| k + 1);
            if k > MAX_BOARD_DAYS {
                break;
            }
            extra = Some(k);
        }
        out.truncate(n);
        out
    }

    /// `YYYYMMDD_HH:MM` part of a trip reference.
    pub(crate) fn trip_ref(&self, at: &TripRef) -> String {
        let tt = self.tt();
        let dep0 = tt.deps(at.route as usize, 0)[at.j(tt)];
        format!(
            "{}_{:02}:{:02}",
            date_of(tt, at.day).strftime("%Y%m%d"),
            dep0 / 3600,
            dep0 % 3600 / 60
        )
    }

    /// Resolves `YYYYMMDD_HH:MM` + GTFS trip id (or a bare GTFS trip id on
    /// `date`, the first run that day).
    pub(crate) fn find_trip(
        &self,
        gtfs_id: &str,
        date: jiff::civil::Date,
        hhmm: Option<u32>,
    ) -> Option<TripRef> {
        let tt = self.tt();
        let g = tt.ix.gtfs_trip(&tt.gtfs_trip_id, gtfs_id)?;
        let day = day_of(tt, date)?;
        let mut best: Option<(u32, TripRef)> = None;
        for &trip in tt.ix.trips_of(g) {
            if !tt.active(tt.trip_bits[trip as usize], day as usize) {
                continue;
            }
            let at = TripRef {
                route: route_of(tt, trip),
                trip,
                day,
            };
            let dep0 = tt.deps(at.route as usize, 0)[at.j(tt)];
            if hhmm.is_some_and(|m| dep0 / 60 != m) {
                continue;
            }
            if best.is_none_or(|(d, _)| dep0 < d) {
                best = Some((dep0, at));
            }
        }
        best.map(|b| b.1)
    }

    /// Trips running on `date` whose name (display name, line + number or
    /// bare number) matches `name`, optionally only those calling at one of
    /// `stops` (then with that call's position).
    pub(crate) fn trips_by_name(
        &self,
        name: &str,
        date: jiff::civil::Date,
        stops: Option<&[u32]>,
        limit: usize,
    ) -> Vec<(TripRef, Option<u32>)> {
        let tt = self.tt();
        let key = crate::search::name_key(name);
        let Some(day) = day_of(tt, date) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for &g in tt.ix.trips_named(&key) {
            for &trip in tt.ix.trips_of(g) {
                let route = route_of(tt, trip);
                let info = tt.route_info[route as usize] as usize;
                let keys = crate::search::trip_keys(
                    &tt.gtfs_trip_display,
                    &tt.gtfs_trip_short,
                    &tt.route_info_line,
                    g as usize,
                    info,
                );
                if !keys.contains(&key) || !tt.active(tt.trip_bits[trip as usize], day as usize) {
                    continue;
                }
                let at = TripRef { route, trip, day };
                let pos = match stops {
                    None => None,
                    Some(st) => {
                        match (0..tt.n_route_stops(route as usize))
                            .find(|&p| st.contains(&tt.route_stop(route as usize, p)))
                        {
                            Some(p) => Some(p as u32),
                            None => continue,
                        }
                    }
                };
                out.push((at, pos));
            }
        }
        out.sort_by_key(|(at, _)| at.times(tt, 0).1);
        out.dedup_by_key(|(at, _)| at.trip);
        out.truncate(limit);
        out
    }
}
