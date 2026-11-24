//! Read-only view of a snapshot directory written by `build`.
//!
//! All large arrays are memory-mapped (page cache, evictable); only the
//! per-(time zone, day) UTC base table is computed into the heap at open
//! (timezones × days × 8 B).
use crate::build::{FOOT_EXPLICIT, SNAPSHOT_VERSION, SnapshotMeta, noon_minus_12h};
use crate::catalog::Catalog;
use crate::store::Array;
use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};

pub struct Strs {
    off: Array<u64>,
    bytes: Array<u8>,
}

impl Strs {
    fn open(dir: &Path, name: &str, m: &crate::store::Manifest) -> Result<Self> {
        Ok(Self {
            off: Array::open(dir, &format!("{name}.off"), m)?,
            bytes: Array::open(dir, &format!("{name}.bytes"), m)?,
        })
    }
    pub fn get(&self, i: usize) -> &str {
        let (a, b) = (self.off[i] as usize, self.off[i + 1] as usize);
        std::str::from_utf8(&self.bytes[a..b]).unwrap_or("<invalid utf-8>")
    }
}

pub struct Timetable {
    pub flex: Array<u32>,
    pub flex_area_off: Array<u32>,
    pub flex_area_stops: Array<u32>,
    pub flex_pickup_off: Array<u32>,
    pub flex_pickup: Array<u32>,
    pub flex_booking: Strs,
    pub dir: PathBuf,
    pub meta: SnapshotMeta,
    pub catalog: Catalog,
    pub stop_route_off: Array<u32>,
    pub stop_route: Array<u32>,
    pub stop_route_pos: Array<u32>,
    pub foot_off: Array<u32>,
    pub foot_to: Array<u32>,
    pub foot_secs: Array<u32>,
    pub lb_in_off: Array<u32>,
    pub lb_in_from: Array<u32>,
    pub lb_in_secs: Array<u32>,
    pub rule_off: Array<u32>,
    pub rule_from_stop: Array<u32>,
    pub rule_from_trip: Array<u32>,
    pub rule_from_route: Array<u32>,
    pub rule_to_trip: Array<u32>,
    pub rule_to_route: Array<u32>,
    pub rule_secs: Array<u32>,
    pub stop_change: Array<u32>,
    pub stop_tz: Array<u16>,
    pub route_stop_off: Array<u64>,
    pub route_stops: Array<u32>,
    pub route_flags: Array<u8>,
    pub route_trip_off: Array<u32>,
    pub route_time_off: Array<u64>,
    pub route_tz: Array<u16>,
    pub route_info: Array<u32>,
    pub route_max_time: Array<u32>,
    pub route_info_type: Array<u16>,
    #[allow(dead_code)] // kept for diagnostics (route service days)
    pub route_days: Array<u32>,
    pub stop_days: Array<u32>,
    pub bitfields: Array<u64>,
    pub trip_bits: Array<u32>,
    pub trip_gtfs: Array<u32>,
    pub dep: Array<u32>,
    pub arr: Array<u32>,
    pub route_info_id: Strs,
    pub route_info_line: Strs,
    pub route_info_color: Strs,
    pub route_info_agency: Strs,
    pub gtfs_trip_id: Strs,
    pub gtfs_trip_headsign: Array<u32>,
    /// `trip_short_name` / display name per GTFS trip (empty = none / the
    /// route's line).
    pub gtfs_trip_short: Strs,
    pub gtfs_trip_display: Strs,
    pub headsigns: Strs,
    /// Unix seconds that relative time 0 corresponds to.
    pub origin: i64,
    /// `base[tz][day]`: relative time of "noon − 12 h" of service day `day`
    /// (bit index) in time zone `tz`.
    pub base: Vec<Vec<u32>>,
    pub tzs: Vec<jiff::tz::TimeZone>,
    pub words: usize,
    pub days: usize,
}

