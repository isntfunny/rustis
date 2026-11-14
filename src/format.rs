//! `timetable.bin`: a little-endian, sectioned, read-only file.
//!
//! Layout: `MAGIC` (8 bytes), format version (u32), section count (u32),
//! then one 24-byte directory entry per section
//! `(id: u32, elem_size: u32, offset: u64, count: u64)`. Section payloads
//! are 8-byte aligned. Values are always decoded with `from_le_bytes`; the
//! mapping is never reinterpreted as Rust types, so corrupt data can produce
//! wrong answers at worst (and is range-checked at open), not undefined
//! behaviour.
use anyhow::{Context, Result, bail, ensure};
use std::io::Write;

pub const MAGIC: &[u8; 8] = b"RUSTISTT";
pub const FORMAT_VERSION: u32 = 2;
const DIR_ENTRY: usize = 24;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Sec {
    RouteStopOffsets = 1,
    RouteStops = 2,
    RouteTripOffsets = 3,
    RouteCellOffsets = 4,
    RouteGtfs = 5,
    StopRouteOffsets = 6,
    StopRoutes = 7,
    FootOffsets = 8,
    FootTo = 9,
    FootSecs = 10,
    TripRoute = 11,
    Arrivals = 12,
    Departures = 13,
    Flags = 14,
    TripIdOffsets = 15,
    TripIdBytes = 16,
    StopChangeSecs = 17,
    // catalog.bin
    CatIdOffsets = 20,
    CatIdBytes = 21,
    CatNameOffsets = 22,
    CatNameBytes = 23,
    CatParent = 24,
    CatLat = 25,
    CatLon = 26,
    CatIdSorted = 27,
    CatLatSorted = 28,
}

pub const ALL_SECTIONS: [Sec; 17] = [
    Sec::RouteStopOffsets,
    Sec::RouteStops,
    Sec::RouteTripOffsets,
    Sec::RouteCellOffsets,
    Sec::RouteGtfs,
    Sec::StopRouteOffsets,
    Sec::StopRoutes,
    Sec::FootOffsets,
    Sec::FootTo,
    Sec::FootSecs,
    Sec::TripRoute,
    Sec::Arrivals,
    Sec::Departures,
    Sec::Flags,
    Sec::TripIdOffsets,
    Sec::TripIdBytes,
    Sec::StopChangeSecs,
];

pub const CATALOG_MAGIC: &[u8; 8] = b"RUSTISCT";
pub const CATALOG_SECTIONS: [Sec; 9] = [
    Sec::CatIdOffsets,
    Sec::CatIdBytes,
    Sec::CatNameOffsets,
    Sec::CatNameBytes,
    Sec::CatParent,
    Sec::CatLat,
    Sec::CatLon,
    Sec::CatIdSorted,
    Sec::CatLatSorted,
];

impl Sec {
    pub fn elem_size(self) -> u32 {
        match self {
            Sec::RouteCellOffsets
            | Sec::TripIdOffsets
            | Sec::CatIdOffsets
            | Sec::CatNameOffsets
            | Sec::CatLat
            | Sec::CatLon => 8,
            Sec::Flags | Sec::TripIdBytes | Sec::CatIdBytes | Sec::CatNameBytes => 1,
            _ => 4,
        }
    }
}

pub const FLAG_NO_PICKUP: u8 = 1;
pub const FLAG_NO_DROP_OFF: u8 = 2;
pub const FLAG_PICKUP_BOOKING: u8 = 4;
pub const FLAG_DROP_OFF_BOOKING: u8 = 8;

/// Buffers sections and writes them in one pass.
#[derive(Default)]
pub struct Writer {
    sections: Vec<(Sec, Vec<u8>)>,
}

impl Writer {
    pub fn u32s(&mut self, sec: Sec, values: impl IntoIterator<Item = u32>) {
        debug_assert_eq!(sec.elem_size(), 4);
        let bytes = values.into_iter().flat_map(u32::to_le_bytes).collect();
        self.sections.push((sec, bytes));
    }
    pub fn u64s(&mut self, sec: Sec, values: impl IntoIterator<Item = u64>) {
        debug_assert_eq!(sec.elem_size(), 8);
        let bytes = values.into_iter().flat_map(u64::to_le_bytes).collect();
        self.sections.push((sec, bytes));
    }
    pub fn bytes(&mut self, sec: Sec, values: Vec<u8>) {
        debug_assert_eq!(sec.elem_size(), 1);
        self.sections.push((sec, values));
    }

