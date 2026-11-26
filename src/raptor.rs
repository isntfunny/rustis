//! RAPTOR over the mapped timetable with exact lower-bound pruning.
//!
//! * Labels are kept sparsely per round (`ride[k]`, `walk[k]`): only touched
//!   stops cost memory.
//! * A ride label records the *arrival*; boarding from it needs the stop's
//!   minimum change time (or a matching trip/route-specific rule). A walk
//!   label (footpath after a ride, or from a source) is ready to board at
//!   once. Footpaths are not chained (the footpath relation is closed at import).
//! * Lower bounds: Dijkstra over the reversed lower-bound graph from the
//!   targets (seeded with egress walks), expanded lazily only as far as a
//!   pruning decision needs. A label is dropped if `arr + lb ≥ bound`, where
//!   `bound` is the best target arrival found so far (or the search window).
//!   `lb` never overestimates, so no journey that could beat `bound` is lost.
use crate::build::{FLAG_NO_DROP_OFF, FLAG_NO_PICKUP, FORBIDDEN, NONE};
use crate::rt::{Rt, TripRt};
use crate::tt::Timetable;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::hash::{BuildHasherDefault, Hasher};

/// Small, fast hasher for u32 keys (FxHash constant).
#[derive(Default)]
pub struct Fx(u64);
impl Hasher for Fx {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(5) ^ u64::from(b)).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
        }
    }
    fn write_u32(&mut self, v: u32) {
        self.0 = (self.0.rotate_left(5) ^ u64::from(v)).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}
pub type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<Fx>>;

/// Dense per-stop `u32` slots in an anonymous private mapping: pages are
/// zero-filled on first write only, and the whole mapping goes back to the
/// kernel when the search ends (a heap `Vec` would stay in the allocator's
/// arena and keep the process RSS at its high-water mark). A search that
/// touches few stops therefore stays small, while a huge search costs at
/// most 4 B per stop instead of a hash-map entry (~16–24 B).
struct Lazy(memmap2::MmapMut);

