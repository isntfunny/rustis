//! `catalog.bin`: the complete stop catalogue of a snapshot, read through a
//! read-only mapping instead of being held on the heap.
//!
//! Every stop of the feed is listed (also stops no trip serves in the
//! snapshot period); the routing graph uses the same indices. Lookups by id
//! use a sorted index (binary search), bounding-box searches a latitude-sorted
//! index. Coordinates are stored as `f64` bits (`NaN` = missing).
use crate::format::{self, CATALOG_MAGIC, CATALOG_SECTIONS, Range, Sec, Writer, le_u32, le_u64};
use crate::model::Stop;
use anyhow::{Context, Result, ensure};
use memmap2::Mmap;
use std::cmp::Ordering;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

const NO_PARENT: u32 = u32::MAX;

pub(crate) struct Catalog {
    map: Mmap,
    n: usize,
    sec: [Range; format::MAX_SECTION_ID],
}

/// Writes the catalogue; returns the file size.
pub(crate) fn write(stops: &[Stop], path: &Path) -> Result<u64> {
    let n = stops.len();
    ensure!(n < NO_PARENT as usize, "too many stops");
    let index: std::collections::HashMap<&str, u32> = stops
        .iter()
        .enumerate()
        .map(|(i, s)| (s.stop_id.as_str(), i as u32))
        .collect();
    let strings = |f: &dyn Fn(&Stop) -> &str| {
        let (mut offs, mut bytes) = (vec![0u64], Vec::new());
        for s in stops {
            bytes.extend_from_slice(f(s).as_bytes());
            offs.push(bytes.len() as u64);
        }
        (offs, bytes)
    };
    let (id_off, id_bytes) = strings(&|s| s.stop_id.as_str());
    let (name_off, name_bytes) = strings(&|s| s.name.as_str());
    let mut by_id: Vec<u32> = (0..n as u32).collect();
    by_id.sort_by(|&a, &b| {
        stops[a as usize]
            .stop_id
            .as_bytes()
            .cmp(stops[b as usize].stop_id.as_bytes())
    });
    let lat = |i: u32| stops[i as usize].lat.unwrap_or(f64::NAN);
    let mut by_lat: Vec<u32> = (0..n as u32).collect();
    by_lat.sort_by(|&a, &b| lat(a).total_cmp(&lat(b)));
    let mut w = Writer::default();
    w.u64s(Sec::CatIdOffsets, id_off);
    w.bytes(Sec::CatIdBytes, id_bytes);
    w.u64s(Sec::CatNameOffsets, name_off);
    w.bytes(Sec::CatNameBytes, name_bytes);
    w.u32s(
        Sec::CatParent,
        stops.iter().map(|s| {
            s.parent_station
                .as_deref()
                .and_then(|p| index.get(p).copied())
                .unwrap_or(NO_PARENT)
        }),
    );
    w.u64s(
        Sec::CatLat,
        stops.iter().map(|s| s.lat.unwrap_or(f64::NAN).to_bits()),
    );
    w.u64s(
        Sec::CatLon,
        stops.iter().map(|s| s.lon.unwrap_or(f64::NAN).to_bits()),
    );
    w.u32s(Sec::CatIdSorted, by_id);
    w.u32s(Sec::CatLatSorted, by_lat);
    let mut out = BufWriter::new(File::create(path)?);
    let bytes = w.write_to(&mut out, CATALOG_MAGIC)?;
    out.flush()?;
    out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    Ok(bytes)
}

