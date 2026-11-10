//! Read-only P4K (CIG zip64 variant) index and entry reader.
//!
//! Handles the zip64 end-of-central-directory, store (0), deflate (8) and CIG's zstd (100) methods.  The whole central directory is
//! indexed once (about 1.4 million entries; the central directory is memory mapped and its records are parsed in parallel) so
//! entries can be found by name and read on demand.  A few small script/XML files (4,276, flagged in the central directory's extra
//! block) are stored encrypted: they are decrypted with the key found in the game executable (see crypto.rs); without the
//! executable they fail with `Error::Unreadable`.

use std::collections::HashMap;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use rayon::prelude::*;

use crate::crypto;

const EOCD: &[u8] = b"PK\x05\x06";
const LOC64: &[u8] = b"PK\x06\x07";
const CDFH: &[u8] = b"PK\x01\x02";
const LFH: &[u8] = b"PK\x03\x04";
const ZSTD_MAGIC: &[u8] = b"\x28\xb5\x2f\xfd";

/// Not a zip flag: set from the extra block (crypto.rs); zip bit 0 is 0 for every entry of the archive.
pub const ENCRYPTED: u32 = 0x10000;

#[derive(Debug)]
pub enum Error {
    /// Entry payload is not a plain stored / deflate / zstd stream (e.g. stored in the archive's encrypted form, or the key
    /// is not available).
    Unreadable(String),
    /// Any other malformed entry or I/O failure.
    Bad(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unreadable(s) | Error::Bad(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: Arc<str>,
    pub method: u16,
    pub flags: u32,
    pub comp_size: u64,
    pub size: u64,
    pub offset: u64,
}

impl Entry {
    pub fn encrypted(&self) -> bool {
        self.flags & (1 | ENCRYPTED) != 0
    }
}

/// Lowercase with an ASCII fast path (the archive's names are ASCII).
pub fn lower(s: &str) -> String {
    if s.is_ascii() {
        s.to_ascii_lowercase()
    } else {
        s.to_lowercase()
    }
}

fn ends_with_ci(name: &str, ext: &str) -> bool {
    let (n, e) = (name.as_bytes(), ext.as_bytes());
    if e.is_ascii() {
        n.len() >= e.len() && n[n.len() - e.len()..].eq_ignore_ascii_case(e)
    } else {
        lower(name).ends_with(ext)
    }
}

/// `{lowercase path: entry index}` for the entries ending with one extension, plus the lowercase paths in archive order.
pub struct LowerIndex {
    pub map: HashMap<String, usize>,
    /// Lowercase paths in the order of first appearance (insertion order).
    pub order: Vec<String>,
}

pub struct P4k {
    pub path: PathBuf,
    mmap: memmap2::Mmap,
    /// Entries in archive order (a repeated name keeps its first position and its last record).
    pub entries: Vec<Entry>,
    by_name: HashMap<Arc<str>, usize>,
    lower: Mutex<HashMap<String, Arc<LowerIndex>>>,
    by_base: Mutex<HashMap<String, Arc<HashMap<String, Vec<String>>>>>,
    key: OnceLock<std::result::Result<[u8; 16], String>>,
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn parse_record(cd: &[u8], pos: usize) -> Entry {
    let flags_zip = u16_at(cd, pos + 8) as u32;
    let method = u16_at(cd, pos + 10);
    let mut comp = u32_at(cd, pos + 20) as u64;
    let mut size = u32_at(cd, pos + 24) as u64;
    let nlen = u16_at(cd, pos + 28) as usize;
    let elen = u16_at(cd, pos + 30) as usize;
    let mut off = u32_at(cd, pos + 42) as u64;
    let name = String::from_utf8_lossy(&cd[pos + 46..pos + 46 + nlen]).replace('\\', "/");
    let extra = &cd[pos + 46 + nlen..pos + 46 + nlen + elen];
    let mut flags = flags_zip;
    const SAT: u64 = 0xFFFF_FFFF;
    if comp == SAT || size == SAT || off == SAT {
        let mut ep = 0;
        while ep + 4 <= extra.len() {
            let tag = u16_at(extra, ep);
            let tlen = u16_at(extra, ep + 2) as usize;
            if tag == 1 {
                // zip64 extended information, fields present only for the saturated values
                let mut vo = ep + 4;
                if size == SAT && vo + 8 <= extra.len() {
                    size = u64_at(extra, vo);
                    vo += 8;
                }
                if comp == SAT && vo + 8 <= extra.len() {
                    comp = u64_at(extra, vo);
                    vo += 8;
                }
                if off == SAT && vo + 8 <= extra.len() {
                    off = u64_at(extra, vo);
                }
            }
            ep += 4 + tlen;
        }
    }
    if extra.len() == crypto::EXTRA_LEN && u16_at(extra, crypto::FLAG_OFFSET) == 1 {
        flags |= ENCRYPTED;
    }
    Entry { name: Arc::from(name), method, flags, comp_size: comp, size, offset: off }
}

impl P4k {
    pub fn open(path: impl AsRef<Path>) -> Result<P4k> {
        let path = path.as_ref().to_path_buf();
        let f = std::fs::File::open(&path).map_err(|e| Error::Bad(format!("{}: {}", path.display(), e)))?;
        let mmap = unsafe { memmap2::Mmap::map(&f) }.map_err(|e| Error::Bad(format!("{}: {}", path.display(), e)))?;
        let (entries, by_name) = Self::index(&mmap)?;
        Ok(P4k {
            path,
            mmap,
            entries,
            by_name,
            lower: Mutex::new(HashMap::new()),
            by_base: Mutex::new(HashMap::new()),
            key: OnceLock::new(),
        })
    }

    // ---------------------------------------------------------------- index
    fn index(m: &[u8]) -> Result<(Vec<Entry>, HashMap<Arc<str>, usize>)> {
        let bad = |s: &str| Error::Bad(s.to_string());
        let end = m.len();
        let start = end.saturating_sub(128 * 1024);
        let tail = &m[start..end];
        let rel = tail.windows(4).rposition(|w| w == EOCD).ok_or_else(|| bad("no end of central directory"))?;
        let eocd = start + rel;
        if eocd + 22 > end {
            return Err(bad("truncated end of central directory"));
        }
        let mut cd_size = u32_at(m, eocd + 12) as u64;
        let mut cd_off = u32_at(m, eocd + 16) as u64;
        if eocd >= 20 && &m[eocd - 20..eocd - 16] == LOC64 {
            let eocd64_off = u64_at(m, eocd - 20 + 8) as usize;
            if eocd64_off + 56 > end {
                return Err(bad("bad zip64 end of central directory"));
            }
            cd_size = u64_at(m, eocd64_off + 40);
            cd_off = u64_at(m, eocd64_off + 48);
        }
        let cd_end = (cd_off + cd_size).min(end as u64) as usize;
        let cd = &m[(cd_off as usize).min(end)..cd_end];
        // pass 1: record starts (cheap, sequential); pass 2: parse the records in parallel
        let mut starts = Vec::with_capacity(1 << 21);
        let mut pos = 0usize;
        while pos + 46 <= cd.len() && &cd[pos..pos + 4] == CDFH {
            starts.push(pos);
            pos += 46 + u16_at(cd, pos + 28) as usize + u16_at(cd, pos + 30) as usize + u16_at(cd, pos + 32) as usize;
        }
        let parsed: Vec<Entry> = starts.par_iter().filter(|&&p| p + 46 + u16_at(cd, p + 28) as usize + u16_at(cd, p + 30) as usize <= cd.len())
            .map(|&p| parse_record(cd, p))
            .collect();
        let mut entries: Vec<Entry> = Vec::with_capacity(parsed.len());
        let mut by_name: HashMap<Arc<str>, usize> = HashMap::with_capacity(parsed.len());
        for e in parsed {
            match by_name.get(&e.name) {
                Some(&i) => entries[i] = e,
                None => {
                    by_name.insert(e.name.clone(), entries.len());
                    entries.push(e);
                }
            }
        }
        Ok((entries, by_name))
    }

    // ---------------------------------------------------------------- lookups
    /// Entry by exact (case-sensitive) archive name.
    pub fn get(&self, name: &str) -> Option<&Entry> {
        self.by_name.get(name).map(|&i| &self.entries[i])
    }

    pub fn contains(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// Entries whose lowercase path contains `needle` (and ends with one of `suffixes`, lowercase).
    pub fn find<'a>(&'a self, needle: &str, suffixes: &'a [&str]) -> impl Iterator<Item = &'a Entry> + 'a {
        let needle = lower(&needle.replace('\\', "/"));
        self.entries.iter().filter(move |e| {
            let low = lower(&e.name);
            low.contains(&needle) && (suffixes.is_empty() || suffixes.iter().any(|s| low.ends_with(s)))
        })
    }

    /// Entries ending with `ext` (lowercase), by lowercase path; built once per extension.
    pub fn lower_index(&self, ext: &str) -> Arc<LowerIndex> {
        // The index is built WITHOUT holding the lock: the build runs a rayon `par_iter`, and a rayon worker that is itself inside a
        // `find_path` (texture prefetch, parallel tests) would otherwise block on the lock while the lock holder waits for that
        // worker (a deadlock seen with concurrent tests).  Two threads may build the same index once; the first insert wins.
        if let Some(i) = self.lower.lock().unwrap().get(ext) {
            return i.clone();
        }
        let hits: Vec<(String, usize)> = self
            .entries
            .par_iter()
            .enumerate()
            .filter(|(_, e)| ends_with_ci(&e.name, ext))
            .map(|(i, e)| (lower(&e.name), i))
            .collect();
        let mut map = HashMap::with_capacity(hits.len());
        let mut order = Vec::with_capacity(hits.len());
        for (l, i) in hits {
            if map.insert(l.clone(), i).is_none() {
                order.push(l);
            }
        }
        let idx = Arc::new(LowerIndex { map, order });
        self.lower.lock().unwrap().entry(ext.to_string()).or_insert(idx).clone()
    }

    /// `{lowercase file name: [lowercase paths]}` for every entry ending with `ext` (built once per extension).
    pub fn basename_index(&self, ext: &str) -> Arc<HashMap<String, Vec<String>>> {
        if let Some(i) = self.by_base.lock().unwrap().get(ext) {
            return i.clone();
        }
        let li = self.lower_index(ext);
        let mut idx: HashMap<String, Vec<String>> = HashMap::new();
        for path in &li.order {
            idx.entry(path.rsplit('/').next().unwrap().to_string()).or_default().push(path.clone());
        }
        let idx = Arc::new(idx);
        self.by_base.lock().unwrap().insert(ext.to_string(), idx.clone());
        idx
    }

    /// Case-insensitive lookup of 'Data/<rel_path>' among entries ending with `ext`.
    ///
    /// `rel_path` may use backslashes and may or may not already start with 'data/'.  Engine assets are the exception: a path that
    /// starts with 'EngineAssets/' or 'Engine/' is looked up under 'Engine/EngineAssets/' ('Engine/...').  EMPIRICAL: 800 texture
    /// slots of the archive's .mtl files name 'EngineAssets/textures/perlinnoise2d.dds', '.../screeninterference/pixel_layout_crt.tif'
    /// and the like (the Carrack holo table's noise map among them), which exist only as 'Engine/EngineAssets/...'; no entry lives
    /// under 'Data/Engine*' (the engine root is an alias of its own, INFERRED from that layout).
    pub fn find_path(&self, rel_path: &str, ext: &str) -> Option<&Entry> {
        let mut rel = lower(rel_path.replace('\\', "/").trim_start_matches('/'));
        if rel.starts_with("engineassets/") {
            rel = format!("engine/{}", rel);
        } else if !(rel.starts_with("data/") || rel.starts_with("engine/")) {
            rel = format!("data/{}", rel);
        }
        let li = self.lower_index(ext);
        li.map.get(&rel).map(|&i| &self.entries[i])
    }

    // ---------------------------------------------------------------- reading
    fn archive_key(&self, entry: &Entry) -> Result<[u8; 16]> {
        let k = self.key.get_or_init(|| crypto::find_key(&crypto::exe_path(&self.path)));
        match k {
            Ok(k) => Ok(*k),
            Err(e) => Err(Error::Unreadable(format!("encrypted entry {}: {}", entry.name, e))),
        }
    }

    /// The entry's stored bytes (compressed, still encrypted if flagged), straight from the memory map.
    pub fn raw(&self, entry: &Entry) -> Result<&[u8]> {
        let m = &self.mmap[..];
        let o = entry.offset as usize;
        if o.checked_add(30).map_or(true, |e| e > m.len()) || &m[o..o + 4] != LFH {
            return Err(Error::Bad(format!("bad local header: {}", entry.name)));
        }
        let start = o + 30 + u16_at(m, o + 26) as usize + u16_at(m, o + 28) as usize;
        let end = start.saturating_add(entry.comp_size as usize).min(m.len());
        Ok(&m[start.min(end)..end])
    }

    pub fn read(&self, entry: &Entry) -> Result<Vec<u8>> {
        let raw = self.raw(entry)?;
        let dec;
        let data: &[u8] = if entry.encrypted() {
            dec = crypto::decrypt(raw, &self.archive_key(entry)?);
            &dec
        } else {
            raw
        };
        match entry.method {
            0 => Ok(data.to_vec()),
            8 => {
                let mut out = Vec::with_capacity(entry.size as usize);
                flate2::read::DeflateDecoder::new(data).read_to_end(&mut out).map_err(|e| Error::Bad(format!("deflate {}: {}", entry.name, e)))?;
                Ok(out)
            }
            100 => {
                if data.len() < 4 || &data[..4] != ZSTD_MAGIC {
                    return Err(Error::Unreadable(format!("not a zstd frame (encrypted in the archive): {}", entry.name)));
                }
                if entry.encrypted() {
                    // zero padding follows the end of the zstd frame
                    let mut out = Vec::with_capacity(entry.size as usize);
                    let mut d = zstd::stream::read::Decoder::new(data).map_err(|e| Error::Bad(format!("zstd {}: {}", entry.name, e)))?.single_frame();
                    d.read_to_end(&mut out).map_err(|e| Error::Bad(format!("zstd {}: {}", entry.name, e)))?;
                    Ok(out)
                } else {
                    zstd::bulk::decompress(data, (entry.size as usize).max(1)).map_err(|e| Error::Bad(format!("zstd {}: {}", entry.name, e)))
                }
            }
            m => Err(Error::Bad(format!("unsupported method {}: {}", m, entry.name))),
        }
    }
}
