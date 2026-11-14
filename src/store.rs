//! One flat little-endian array per file, written streaming and read through a
//! read-only memory map. The snapshot's `manifest.json` lists every array
//! with element size, length and FNV-1a checksum (checked by `verify`).
use anyhow::{Context, Result, ensure};
use memmap2::Mmap;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

#[cfg(not(target_endian = "little"))]
compile_error!("Rustis snapshots are little-endian and mapped in place");

/// Plain integer element types that may be viewed in place.
pub trait Elem: Copy + Default + 'static {
    const SIZE: usize;
    fn put(self, out: &mut Vec<u8>);
}
macro_rules! elem {
    ($($t:ty),*) => {$(
        impl Elem for $t {
            const SIZE: usize = std::mem::size_of::<$t>();
            fn put(self, out: &mut Vec<u8>) { out.extend_from_slice(&self.to_le_bytes()); }
        }
    )*};
}
elem!(u8, u16, u32, u64, i32, i64);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArrayInfo {
    pub elem_size: usize,
    pub len: u64,
    pub fnv64: String,
}

pub type Manifest = BTreeMap<String, ArrayInfo>;

/// Streaming writer of one array file.
pub struct ArrayWriter<T: Elem> {
    out: BufWriter<File>,
    buf: Vec<u8>,
    len: u64,
    fnv: u64,
    name: String,
    _t: std::marker::PhantomData<T>,
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

impl<T: Elem> ArrayWriter<T> {
    pub fn create(dir: &Path, name: &str) -> Result<Self> {
        let p = dir.join(name);
        Ok(Self {
            out: BufWriter::with_capacity(
                1 << 20,
                File::create(&p).with_context(|| format!("creating {}", p.display()))?,
            ),
            buf: Vec::with_capacity(1 << 16),
            len: 0,
            fnv: FNV_OFFSET,
            name: name.to_owned(),
            _t: std::marker::PhantomData,
        })
    }
    pub fn push(&mut self, v: T) -> Result<()> {
        v.put(&mut self.buf);
        self.len += 1;
        if self.buf.len() >= 1 << 16 {
            self.drain()?;
        }
        Ok(())
    }
    pub fn extend(&mut self, it: impl IntoIterator<Item = T>) -> Result<()> {
        for v in it {
            self.push(v)?;
        }
        Ok(())
    }
    pub fn len(&self) -> u64 {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    fn drain(&mut self) -> Result<()> {
        self.fnv = fnv(self.fnv, &self.buf);
        self.out.write_all(&self.buf)?;
        self.buf.clear();
        Ok(())
    }
    pub fn finish(mut self, manifest: &mut Manifest) -> Result<()> {
        self.drain()?;
        self.out.flush()?;
        self.out.get_ref().sync_all()?;
        manifest.insert(
            self.name,
            ArrayInfo {
                elem_size: T::SIZE,
                len: self.len,
                fnv64: format!("{:016x}", self.fnv),
            },
        );
        Ok(())
    }
}

/// Writes a whole slice as one array.
pub fn write_all<T: Elem>(
    dir: &Path,
    name: &str,
    data: &[T],
    manifest: &mut Manifest,
) -> Result<()> {
    let mut w = ArrayWriter::<T>::create(dir, name)?;
    w.extend(data.iter().copied())?;
    w.finish(manifest)
}

/// Read-only mapped array.
pub struct Array<T: Elem> {
    map: Option<Mmap>,
    len: usize,
    _t: std::marker::PhantomData<T>,
}

impl<T: Elem> Array<T> {
    pub fn open(dir: &Path, name: &str, manifest: &Manifest) -> Result<Self> {
        let info = manifest
            .get(name)
            .with_context(|| format!("manifest lacks {name}"))?;
        ensure!(info.elem_size == T::SIZE, "{name}: element size mismatch");
        let p = dir.join(name);
        let f = File::open(&p).with_context(|| format!("opening {}", p.display()))?;
        let bytes = f.metadata()?.len();
        ensure!(
            bytes == info.len * T::SIZE as u64,
            "{name}: file size {bytes} does not match manifest"
        );
        let map = if bytes == 0 {
            None
        } else {
            // SAFETY: snapshot files are written once, made read-only and never
            // modified afterwards (a new import writes a new directory).
            Some(unsafe { Mmap::map(&f) }.with_context(|| format!("mapping {name}"))?)
        };
        let a = Self {
            map,
            len: info.len as usize,
            _t: std::marker::PhantomData,
        };
        // Alignment: mappings are page aligned; checked once here.
        ensure!(a.as_slice().len() == a.len, "{name}: misaligned mapping");
        Ok(a)
    }
    pub fn as_slice(&self) -> &[T] {
        match &self.map {
            None => &[],
            Some(m) => {
                // SAFETY: T is a plain little-endian integer (Elem is only
                // implemented for those), the mapping is page aligned, and every
                // bit pattern is a valid value.
                let (pre, mid, _) = unsafe { m.align_to::<T>() };
                if pre.is_empty() {
                    &mid[..self.len.min(mid.len())]
                } else {
                    &[]
                }
            }
        }
    }
    pub fn bytes(&self) -> u64 {
        (self.len * T::SIZE) as u64
    }
}

impl<T: Elem> std::ops::Deref for Array<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

/// Recomputes every checksum of the manifest.
pub fn verify(dir: &Path, manifest: &Manifest) -> Result<()> {
    for (name, info) in manifest {
        let mut f = File::open(dir.join(name))?;
        let mut h = FNV_OFFSET;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = std::io::Read::read(&mut f, &mut buf)?;
            if n == 0 {
                break;
            }
            h = fnv(h, &buf[..n]);
        }
        ensure!(
            format!("{h:016x}") == info.fnv64,
            "{name}: checksum mismatch"
        );
    }
    Ok(())
}

pub fn path(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}