impl Lazy {
    fn new(n: usize) -> Self {
        Self(
            memmap2::MmapMut::map_anon((n.max(1)) * 4).expect("anonymous mapping for search state"),
        )
    }
    #[inline]
    fn slot(&self, i: u32) -> u32 {
        let o = i as usize * 4;
        u32::from_ne_bytes(self.0[o..o + 4].try_into().unwrap())
    }
    #[inline]
    fn get(&self, i: u32) -> Option<u32> {
        match self.slot(i) {
            0 => None,
            v => Some(v - 1),
        }
    }
    #[inline]
    fn set(&mut self, i: u32, v: u32) {
        let o = i as usize * 4;
        self.0[o..o + 4].copy_from_slice(&(v.min(u32::MAX - 1) + 1).to_ne_bytes());
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum From {
    Ride,
    Walk,
}

#[derive(Clone, Copy, Debug)]
pub struct RideLabel {
    pub flex: Option<(u32, u32, u32)>, // service, origin stop, departure
    pub arr: u32,
    pub route: u32,
    /// Global trip index.
    pub trip: u32,
    pub day: u32,
    pub board_pos: u32,
    pub alight_pos: u32,
    pub board_from: From,
}

#[derive(Clone, Copy, Debug)]
pub struct WalkLabel {
    pub ready: u32,
    /// Stop the walk started at (`NONE` for a source label).
    pub from: u32,
    pub secs: u32,
}

pub struct Query<'a> {
    pub allow_flex: bool,
    pub allow_timed: bool,
    /// Bitset over routes that may be boarded (`None`: all).
    pub routes: Option<&'a [u64]>,
    /// Partly usable routes: usable stop positions; a ride may only board,
    /// pass and alight at usable positions.
    pub positions: Option<&'a std::collections::HashMap<u32, Vec<bool>>>,
    pub sources: &'a [(u32, u32)],
    pub targets: &'a [(u32, u32)],
    /// Earliest departure from the rider's position (relative seconds).
    pub depart: u32,
    pub max_transfers: usize,
    /// Only journeys arriving before `depart + window` are searched.
    pub window: u32,
    /// Effort cap: the search stops after this many labels (bounds memory
    /// and time of searches that explore most of the network); the result
    /// is then flagged as possibly incomplete.
    pub max_labels: usize,
    /// Realtime overlay (delays, cancellations, skipped stops).
    pub rt: &'a Rt,
}

#[derive(Clone, Debug)]
pub enum Step {
    Flex {
        service: u32,
        day: u32,
        from: u32,
        to: u32,
        dep: u32,
        arr: u32,
    },
    /// Rider's own walk to a source stop.
    Access {
        stop: u32,
        secs: u32,
    },
    Ride {
        route: u32,
        trip: u32,
        day: u32,
        board_pos: u32,
        alight_pos: u32,
    },
    Walk {
        from: u32,
        to: u32,
        secs: u32,
    },
    Egress {
        stop: u32,
        secs: u32,
    },
}

#[derive(Clone, Debug)]
pub struct Journey {
    /// Arrival at the rider's destination (relative seconds).
    pub arrival: u32,
    pub steps: Vec<Step>,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct SearchStats {
    pub rounds: usize,
    pub routes_scanned: usize,
    pub labels: usize,
    pub lb_settled: usize,
    pub trip_checks: usize,
    /// The label budget was exhausted (results possibly incomplete).
    pub truncated: bool,
}

/// Lazily expanded backward Dijkstra (lower bounds to the targets).
/// Lower bounds to the targets; independent of the departure time, so one
/// instance is shared by all searches of a request (its settled region only grows).
pub struct LowerBound<'t> {
    tt: &'t Timetable,
    dist: Lazy,
    settled: Lazy,
    n_settled: usize,
    heap: BinaryHeap<Reverse<(u32, u32)>>,
}

impl<'t> LowerBound<'t> {
    pub fn new(tt: &'t Timetable, targets: &[(u32, u32)]) -> Self {
        let n = tt.meta.n_stops;
        let mut lb = Self {
            tt,
            dist: Lazy::new(n),
            settled: Lazy::new(n),
            n_settled: 0,
            heap: BinaryHeap::new(),
        };
        for &(s, w) in targets {
            if lb.dist.get(s).is_none_or(|d| w < d) {
                lb.dist.set(s, w);
                lb.heap.push(Reverse((w, s)));
            }
        }
        lb
    }
    /// Exact lower bound of `s` to the targets if it is below `limit`.
    pub fn below(&mut self, s: u32, limit: u32) -> Option<u32> {
        self.at_least(s, limit);
        self.settled.get(s).filter(|&d| d < limit)
    }
    /// True if `lb(s) ≥ need` (expands the search only up to `need`).
    fn at_least(&mut self, s: u32, need: u32) -> bool {
        loop {
            if let Some(d) = self.settled.get(s) {
                return d >= need;
            }
            let Some(&Reverse((d, u))) = self.heap.peek() else {
                return true;
            };
            if d >= need {
                return true; // everything closer than `need` is settled
            }
            self.heap.pop();
            if self.settled.get(u).is_some() {
                continue;
            }
            self.settled.set(u, d);
            self.n_settled += 1;
            let tt = self.tt;
            let (a, b) = (
                tt.lb_in_off[u as usize] as usize,
                tt.lb_in_off[u as usize + 1] as usize,
            );
            for (&v, &w) in tt.lb_in_from[a..b].iter().zip(&tt.lb_in_secs[a..b]) {
                let nd = d.saturating_add(w);
                if self.settled.get(v).is_none() && self.dist.get(v).is_none_or(|x| nd < x) {
                    self.dist.set(v, nd);
                    self.heap.push(Reverse((nd, v)));
                }
            }
        }
    }
}

struct Search<'t, 'q, 'l> {
    tt: &'t Timetable,
    q: &'q Query<'q>,
    targets: FxMap<u32, u32>,
    ride: Vec<FxMap<u32, RideLabel>>,
    walk: Vec<FxMap<u32, WalkLabel>>,
    best_ride: Lazy,
    best_ready: Lazy,
    lb: &'l mut LowerBound<'t>,
    bound: u32,
    inserted: usize,
    /// Best target arrival per round: (arrival, target stop, label kind,
    /// final walk (from stop, secs) after a ride – `NONE` otherwise).
    best_target: Vec<Option<Target>>,
    stats: SearchStats,
}