impl Timetable {
    pub fn open(dir: &Path) -> Result<Self> {
        let meta: SnapshotMeta = serde_json::from_slice(
            &std::fs::read(dir.join("snapshot.json"))
                .with_context(|| format!("reading {}/snapshot.json", dir.display()))?,
        )
        .context("parsing snapshot.json")?;
        ensure!(
            meta.version == SNAPSHOT_VERSION,
            "unsupported snapshot version {}",
            meta.version
        );
        let m = &meta.manifest;
        macro_rules! a {
            ($n:literal) => {
                Array::open(dir, $n, m)?
            };
        }
        let catalog = Catalog::open(&dir.join("catalog.bin"), meta.n_stops)?;
        let first = jiff::civil::Date::strptime("%Y-%m-%d", &meta.first_day)?;
        let d0 = first - jiff::Span::new().days(1);
        let origin = d0
            .at(0, 0, 0, 0)
            .to_zoned(jiff::tz::TimeZone::UTC)?
            .timestamp()
            .as_second()
            - 2 * 86_400;
        let days = meta.horizon_days as usize + 1;
        let mut tzs = Vec::new();
        let mut base = Vec::new();
        for name in &meta.timezones {
            let tz = jiff::tz::TimeZone::get(name)?;
            let mut v = Vec::with_capacity(days + 8);
            for d in 0..days + 8 {
                let date = d0 + jiff::Span::new().days(d as i64);
                v.push(
                    u32::try_from(noon_minus_12h(&tz, date)? - origin)
                        .context("time base out of range")?,
                );
            }
            base.push(v);
            tzs.push(tz);
        }
        let tt = Self {
            flex: a!("flex"),
            flex_area_off: a!("flex_area_off"),
            flex_area_stops: a!("flex_area_stops"),
            flex_pickup_off: a!("flex_pickup_off"),
            flex_pickup: a!("flex_pickup"),
            flex_booking: Strs::open(dir, "flex_booking", m)?,
            dir: dir.to_owned(),
            catalog,
            stop_route_off: a!("stop_route_off"),
            stop_route: a!("stop_route"),
            stop_route_pos: a!("stop_route_pos"),
            foot_off: a!("foot_off"),
            foot_to: a!("foot_to"),
            foot_secs: a!("foot_secs"),
            lb_in_off: a!("lb_in_off"),
            lb_in_from: a!("lb_in_from"),
            lb_in_secs: a!("lb_in_secs"),
            rule_off: a!("rule_off"),
            rule_from_stop: a!("rule_from_stop"),
            rule_from_trip: a!("rule_from_trip"),
            rule_from_route: a!("rule_from_route"),
            rule_to_trip: a!("rule_to_trip"),
            rule_to_route: a!("rule_to_route"),
            rule_secs: a!("rule_secs"),
            stop_change: a!("stop_change"),
            stop_tz: a!("stop_tz"),
            route_stop_off: a!("route_stop_off"),
            route_stops: a!("route_stops"),
            route_flags: a!("route_flags"),
            route_trip_off: a!("route_trip_off"),
            route_time_off: a!("route_time_off"),
            route_tz: a!("route_tz"),
            route_info: a!("route_info"),
            route_max_time: a!("route_max_time"),
            route_info_type: a!("route_info_type"),
            route_days: a!("route_days"),
            stop_days: a!("stop_days"),
            bitfields: a!("bitfields"),
            trip_bits: a!("trip_bits"),
            trip_gtfs: a!("trip_gtfs"),
            dep: a!("dep"),
            arr: a!("arr"),
            route_info_id: Strs::open(dir, "route_info_id", m)?,
            route_info_line: Strs::open(dir, "route_info_line", m)?,
            route_info_color: Strs::open(dir, "route_info_color", m)?,
            route_info_agency: Strs::open(dir, "route_info_agency", m)?,
            gtfs_trip_id: Strs::open(dir, "gtfs_trip_id", m)?,
            gtfs_trip_headsign: a!("gtfs_trip_headsign"),
            gtfs_trip_short: Strs::open(dir, "gtfs_trip_short", m)?,
            gtfs_trip_display: Strs::open(dir, "gtfs_trip_display", m)?,
            headsigns: Strs::open(dir, "headsigns", m)?,
            origin,
            base,
            tzs,
            words: meta.bitfield_words as usize,
            days,
            meta,
        };
        tt.validate()?;
        Ok(tt)
    }

