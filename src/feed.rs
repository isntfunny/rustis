//! Streaming access to one GTFS feed (ZIP or directory).
//!
//! Members are read row by row through the `csv` crate; nothing is
//! materialised beyond the current record. Columns are looked up by name once
//! per file (`Cols`), tolerating a UTF-8 BOM, surrounding whitespace in
//! headers and missing optional columns.
use anyhow::{Context, Result, bail};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

pub enum Feed {
    Dir(PathBuf),
    Zip(zip::ZipArchive<File>),
}

/// Column positions of one CSV header.
pub struct Cols(Vec<(String, usize)>);

impl Cols {
    pub fn get(&self, name: &str) -> Option<usize> {
        self.0.iter().find(|(n, _)| n == name).map(|&(_, i)| i)
    }
}

/// A field of the current record, trimmed; `""` if the column is absent.
pub fn field(rec: &csv::ByteRecord, col: Option<usize>) -> &[u8] {
    col.and_then(|c| rec.get(c))
        .map_or(&[][..], |f| f.trim_ascii())
}

pub fn text(rec: &csv::ByteRecord, col: Option<usize>) -> String {
    String::from_utf8_lossy(field(rec, col)).into_owned()
}

impl Feed {
    pub fn open(path: &Path) -> Result<Self> {
        if path.is_dir() {
            Ok(Self::Dir(path.to_owned()))
        } else {
            let f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
            Ok(Self::Zip(zip::ZipArchive::new(f).with_context(|| {
                format!("reading zip {}", path.display())
            })?))
        }
    }

    /// Base names of all members (GTFS files may sit in a sub-directory).
    pub fn members(&self) -> Vec<String> {
        match self {
            Self::Dir(d) => std::fs::read_dir(d)
                .map(|it| {
                    it.filter_map(|e| e.ok())
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default(),
            Self::Zip(z) => z
                .file_names()
                .filter(|n| !n.ends_with('/'))
                .map(|n| n.rsplit('/').next().unwrap_or(n).to_owned())
                .collect(),
        }
    }

    pub fn bytes(&mut self, name: &str) -> Result<Option<Vec<u8>>> {
        let mut bytes = Vec::new();
        match self {
            Self::Dir(d) => {
                let p = d.join(name);
                if !p.exists() {
                    return Ok(None);
                }
                bytes = std::fs::read(p)?;
            }
            Self::Zip(z) => {
                let Some(full) = z
                    .file_names()
                    .find(|n| *n == name || n.ends_with(&format!("/{name}")))
                    .map(str::to_owned)
                else {
                    return Ok(None);
                };
                z.by_name(&full)?.read_to_end(&mut bytes)?;
            }
        }
        Ok(Some(bytes))
    }

    /// Streams the rows of `name` to `f(cols, record)`. Returns `false` if the
    /// file is absent.
    pub fn rows(
        &mut self,
        name: &str,
        mut f: impl FnMut(&Cols, &csv::ByteRecord) -> Result<()>,
    ) -> Result<bool> {
        let reader: Box<dyn Read + '_> = match self {
            Self::Dir(d) => {
                let p = d.join(name);
                if !p.exists() {
                    return Ok(false);
                }
                Box::new(File::open(p)?)
            }
            Self::Zip(z) => {
                let Some(full) = z
                    .file_names()
                    .find(|n| *n == name || n.ends_with(&format!("/{name}")))
                    .map(str::to_owned)
                else {
                    return Ok(false);
                };
                Box::new(z.by_name(&full)?)
            }
        };
        let mut rd = csv::ReaderBuilder::new()
            .flexible(true)
            .has_headers(true)
            .from_reader(reader);
        let header = rd
            .byte_headers()
            .with_context(|| format!("{name}: header"))?;
        let cols = Cols(
            header
                .iter()
                .enumerate()
                .map(|(i, h)| {
                    let h = String::from_utf8_lossy(h);
                    (h.trim_start_matches('\u{feff}').trim().to_owned(), i)
                })
                .collect(),
        );
        let mut rec = csv::ByteRecord::new();
        loop {
            match rd.read_byte_record(&mut rec) {
                Ok(true) => f(&cols, &rec).with_context(|| {
                    format!("{name}: line {}", rec.position().map_or(0, |p| p.line()))
                })?,
                Ok(false) => break,
                Err(e) => bail!("{name}: {e}"),
            }
        }
        Ok(true)
    }
}

/// `HH:MM:SS` (hours may exceed 24) → seconds; `None` for an empty field.
pub fn parse_time(s: &[u8]) -> Result<Option<u32>> {
    if s.is_empty() {
        return Ok(None);
    }
    let mut parts = [0u32; 3];
    let mut n = 0;
    for p in s.split(|&b| b == b':') {
        anyhow::ensure!(
            n < 3 && !p.is_empty(),
            "bad time {:?}",
            String::from_utf8_lossy(s)
        );
        let p = p.trim_ascii();
        let mut v = 0u32;
        for &b in p {
            anyhow::ensure!(
                b.is_ascii_digit(),
                "bad time {:?}",
                String::from_utf8_lossy(s)
            );
            v = v * 10 + u32::from(b - b'0');
        }
        parts[n] = v;
        n += 1;
    }
    anyhow::ensure!(
        n == 3 && parts[1] < 60 && parts[2] < 60,
        "bad time {:?}",
        String::from_utf8_lossy(s)
    );
    Ok(Some(parts[0] * 3600 + parts[1] * 60 + parts[2]))
}

/// `YYYYMMDD` → days since 1970-01-01.
pub fn parse_date(s: &[u8]) -> Result<i32> {
    anyhow::ensure!(
        s.len() == 8 && s.iter().all(u8::is_ascii_digit),
        "bad date {:?}",
        String::from_utf8_lossy(s)
    );
    let n = |a: usize, b: usize| {
        s[a..b]
            .iter()
            .fold(0i32, |v, &d| v * 10 + i32::from(d - b'0'))
    };
    let d = jiff::civil::Date::new(n(0, 4) as i16, n(4, 6) as i8, n(6, 8) as i8)
        .with_context(|| format!("bad date {:?}", String::from_utf8_lossy(s)))?;
    Ok(day_number(d))
}

pub fn day_number(d: jiff::civil::Date) -> i32 {
    (d - jiff::civil::date(1970, 1, 1)).get_days()
}

pub fn date_of(day: i32) -> jiff::civil::Date {
    jiff::civil::date(1970, 1, 1) + jiff::Span::new().days(i64::from(day))
}