#[derive(Clone, Copy)]
struct Target {
    arr: u32,
    stop: u32,
    kind: From,
    walk_from: u32,
    walk_secs: u32,
}

/// One RAPTOR search; `lb` must have been created for `q.targets`.
pub fn search<'t>(
    tt: &'t Timetable,
    q: &Query,
    lb: &mut LowerBound<'t>,
) -> (Vec<Journey>, SearchStats) {
    let mut s = Search {
        tt,
        q,
        targets: q.targets.iter().fold(FxMap::default(), |mut m, &(t, w)| {
            let e = m.entry(t).or_insert(w);
            *e = (*e).min(w);
            m
        }),
        ride: vec![FxMap::default()],
        walk: vec![FxMap::default()],
        best_ride: Lazy::new(tt.meta.n_stops),
        best_ready: Lazy::new(tt.meta.n_stops),
        lb,
        inserted: 0,
        bound: q.depart.saturating_add(q.window),
        best_target: vec![None],
        stats: SearchStats::default(),
    };
    s.run();
    let mut out = Vec::new();
    let mut prev = u32::MAX;
    for k in 0..s.best_target.len() {
        if let Some(t) = s.best_target[k]
            && t.arr < prev
        {
            prev = t.arr;
            if let Some(j) = s.reconstruct(k, t) {
                out.push(j);
            }
        }
    }
    s.stats.lb_settled = s.lb.n_settled;
    (out, s.stats)
}

impl<'q> Search<'_, 'q, '_> {
    fn note_target(&mut self, k: usize, stop: u32, arr: u32, kind: From) {
        self.note_target_walk(k, stop, arr, kind, NONE, 0);
    }

    /// `walk_from`/`walk_secs`: the target is reached on foot from the ride
    /// label at `walk_from` (its walk label may hold another, better-for-
    /// boarding path, so the walk is recorded here).
    fn note_target_walk(
        &mut self,
        k: usize,
        stop: u32,
        arr: u32,
        kind: From,
        walk_from: u32,
        walk_secs: u32,
    ) {
        if let Some(&w) = self.targets.get(&stop) {
            let a = arr.saturating_add(w);
            if a < self.bound && self.best_target[k].is_none_or(|b| a < b.arr) {
                self.best_target[k] = Some(Target {
                    arr: a,
                    stop,
                    kind,
                    walk_from,
                    walk_secs,
                });
                self.bound = a;
            }
        }
    }

    fn pruned(&mut self, s: u32, t: u32) -> bool {
        t >= self.bound || self.lb.at_least(s, self.bound - t)
    }