    /// Structural checks (cheap, O(routes + stops)); `verify` adds checksums.
    fn validate(&self) -> Result<()> {
        let (ns, nr, nt) = (self.meta.n_stops, self.meta.n_routes, self.meta.n_trips);
        let csr = |name: &str, off: &[u32], len: usize| -> Result<()> {
            ensure!(off.len() == ns + 1, "{name}: wrong length");
            ensure!(
                off.windows(2).all(|w| w[0] <= w[1]) && off[ns] as usize == len,
                "{name}: not monotone"
            );
            Ok(())
        };
        csr(
            "stop_route_off",
            &self.stop_route_off,
            self.stop_route.len(),
        )?;
        csr("foot_off", &self.foot_off, self.foot_to.len())?;
        csr("lb_in_off", &self.lb_in_off, self.lb_in_from.len())?;
        csr("rule_off", &self.rule_off, self.rule_secs.len())?;
        csr(
            "flex_pickup_off",
            &self.flex_pickup_off,
            self.flex_pickup.len(),
        )?;
        ensure!(
            self.flex.len().is_multiple_of(crate::flex::WIDTH),
            "flex row width"
        );
        ensure!(
            self.flex_area_off.len() == self.flex.len() / crate::flex::WIDTH * 2 + 1,
            "flex area count"
        );
        ensure!(
            self.flex_area_off.first() == Some(&0)
                && self.flex_area_off.last().copied() == Some(self.flex_area_stops.len() as u32)
                && self.flex_area_off.windows(2).all(|w| w[0] < w[1]),
            "flex area offsets"
        );
        ensure!(
            self.route_stop_off.len() == nr + 1 && self.route_trip_off.len() == nr + 1,
            "route offsets"
        );
        ensure!(
            self.route_stop_off[nr] as usize == self.route_stops.len(),
            "route stops length"
        );
        ensure!(
            self.route_flags.len() == self.route_stops.len(),
            "route flags length"
        );
        ensure!(
            self.route_trip_off[nr] as usize == nt && self.trip_bits.len() == nt,
            "trip count"
        );
        ensure!(
            self.dep.len() == self.arr.len() && self.route_time_off[nr] as usize == self.dep.len(),
            "times length"
        );
        for r in 0..nr {
            let n = self.route_stop_off[r + 1] - self.route_stop_off[r];
            let t = u64::from(self.route_trip_off[r + 1] - self.route_trip_off[r]);
            ensure!(n >= 2 && t >= 1, "route {r} degenerate");
            ensure!(
                self.route_time_off[r + 1] - self.route_time_off[r] == n * t,
                "route {r} times size"
            );
            ensure!((self.route_tz[r] as usize) < self.tzs.len(), "route tz");
        }
        ensure!(
            self.stop_change.len() == ns && self.stop_tz.len() == ns,
            "stop arrays"
        );
        Ok(())
    }

    /// Checksums plus element range checks (reads every array once; `open`
    /// only checks sizes and offsets so start-up does not page in the data).
    pub fn verify(&self) -> Result<()> {
        crate::store::verify(&self.dir, &self.meta.manifest)?;
        let (ns, nr) = (self.meta.n_stops, self.meta.n_routes);
        ensure!(
            self.stop_route.iter().all(|&r| (r as usize) < nr),
            "stop_route out of range"
        );
        ensure!(
            self.foot_to
                .iter()
                .chain(self.lb_in_from.iter())
                .chain(self.route_stops.iter())
                .all(|&s| (s as usize) < ns),
            "stop index out of range"
        );
        let nb = self.bitfields.len() / self.words.max(1);
        ensure!(
            self.trip_bits.iter().all(|&b| (b as usize) < nb),
            "bitfield index out of range"
        );
        let nf = self.flex.len() / crate::flex::WIDTH;
        ensure!(
            self.flex_pickup.iter().all(|&i| (i as usize) < nf),
            "flex pickup index"
        );
        ensure!(
            self.flex_area_stops.iter().all(|&i| (i as usize) < ns),
            "flex stop index"
        );
        for f in self.flex.as_chunks::<{ crate::flex::WIDTH }>().0 {
            ensure!(
                f[0] <= f[1] && f[2] <= f[3] && f[4] >= crate::flex::BUFFER,
                "flex windows/duration"
            );
            ensure!(
                (f[5] as usize) < nb
                    && (f[6] as usize) < self.tzs.len()
                    && (f[7] as usize) < self.meta.n_route_infos,
                "flex metadata index"
            );
            ensure!(
                (f[8] as usize) < nf * 2 && (f[9] as usize) < nf * 2,
                "flex area index"
            );
        }
        for r in 0..nr {
            for pos in 0..self.n_route_stops(r) {
                ensure!(
                    self.deps(r, pos).windows(2).all(|w| w[0] <= w[1]),
                    "route {r} not sorted at {pos}"
                );
            }
        }
        Ok(())
    }