impl Catalog {
    pub(crate) fn open(path: &Path, n: usize) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        // SAFETY: same invariant as timetable.bin – written once, read-only.
        let map = unsafe { Mmap::map(&file) }.context("mapping catalog.bin")?;
        let sec = format::parse_directory(&map, CATALOG_MAGIC, &CATALOG_SECTIONS)?;
        let c = Self { map, n, sec };
        for (offs, bytes) in [
            (Sec::CatIdOffsets, Sec::CatIdBytes),
            (Sec::CatNameOffsets, Sec::CatNameBytes),
        ] {
            let o = c.sec[offs as usize];
            ensure!(o.count == n + 1, "catalog {offs:?}: wrong count");
            let mut prev = 0;
            for i in 0..=n {
                let v = le_u64(&c.map, o.start + 8 * i);
                ensure!(
                    v >= prev && (i > 0 || v == 0),
                    "catalog {offs:?}: not monotone"
                );
                prev = v;
            }
            ensure!(
                prev as usize == c.sec[bytes as usize].count,
                "catalog {bytes:?}: length mismatch"
            );
        }
        for s in [
            Sec::CatParent,
            Sec::CatLat,
            Sec::CatLon,
            Sec::CatIdSorted,
            Sec::CatLatSorted,
        ] {
            ensure!(c.sec[s as usize].count == n, "catalog {s:?}: wrong count");
        }
        for s in [Sec::CatIdSorted, Sec::CatLatSorted] {
            let mut seen = vec![false; n];
            for k in 0..n {
                let i = c.u32_at(s, k) as usize;
                ensure!(
                    i < n && !std::mem::replace(&mut seen[i], true),
                    "catalog {s:?}: not a permutation"
                );
            }
        }
        for k in 0..n {
            let p = c.u32_at(Sec::CatParent, k);
            ensure!(
                p == NO_PARENT || (p as usize) < n,
                "catalog parent out of range"
            );
        }
        Ok(c)
    }

    pub(crate) fn file_bytes(&self) -> u64 {
        self.map.len() as u64
    }

    fn u32_at(&self, s: Sec, k: usize) -> u32 {
        le_u32(&self.map, self.sec[s as usize].start + 4 * k)
    }
    fn f64_at(&self, s: Sec, k: usize) -> f64 {
        f64::from_bits(le_u64(&self.map, self.sec[s as usize].start + 8 * k))
    }
    fn bytes(&self, offs: Sec, bytes: Sec, i: usize) -> &[u8] {
        let o = self.sec[offs as usize].start;
        let (a, b) = (
            le_u64(&self.map, o + 8 * i) as usize,
            le_u64(&self.map, o + 8 * (i + 1)) as usize,
        );
        let base = self.sec[bytes as usize].start;
        &self.map[base + a..base + b]
    }
    fn text(&self, offs: Sec, bytes: Sec, i: usize) -> String {
        String::from_utf8_lossy(self.bytes(offs, bytes, i)).into_owned()
    }

    pub(crate) fn coord(&self, i: usize) -> Option<[f64; 2]> {
        let (lat, lon) = (self.f64_at(Sec::CatLat, i), self.f64_at(Sec::CatLon, i));
        (lat.is_finite() && lon.is_finite()).then_some([lat, lon])
    }

    pub(crate) fn stop(&self, i: usize, served: bool) -> Stop {
        let [lat, lon] = self
            .coord(i)
            .map_or([None, None], |[a, b]| [Some(a), Some(b)]);
        let p = self.u32_at(Sec::CatParent, i);
        Stop {
            stop_id: self.text(Sec::CatIdOffsets, Sec::CatIdBytes, i),
            name: self.text(Sec::CatNameOffsets, Sec::CatNameBytes, i),
            lat,
            lon,
            parent_station: (p != NO_PARENT)
                .then(|| self.text(Sec::CatIdOffsets, Sec::CatIdBytes, p as usize)),
            served,
        }
    }

    /// Stop index for a GTFS stop id (binary search over the sorted index).
    pub(crate) fn lookup(&self, id: &str) -> Option<usize> {
        let (mut lo, mut hi) = (0, self.n);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let i = self.u32_at(Sec::CatIdSorted, mid) as usize;
            match self
                .bytes(Sec::CatIdOffsets, Sec::CatIdBytes, i)
                .cmp(id.as_bytes())
            {
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
                Ordering::Equal => return Some(i),
            }
        }
        None
    }

    /// Stop indices with `min_lat ≤ lat ≤ max_lat`, in latitude order.
    pub(crate) fn lat_band(&self, min_lat: f64, max_lat: f64) -> impl Iterator<Item = usize> + '_ {
        let lat_of =
            |k: usize| self.f64_at(Sec::CatLat, self.u32_at(Sec::CatLatSorted, k) as usize);
        let (mut lo, mut hi) = (0, self.n);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if lat_of(mid).total_cmp(&min_lat) == Ordering::Less {
                lo = mid + 1
            } else {
                hi = mid
            }
        }
        (lo..self.n)
            .take_while(move |&k| lat_of(k) <= max_lat)
            .map(move |k| self.u32_at(Sec::CatLatSorted, k) as usize)
    }
}