    fn run(&mut self) {
        let tt = self.tt;
        // Round 0: sources and footpaths from them.
        let mut marked: Vec<u32> = Vec::new();
        for &(s, w) in self.q.sources {
            let t = self.q.depart.saturating_add(w);
            if self.best_ready.get(s).is_none_or(|b| t < b) && !self.pruned(s, t) {
                self.walk[0].insert(
                    s,
                    WalkLabel {
                        ready: t,
                        from: NONE,
                        secs: w,
                    },
                );
                self.best_ready.set(s, t);
                marked.push(s);
                self.note_target(0, s, t, From::Walk);
            }
        }
        for &(s, w) in self.q.sources {
            let Some(src) = self.walk[0].get(&s).copied() else {
                continue;
            };
            if src.from != NONE || src.secs != w {
                continue;
            }
            for (u, secs) in tt.footpaths(s) {
                let t = src.ready.saturating_add(secs);
                if self.best_ready.get(u).is_none_or(|b| t < b) && !self.pruned(u, t) {
                    self.walk[0].insert(
                        u,
                        WalkLabel {
                            ready: t,
                            from: s,
                            secs,
                        },
                    );
                    self.best_ready.set(u, t);
                    marked.push(u);
                    self.note_target(0, u, t, From::Walk);
                }
            }
        }
        for k in 1..=self.q.max_transfers + 1 {
            if marked.is_empty() || self.stats.truncated {
                break;
            }
            self.stats.rounds = k;
            self.ride.push(FxMap::default());
            self.walk.push(FxMap::default());
            self.best_target.push(None);
            // Routes to scan, from their earliest marked position.
            let mut queue: FxMap<u32, u32> = FxMap::default();
            marked.sort_unstable();
            marked.dedup();
            for &s in &marked {
                if !self.q.allow_timed {
                    continue;
                }
                let (a, b) = (
                    tt.stop_route_off[s as usize] as usize,
                    tt.stop_route_off[s as usize + 1] as usize,
                );
                for (&r, &p) in tt.stop_route[a..b].iter().zip(&tt.stop_route_pos[a..b]) {
                    let e = queue.entry(r).or_insert(p);
                    *e = (*e).min(p);
                }
            }
            let mut improved: Vec<u32> = Vec::new();
            let mut routes: Vec<(u32, u32)> = queue.into_iter().collect();
            routes.sort_unstable();
            for (r, p0) in routes {
                if self
                    .q
                    .routes
                    .is_some_and(|m| m[r as usize / 64] >> (r % 64) & 1 == 0)
                {
                    continue;
                }
                if self.inserted > self.q.max_labels {
                    self.stats.truncated = true;
                    break;
                }
                self.stats.routes_scanned += 1;
                self.scan_route(k, r, p0, &mut improved);
            }
            if self.q.allow_flex {
                self.scan_flex(k, &marked, &mut improved);
            }
            // Footpaths and ready times from the improved ride labels.
            let mut next: Vec<u32> = Vec::new();
            improved.sort_unstable();
            improved.dedup();
            for &s in &improved {
                let Some(l) = self.ride[k].get(&s).copied() else {
                    continue;
                };
                let ct = tt.stop_change[s as usize];
                if ct != FORBIDDEN {
                    let ready = l.arr.saturating_add(ct);
                    if self.best_ready.get(s).is_none_or(|b| ready < b) {
                        self.best_ready.set(s, ready);
                        next.push(s);
                    }
                }
                // Leaving the stop on foot to board elsewhere takes at least
                // the stop's change time (as nigiri does) unless transfers.txt
                // gives this very pair; arriving at a target only takes the walk.
                let floor = if ct == FORBIDDEN { 0 } else { ct };
                for (u, secs, explicit) in tt.footpaths_flagged(s) {
                    let on_foot = l.arr.saturating_add(secs);
                    if self.targets.contains_key(&u) {
                        self.note_target_walk(k, u, on_foot, From::Walk, s, secs);
                    }
                    let t = if explicit {
                        on_foot
                    } else {
                        l.arr.saturating_add(secs.max(floor))
                    };
                    if self.best_ready.get(u).is_none_or(|b| t < b) && !self.pruned(u, t) {
                        self.inserted += 1;
                        self.walk[k].insert(
                            u,
                            WalkLabel {
                                ready: t,
                                from: s,
                                secs,
                            },
                        );
                        self.best_ready.set(u, t);
                        next.push(u);
                    }
                }
            }
            self.stats.labels += self.ride[k].len() + self.walk[k].len();
            marked = next;
        }
    }

    fn scan_flex(&mut self, k: usize, marked: &[u32], improved: &mut Vec<u32>) {
        let tt = self.tt;
        for &s in marked {
            let Some((ready, from, a_stop, a_arr, a_trip, a_route)) = self.ready(k - 1, s) else {
                continue;
            };
            for &service in &tt.flex_pickup[tt.flex_pickup_off[s as usize] as usize
                ..tt.flex_pickup_off[s as usize + 1] as usize]
            {
                let f = tt.flex_service(service);
                let rules = self.rules_for(s, a_stop, a_trip, a_route);
                let ready = match rules.as_ref().and_then(|rs| best_rule(rs, f[10], f[7])) {
                    Some(rule) if rule.secs == FORBIDDEN => continue,
                    Some(rule) => a_arr.saturating_add(rule.secs),
                    None => match ready {
                        Some(t) => t,
                        None => continue,
                    },
                };
                for day in 0..tt.days {
                    let base = tt.base[f[6] as usize][day];
                    if base.saturating_add(f[1]) < ready
                        || base.saturating_add(f[0]) >= self.bound
                        || !tt.active(f[5], day)
                    {
                        continue;
                    }
                    let dep = ready.max(base + f[0]);
                    let arr = dep.saturating_add(f[4]).max(base + f[2]);
                    if dep > base + f[1] || arr > base + f[3] {
                        continue;
                    }
                    for &to in tt.flex_area(f[9]) {
                        if self.inserted > self.q.max_labels {
                            self.stats.truncated = true;
                            return;
                        }
                        if self.best_ride.get(to).is_none_or(|b| arr < b) && !self.pruned(to, arr) {
                            self.best_ride.set(to, arr);
                            self.inserted += 1;
                            self.ride[k].insert(
                                to,
                                RideLabel {
                                    flex: Some((service, s, dep)),
                                    arr,
                                    route: NONE,
                                    trip: NONE,
                                    day: day as u32,
                                    board_pos: 0,
                                    alight_pos: 0,
                                    board_from: from,
                                },
                            );
                            improved.push(to);
                            self.note_target(k, to, arr, From::Ride);
                        }
                    }
                }
            }
        }
    }