    pub fn mapped_bytes(&self) -> u64 {
        self.meta
            .manifest
            .values()
            .map(|i| i.len * i.elem_size as u64)
            .sum::<u64>()
            + self.catalog.file_bytes()
    }

    #[inline]
    pub fn active(&self, bits: u32, day: usize) -> bool {
        day < self.days
            && self.bitfields[bits as usize * self.words + day / 64] >> (day % 64) & 1 == 1
    }

    pub fn flex_service(&self, i: u32) -> &[u32] {
        &self.flex[i as usize * crate::flex::WIDTH..(i as usize + 1) * crate::flex::WIDTH]
    }
    pub fn flex_area(&self, a: u32) -> &[u32] {
        &self.flex_area_stops
            [self.flex_area_off[a as usize] as usize..self.flex_area_off[a as usize + 1] as usize]
    }
    pub fn n_route_stops(&self, r: usize) -> usize {
        (self.route_stop_off[r + 1] - self.route_stop_off[r]) as usize
    }
    pub fn route_stop(&self, r: usize, pos: usize) -> u32 {
        self.route_stops[self.route_stop_off[r] as usize + pos]
    }
    pub fn route_flag(&self, r: usize, pos: usize) -> u8 {
        self.route_flags[self.route_stop_off[r] as usize + pos]
    }
    /// Departure times at `pos` of all trips of route `r` (ascending).
    pub fn deps(&self, r: usize, pos: usize) -> &[u32] {
        let n = (self.route_trip_off[r + 1] - self.route_trip_off[r]) as usize;
        let a = self.route_time_off[r] as usize + pos * n;
        &self.dep[a..a + n]
    }
    pub fn arrs(&self, r: usize, pos: usize) -> &[u32] {
        let n = (self.route_trip_off[r + 1] - self.route_trip_off[r]) as usize;
        let a = self.route_time_off[r] as usize + pos * n;
        &self.arr[a..a + n]
    }
    /// Whether any trip serves stop `s` on a service day in `lo..=hi`
    /// (bit indices; conservative helper for the "no service" pre-check).
    pub fn served_between(&self, s: usize, lo: usize, hi: usize) -> bool {
        let b = self.stop_days[s];
        b != crate::build::NONE
            && (lo..=hi.min(self.days.saturating_sub(1))).any(|d| self.active(b, d))
    }
    pub fn served(&self, s: usize) -> bool {
        self.stop_days[s] != crate::build::NONE
    }
    pub fn footpaths(&self, s: u32) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.footpaths_flagged(s).map(|(t, secs, _)| (t, secs))
    }

    /// Footpaths with their transfers.txt origin (`true` = explicit pair).
    pub fn footpaths_flagged(&self, s: u32) -> impl Iterator<Item = (u32, u32, bool)> + '_ {
        let (a, b) = (
            self.foot_off[s as usize] as usize,
            self.foot_off[s as usize + 1] as usize,
        );
        self.foot_to[a..b]
            .iter()
            .copied()
            .zip(self.foot_secs[a..b].iter().copied())
            .map(|(t, x)| (t, x & !FOOT_EXPLICIT, x & FOOT_EXPLICIT != 0))
    }
}