    pub fn write_to(mut self, out: &mut impl Write, magic: &[u8; 8]) -> Result<u64> {
        self.sections.sort_by_key(|(s, _)| *s);
        let header = 16 + DIR_ENTRY * self.sections.len();
        let mut offset = align8(header as u64);
        let mut dir = Vec::with_capacity(header);
        dir.extend_from_slice(magic);
        dir.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        dir.extend_from_slice(&(self.sections.len() as u32).to_le_bytes());
        for (sec, data) in &self.sections {
            let size = sec.elem_size();
            dir.extend_from_slice(&(*sec as u32).to_le_bytes());
            dir.extend_from_slice(&size.to_le_bytes());
            dir.extend_from_slice(&offset.to_le_bytes());
            dir.extend_from_slice(&(data.len() as u64 / u64::from(size)).to_le_bytes());
            offset = align8(offset + data.len() as u64);
        }
        out.write_all(&dir)?;
        let mut written = dir.len() as u64;
        for (_, data) in &self.sections {
            let pad = align8(written) - written;
            out.write_all(&[0u8; 8][..pad as usize])?;
            out.write_all(data)?;
            written = align8(written) + data.len() as u64;
        }
        Ok(written)
    }
}

fn align8(n: u64) -> u64 {
    n.div_ceil(8) * 8
}

/// Validated byte ranges of each section within the file.
#[derive(Clone, Copy, Debug, Default)]
pub struct Range {
    pub start: usize,
    pub count: usize,
}

pub const MAX_SECTION_ID: usize = 32;

pub fn parse_directory(
    file: &[u8],
    magic: &[u8; 8],
    expected: &[Sec],
) -> Result<[Range; MAX_SECTION_ID]> {
    ensure!(
        file.len() >= 16,
        "timetable file truncated ({} bytes)",
        file.len()
    );
    ensure!(
        &file[..8] == magic,
        "not a Rustis timetable file (bad magic)"
    );
    let version = le_u32(file, 8);
    ensure!(
        version == FORMAT_VERSION,
        "unsupported timetable format version {version}"
    );
    let n = le_u32(file, 12) as usize;
    ensure!(n == expected.len(), "unexpected section count {n}");
    ensure!(
        file.len() >= 16 + n * DIR_ENTRY,
        "section directory truncated"
    );
    let mut ranges = [Range::default(); MAX_SECTION_ID];
    let mut seen = [false; MAX_SECTION_ID];
    for i in 0..n {
        let at = 16 + i * DIR_ENTRY;
        let id = le_u32(file, at);
        let Some(sec) = expected.iter().copied().find(|s| *s as u32 == id) else {
            bail!("unknown section id {id}");
        };
        ensure!(!seen[id as usize], "duplicate section {sec:?}");
        seen[id as usize] = true;
        ensure!(
            le_u32(file, at + 4) == sec.elem_size(),
            "bad element size for {sec:?}"
        );
        let offset = le_u64(file, at + 8);
        let count = le_u64(file, at + 16);
        let len = count
            .checked_mul(u64::from(sec.elem_size()))
            .context("section length overflow")?;
        let end = offset.checked_add(len).context("section end overflow")?;
        ensure!(
            end <= file.len() as u64,
            "section {sec:?} exceeds file (truncated or corrupt)"
        );
        ensure!(offset.is_multiple_of(8), "section {sec:?} misaligned");
        ranges[id as usize] = Range {
            start: offset as usize,
            count: count as usize,
        };
    }
    Ok(ranges)
}

#[inline]
pub fn le_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4-byte slice"))
}
#[inline]
pub fn le_u64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8-byte slice"))
}

/// Streaming FNV-1a 64 (same result as [`fnv64`]).
pub struct Fnv64(pub u64);
impl Default for Fnv64 {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}
impl std::io::Write for Fnv64 {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        for &b in buf {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// FNV-1a 64-bit; stable across builds, cheap, sufficient to detect accidental
/// corruption (not tampering).
pub fn fnv64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}