    /// Ready time at `s` from round `k` labels plus alight context for rules:
    /// (ready, from, alight stop, alight arrival, alight trip).
    fn ready(&self, k: usize, s: u32) -> Option<(Option<u32>, From, u32, u32, u32, u32)> {
        let tt = self.tt;
        let context = |l: &RideLabel| {
            if let Some((service, _, _)) = l.flex {
                let f = tt.flex_service(service);
                (f[10], f[7])
            } else {
                (
                    tt.trip_gtfs[l.trip as usize],
                    tt.route_info[l.route as usize],
                )
            }
        };
        let mut best: Option<(Option<u32>, From, u32, u32, u32, u32)> = None;
        if let Some(l) = self.ride[k].get(&s) {
            let ct = tt.stop_change[s as usize];
            let ready = (ct != FORBIDDEN).then(|| l.arr.saturating_add(ct));
            let (g, info) = context(l);
            best = Some((ready, From::Ride, s, l.arr, g, info));
        }
        if let Some(w) = self.walk[k].get(&s) {
            let (stop, arr, trip, route) = if k == 0 {
                (NONE, 0, NONE, NONE)
            } else {
                let l = self.ride[k][&w.from];
                let (g, info) = context(&l);
                (w.from, l.arr, g, info)
            };
            if best.is_none_or(|b| b.0.is_none_or(|r| w.ready < r)) {
                best = Some((Some(w.ready), From::Walk, stop, arr, trip, route));
            }
        }
        best
    }

    fn scan_route(&mut self, k: usize, r: u32, p0: u32, improved: &mut Vec<u32>) {
        let tt = self.tt;
        let ru = r as usize;
        let n = tt.n_route_stops(ru);
        let trip0 = tt.route_trip_off[ru];
        let tz = tt.route_tz[ru] as usize;
        // current trip: (local trip index, day, base, board pos, board_from)
        let mut cur: Option<(usize, u32, u32, u32, From)> = None;
        let usable = self.q.positions.and_then(|m| m.get(&r));
        // realtime: trip of the current ride with overridden times, if any
        let mut cur_rt: Option<&'q TripRt> = None;
        for pos in p0 as usize..n {
            if usable.is_some_and(|u| !u[pos]) {
                cur = None;
                cur_rt = None;
                continue;
            }
            let s = tt.route_stop(ru, pos);
            let flags = tt.route_flag(ru, pos);
            if let Some((j, day, base, bpos, from)) = cur
                && flags & FLAG_NO_DROP_OFF == 0
                && !cur_rt.is_some_and(|t| t.skipped(pos))
            {
                let a = cur_rt.map_or_else(|| base + tt.arrs(ru, pos)[j], |t| t.times[pos].0);
                if self.best_ride.get(s).is_none_or(|b| a < b) && !self.pruned(s, a) {
                    self.best_ride.set(s, a);
                    self.inserted += 1;
                    self.ride[k].insert(
                        s,
                        RideLabel {
                            flex: None,
                            arr: a,
                            route: r,
                            trip: trip0 + j as u32,
                            day,
                            board_pos: bpos,
                            alight_pos: pos as u32,
                            board_from: from,
                        },
                    );
                    improved.push(s);
                    self.note_target(k, s, a, From::Ride);
                }
            }
            if flags & FLAG_NO_PICKUP != 0 || pos + 1 == n {
                continue;
            }
            let Some((ready, from, a_stop, a_arr, a_trip, a_route)) = self.ready(k - 1, s) else {
                continue;
            };
            // Only labels that improved in round k-1 trigger boarding.
            let cur_dep = cur.map(|(j, _, base, _, _)| {
                cur_rt.map_or_else(|| base + tt.deps(ru, pos)[j], |t| t.times[pos].1)
            });
            let rules = self.rules_for(s, a_stop, a_trip, a_route);
            let t_search = match (&rules, ready) {
                (None, Some(t)) => t,
                (None, None) => continue,
                (Some(rs), _) => rs
                    .iter()
                    .filter(|x| x.secs != FORBIDDEN)
                    .map(|x| a_arr.saturating_add(x.secs))
                    .chain(ready)
                    .min()
                    .unwrap_or(u32::MAX),
            };
            if cur_dep.is_some_and(|d| d <= t_search) {
                continue;
            }
            if let Some((j, day, base, over)) =
                self.earliest_trip(ru, pos, tz, t_search, cur_dep, |tt, j, dep| {
                    let Some(rs) = &rules else { return true };
                    let trip = trip0 + j as u32;
                    let (g, info) = (tt.trip_gtfs[trip as usize], tt.route_info[ru]);
                    match best_rule(rs, g, info) {
                        Some(x) if x.secs == FORBIDDEN => false,
                        Some(x) => dep >= a_arr.saturating_add(x.secs),
                        None => ready.is_some_and(|t| dep >= t),
                    }
                })
            {
                cur = Some((j, day, base, pos as u32, from));
                cur_rt = over;
            }
        }
    }

    /// Specific rules at boarding stop `s` that match the alighting context.
    fn rules_for(
        &self,
        s: u32,
        a_stop: u32,
        a_trip: u32,
        a_route: u32,
    ) -> Option<Vec<MatchedRule>> {
        let tt = self.tt;
        let (a, b) = (
            tt.rule_off[s as usize] as usize,
            tt.rule_off[s as usize + 1] as usize,
        );
        if a == b || a_stop == NONE {
            return None;
        }
        let (g, info) = (a_trip, a_route);
        let v: Vec<MatchedRule> = (a..b)
            .filter(|&i| {
                tt.rule_from_stop[i] == a_stop
                    && (tt.rule_from_trip[i] == NONE || tt.rule_from_trip[i] == g)
                    && (tt.rule_from_route[i] == NONE || tt.rule_from_route[i] == info)
            })
            .map(|i| MatchedRule {
                to_trip: tt.rule_to_trip[i],
                to_route: tt.rule_to_route[i],
                secs: tt.rule_secs[i],
                rank: rank(
                    tt.rule_from_trip[i],
                    tt.rule_to_trip[i],
                    tt.rule_from_route[i],
                    tt.rule_to_route[i],
                ),
            })
            .collect();
        (!v.is_empty()).then_some(v)
    }

    /// Earliest trip of route `r` departing at `pos` at or after `t` that is
    /// active on its service day and accepted by `ok`, strictly before `limit`.
    fn earliest_trip(
        &mut self,
        r: usize,
        pos: usize,
        tz: usize,
        t: u32,
        limit: Option<u32>,
        ok: impl Fn(&Timetable, usize, u32) -> bool,
    ) -> Option<(usize, u32, u32, Option<&'q TripRt>)> {
        let tt = self.tt;
        let rt: &'q Rt = self.q.rt;
        let over = rt.has_route(r as u32);
        let deps = tt.deps(r, pos);
        let base = &tt.base[tz];
        let span = tt.route_max_time[r] / 86_400 + 1;
        // day whose base is the last one ≤ t
        let d_t = base.partition_point(|&b| b <= t).saturating_sub(1) as u32;
        let trip0 = tt.route_trip_off[r] as usize;
        let mut best: Option<(usize, u32, u32, Option<&'q TripRt>)> = None;
        let mut best_t = limit.unwrap_or(u32::MAX).min(self.bound);
        for day in d_t.saturating_sub(span)..=d_t + 1 {
            if day as usize >= tt.days {
                break;
            }
            let b = base[day as usize];
            if b >= best_t {
                break;
            }
            let from = if t > b {
                deps.partition_point(|&d| d < t - b)
            } else {
                0
            };
            for (j, &d) in deps.iter().enumerate().skip(from) {
                let td = b + d;
                if td >= best_t {
                    break;
                }
                self.stats.trip_checks += 1;
                if tt.active(tt.trip_bits[trip0 + j], day as usize)
                    && !(over && rt.get((trip0 + j) as u32, day).is_some())
                    && ok(tt, j, td)
                {
                    best = Some((j, day, b, None));
                    best_t = td;
                    break;
                }
            }
        }
        if over {
            // trips with realtime times are not in the sorted base arrays
            for &(j, day) in rt.route_trips(r as u32) {
                let Some(tr) = rt.get((trip0 + j as usize) as u32, day) else {
                    continue;
                };
                if tr.cancelled || tr.skipped(pos) {
                    continue;
                }
                let td = tr.times[pos].1;
                if td >= t && td < best_t && ok(tt, j as usize, td) {
                    best = Some((j as usize, day, base[day as usize], Some(tr)));
                    best_t = td;
                }
            }
        }
        best
    }

    fn reconstruct(&self, k: usize, t: Target) -> Option<Journey> {
        let (target, arrival) = (t.stop, t.arr);
        let mut steps = vec![Step::Egress {
            stop: target,
            secs: self.targets[&target],
        }];
        let (mut k, mut stop, mut kind) = (k, target, t.kind);
        if t.walk_from != NONE {
            steps.push(Step::Walk {
                from: t.walk_from,
                to: target,
                secs: t.walk_secs,
            });
            stop = t.walk_from;
            kind = From::Ride;
        }
        loop {
            match kind {
                From::Walk => {
                    let w = self.walk[k].get(&stop)?;
                    if w.from == NONE {
                        steps.push(Step::Access { stop, secs: w.secs });
                        break;
                    }
                    steps.push(Step::Walk {
                        from: w.from,
                        to: stop,
                        secs: w.secs,
                    });
                    stop = w.from;
                    kind = if k == 0 { From::Walk } else { From::Ride };
                }
                From::Ride => {
                    let l = self.ride[k].get(&stop)?;
                    if let Some((service, from, dep)) = l.flex {
                        steps.push(Step::Flex {
                            service,
                            day: l.day,
                            from,
                            to: stop,
                            dep,
                            arr: l.arr,
                        });
                        stop = from;
                        kind = l.board_from;
                        k = k.checked_sub(1)?;
                        continue;
                    }
                    steps.push(Step::Ride {
                        route: l.route,
                        trip: l.trip,
                        day: l.day,
                        board_pos: l.board_pos,
                        alight_pos: l.alight_pos,
                    });
                    stop = self.tt.route_stop(l.route as usize, l.board_pos as usize);
                    kind = l.board_from;
                    k = k.checked_sub(1)?;
                }
            }
        }
        steps.reverse();
        Some(Journey { arrival, steps })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct MatchedRule {
    to_trip: u32,
    to_route: u32,
    secs: u32,
    rank: u8,
}

/// GTFS specificity: both trips > trip+route > both routes > one trip > one route.
fn rank(ft: u32, tt_: u32, fr: u32, tr: u32) -> u8 {
    let trips = u8::from(ft != NONE) + u8::from(tt_ != NONE);
    let routes = u8::from(fr != NONE) + u8::from(tr != NONE);
    match (trips, routes) {
        (2, _) => 5,
        (1, r) if r >= 1 => 4,
        (0, 2) => 3,
        (1, _) => 2,
        _ => 1,
    }
}

/// Most specific matching rule for boarding trip `g` (route `info`); ties:
/// the strictest (forbidden, then longest time).
fn best_rule(rs: &[MatchedRule], g: u32, info: u32) -> Option<MatchedRule> {
    rs.iter()
        .filter(|x| {
            (x.to_trip == NONE || x.to_trip == g) && (x.to_route == NONE || x.to_route == info)
        })
        .max_by_key(|x| (x.rank, x.secs))
        .copied()
}
