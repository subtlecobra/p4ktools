//! Reader for the CIG '.dba' animation databases and stand-alone '.caf' clips.  Layout (CONFIRMED against
//! the engine / EMPIRICAL on the live archive):
//!
//! * container '#ivo' 0x900; chunk 0x194fbc50 = the '#dba' blocks back to back, chunk 0xf7351608 = the animation list (0x30-byte headers, last
//!   frame at +0x10, NUL separated lower-case paths); duration = last frame / 30.0 (CONFIRMED).
//! * '#dba' block = '#dba', u16 n, u16, u32, n x u32 CRC32 of the bone name, n x 24-byte joint records {rotation track header, position track
//!   header}, each header {u16 keys, u8 time format, u8 data format, u32 time offset, u32 data offset}, offsets relative to the RECORD.
//! * rotation formats 0x80 (4 x f32), 0x81 / 0x82 / 0x83 (smallest three, 10/15/21 bit; the dropped component's index is in the TOP two bits
//!   of the key word: EMPIRICAL); position formats 0xc0 / 0xc1 / 0xc2; time formats 0x40 / 0x41 / 0x42 (bit set).
//!
//! Malformed input is an `Err`.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use crate::cryxml;
use crate::error::*;
use crate::plainxml;

pub const IVO: u32 = 0x6F76_6923;
pub const CH_DATA: u32 = 0x194F_BC50;
pub const CH_NAMES: u32 = 0xF735_1608;
pub const FPS: f64 = 30.0;
const HEADER: usize = 0x30;
const FLT_MAX: f64 = 3.4028234663852886e38;
const SQRT_HALF: f64 = 0.70710677;

/// Decoded keys of one joint: (times in frames, values); None when the joint has no such track.
#[derive(Debug, Clone, Default)]
pub struct Track {
    pub rot: Option<(Vec<f64>, Vec<[f64; 4]>)>,
    pub pos: Option<(Vec<f64>, Vec<[f64; 3]>)>,
}

impl Track {
    pub fn sample(&self, frame: f64) -> (Option<[f64; 4]>, Option<[f64; 3]>) {
        (self.rot.as_ref().map(|(t, v)| sample_rot(t, v, frame)), self.pos.as_ref().map(|(t, v)| sample_pos(t, v, frame)))
    }
}

/// {bone CRC32: Track} in file order.
#[derive(Debug, Clone, Default)]
pub struct Tracks {
    pub order: Vec<u32>,
    pub map: HashMap<u32, Track>,
}

impl Tracks {
    pub fn get(&self, crc: u32) -> Option<&Track> {
        self.map.get(&crc)
    }
    /// (bone CRC32, track) in file order.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &Track)> {
        self.order.iter().map(move |c| (*c, &self.map[c]))
    }
    pub fn len(&self) -> usize {
        self.order.len()
    }
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
    fn put(&mut self, crc: u32, t: Track) {
        if self.map.insert(crc, t).is_none() {
            self.order.push(crc);
        }
    }
}

/// One animation: lower-case path, last frame, duration, bone CRC32s; the key tracks decode lazily from the shared file bytes.
#[derive(Debug)]
pub struct Clip {
    pub name: String,
    pub frames: u32,
    pub seconds: f64,
    pub bones: Vec<u32>,
    src: Option<(Arc<Vec<u8>>, usize)>,
    tracks: OnceLock<Tracks>,
}

impl Clip {
    pub fn tracks(&self) -> &Tracks {
        self.tracks.get_or_init(|| match &self.src {
            Some((blob, at)) => decode_block(blob, *at).unwrap_or_default(),
            None => Tracks::default(),
        })
    }
    fn built(name: String, frames: u32, bones: Vec<u32>, tracks: Tracks) -> Clip {
        let c = Clip { seconds: frames as f64 / FPS, name, frames, bones, src: None, tracks: OnceLock::new() };
        let _ = c.tracks.set(tracks);
        c
    }
}

type RotKeys = Option<(Vec<f64>, Vec<[f64; 4]>)>;
type PosKeys = Option<(Vec<f64>, Vec<[f64; 3]>)>;

/// The clip played backwards (ADB `speed="-1"`): every key time t becomes frames - t.
pub fn reverse_clip(c: &Clip) -> Clip {
    let f = c.frames as f64;
    let mut out = Tracks::default();
    for &b in &c.tracks().order {
        let t = &c.tracks().map[&b];
        let flip_r: RotKeys = t.rot.as_ref().map(|(ts, vs)| (ts.iter().rev().map(|x| f - x).collect(), vs.iter().rev().cloned().collect()));
        let flip_p: PosKeys = t.pos.as_ref().map(|(ts, vs)| (ts.iter().rev().map(|x| f - x).collect(), vs.iter().rev().cloned().collect()));
        out.put(b, Track { rot: flip_r, pos: flip_p });
    }
    Clip::built(c.name.clone(), c.frames, c.bones.clone(), out)
}

/// One Clip that plays `clips` one after the other (key times shifted by the frames before; the next clip starts 0.01 frame later).
pub fn concat_clips(clips: &[Arc<Clip>], name: &str) -> Clip {
    let mut off = 0.0f64;
    let mut cur: HashMap<u32, (RotKeys, PosKeys)> = HashMap::new();
    let mut order: Vec<u32> = vec![];
    for (k, c) in clips.iter().enumerate() {
        for &b in &c.tracks().order {
            let t = &c.tracks().map[&b];
            if !cur.contains_key(&b) {
                order.push(b);
            }
            let e = cur.entry(b).or_insert((None, None));
            if let Some((ts, vs)) = &t.rot {
                let (mut a, mut v) = e.0.take().unwrap_or_default();
                a.extend(ts.iter().map(|x| x + off + 0.01 * k as f64));
                v.extend(vs.iter().cloned());
                e.0 = Some((a, v));
            }
            if let Some((ts, vs)) = &t.pos {
                let (mut a, mut v) = e.1.take().unwrap_or_default();
                a.extend(ts.iter().map(|x| x + off + 0.01 * k as f64));
                v.extend(vs.iter().cloned());
                e.1 = Some((a, v));
            }
        }
        off += c.frames as f64;
    }
    let mut tracks = Tracks::default();
    for b in &order {
        let (r, p) = cur.remove(b).unwrap();
        tracks.put(*b, Track { rot: r, pos: p });
    }
    Clip::built(name.to_string(), off.round() as u32, order, tracks)
}

pub fn crc32(data: &[u8]) -> u32 {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *e = c;
        }
        t
    });
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = t[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

/// The id under which a node `name` appears in the clip's bone set: CRC32 of the name as written (case sensitive) or of its lower-case
/// form, whichever the clip holds.
pub fn bone_id(name: &str, ids: &std::collections::HashSet<u32>) -> Option<u32> {
    for n in [name.to_string(), name.to_lowercase()] {
        let c = crc32(n.as_bytes());
        if ids.contains(&c) {
            return Some(c);
        }
    }
    None
}

fn times(blob: &[u8], at: usize, n: usize, fmt: u8) -> Result<Vec<f64>> {
    match fmt {
        0x40 => (0..n).map(|i| Ok(u8_at(blob, at + i)? as f64)).collect(),
        0x41 => (0..n).map(|i| Ok(u16_at(blob, at + 2 * i)? as f64)).collect(),
        0x42 => {
            let first = u16_at(blob, at)? as usize;
            let last = u16_at(blob, at + 2)? as usize;
            if last < first {
                return Err(Error::msg("bad time bit set"));
            }
            let words = (last - first) / 16 + 1;
            let bits: Vec<u16> = (0..words).map(|i| u16_at(blob, at + 4 + 2 * i)).collect::<Result<_>>()?;
            let out: Vec<f64> = (0..=(last - first)).filter(|i| bits[i >> 4] >> (i & 15) & 1 != 0).map(|i| (first + i) as f64).collect();
            if out.len() != n {
                return Err(Error::msg(format!("time bit set has {} keys, header says {}", out.len(), n)));
            }
            Ok(out)
        }
        f => Err(Error::msg(format!("unknown time format 0x{:x}", f))),
    }
}

const SLOTS: [[usize; 3]; 4] = [[1, 2, 3], [0, 2, 3], [0, 1, 3], [0, 1, 2]];

fn smallest_three(raw: u64, widths: [u32; 3], bits: u32) -> [f64; 4] {
    let mut shift = 0u32;
    let mut comps = [0.0f64; 3];
    for (k, w) in widths.iter().enumerate() {
        let mask = (1u64 << w) - 1;
        comps[k] = ((raw >> shift) & mask) as f64 / mask as f64 * 1.4142135 - SQRT_HALF;
        shift += w;
    }
    let idx = ((raw >> (bits - 2)) & 3) as usize;
    let mut q = [0.0f64; 4];
    for (slot, v) in SLOTS[idx].iter().zip(comps.iter()) {
        q[*slot] = *v;
    }
    q[idx] = (1.0 - comps.iter().map(|v| v * v).sum::<f64>()).max(0.0).sqrt();
    q
}

fn rotations(blob: &[u8], at: usize, n: usize, fmt: u8) -> Result<Vec<[f64; 4]>> {
    match fmt {
        0x80 => (0..n)
            .map(|i| Ok([f32_at(blob, at + 16 * i)?, f32_at(blob, at + 16 * i + 4)?, f32_at(blob, at + 16 * i + 8)?, f32_at(blob, at + 16 * i + 12)?]))
            .collect(),
        0x81 | 0x82 | 0x83 => {
            let (widths, size): ([u32; 3], usize) = match fmt {
                0x81 => ([10, 10, 10], 4),
                0x82 => ([15, 15, 15], 6),
                _ => ([21, 21, 20], 8),
            };
            (0..n)
                .map(|i| {
                    let s = blob.get(at + size * i..at + size * i + size).ok_or_else(|| Error::msg("rotation data out of range"))?;
                    let mut raw = 0u64;
                    for (k, b) in s.iter().enumerate() {
                        raw |= (*b as u64) << (8 * k);
                    }
                    Ok(smallest_three(raw, widths, 8 * size as u32))
                })
                .collect()
        }
        f => Err(Error::msg(format!("unknown rotation format 0x{:x}", f))),
    }
}

fn positions(blob: &[u8], at: usize, n: usize, fmt: u8) -> Result<Vec<[f64; 3]>> {
    match fmt {
        0xC0 => (0..n).map(|i| Ok([f32_at(blob, at + 12 * i)?, f32_at(blob, at + 12 * i + 4)?, f32_at(blob, at + 12 * i + 8)?])).collect(),
        0xC1 | 0xC2 => {
            let sc = [f32_at(blob, at)?, f32_at(blob, at + 4)?, f32_at(blob, at + 8)?];
            let off = [f32_at(blob, at + 12)?, f32_at(blob, at + 16)?, f32_at(blob, at + 20)?];
            let mut at = at + 24;
            if fmt == 0xC1 {
                return (0..n)
                    .map(|i| {
                        let mut v = [0.0; 3];
                        for c in 0..3 {
                            v[c] = u16_at(blob, at + 6 * i + 2 * c)? as f64 * sc[c] + off[c];
                        }
                        Ok(v)
                    })
                    .collect();
            }
            let mut chans: Vec<Vec<f64>> = vec![];
            for c in 0..3 {
                if sc[c] >= FLT_MAX * 0.99 {
                    chans.push(vec![off[c]; n]);
                } else {
                    let v: Vec<f64> = (0..n).map(|i| Ok(u16_at(blob, at + 2 * i)? as f64 * sc[c] + off[c])).collect::<Result<_>>()?;
                    at += 2 * n;
                    chans.push(v);
                }
            }
            Ok((0..n).map(|i| [chans[0][i], chans[1][i], chans[2][i]]).collect())
        }
        f => Err(Error::msg(format!("unknown position format 0x{:x}", f))),
    }
}

/// {joint CRC32: Track} of the '#dba' / '#caf' block starting at blob[at].
pub fn decode_block(blob: &[u8], at: usize) -> Result<Tracks> {
    let magic = blob.get(at..at + 4).ok_or_else(|| Error::msg("no '#dba' block"))?;
    if magic != b"#dba" && magic != b"#caf" {
        return Err(Error::msg("no '#dba' block"));
    }
    let n = u16_at(blob, at + 4)? as usize;
    let rec0 = at + 12 + 4 * n;
    let mut out = Tracks::default();
    for i in 0..n {
        let crc = u32_at(blob, at + 12 + 4 * i)?;
        let rec = rec0 + 24 * i;
        let (mut rot, mut pos) = (None, None);
        for (slot, k) in [0usize, 12].iter().enumerate() {
            let cnt = u16_at(blob, rec + k)? as usize;
            let tf = u8_at(blob, rec + k + 2)?;
            let df = u8_at(blob, rec + k + 3)?;
            let to = u32_at(blob, rec + k + 4)? as usize;
            let dof = u32_at(blob, rec + k + 8)? as usize;
            if cnt == 0 {
                continue;
            }
            let ts = times(blob, rec + to, cnt, tf)?;
            if slot == 0 {
                rot = Some((ts, rotations(blob, rec + dof, cnt, df)?));
            } else {
                pos = Some((ts, positions(blob, rec + dof, cnt, df)?));
            }
        }
        out.put(crc, Track { rot, pos });
    }
    Ok(out)
}

fn find(times: &[f64], f: f64) -> (usize, f64) {
    if f <= times[0] {
        return (0, 0.0);
    }
    if f >= times[times.len() - 1] {
        return (times.len() - 1, 0.0);
    }
    let (mut lo, mut hi) = (0usize, times.len() - 1);
    while hi - lo > 1 {
        let mid = (lo + hi) / 2;
        if times[mid] <= f {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (lo, (f - times[lo]) / (times[hi] - times[lo]))
}

pub fn sample_pos(times: &[f64], vals: &[[f64; 3]], f: f64) -> [f64; 3] {
    let (i, t) = find(times, f);
    if t == 0.0 {
        return vals[i];
    }
    let (a, b) = (vals[i], vals[i + 1]);
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

/// nlerp along the shorter arc.
pub fn sample_rot(times: &[f64], vals: &[[f64; 4]], f: f64) -> [f64; 4] {
    let (i, t) = find(times, f);
    let a = vals[i];
    if t == 0.0 {
        return a;
    }
    let mut b = vals[i + 1];
    if a.iter().zip(b.iter()).map(|(x, y)| x * y).sum::<f64>() < 0.0 {
        b = [-b[0], -b[1], -b[2], -b[3]];
    }
    let mut q = [0.0; 4];
    for k in 0..4 {
        q[k] = a[k] + (b[k] - a[k]) * t;
    }
    let mut ln = q.iter().map(|v| v * v).sum::<f64>().sqrt();
    if ln == 0.0 {
        ln = 1.0;
    }
    [q[0] / ln, q[1] / ln, q[2] / ln, q[3] / ln]
}

fn chunks_of(data: &[u8]) -> Result<HashMap<u32, usize>> {
    if data.len() < 16 || u32_at(data, 0)? != IVO || u32_at(data, 4)? != 0x900 {
        return Err(Error::msg("not a version 0x900 #ivo file"));
    }
    let count = u32_at(data, 8)? as usize;
    let table = u32_at(data, 12)? as usize;
    let mut chunks = HashMap::new();
    for i in 0..count.min(10) {
        let t = u32_at(data, table + 16 * i)?;
        let off = u64_at(data, table + 16 * i + 8)? as usize;
        chunks.insert(t, off);
    }
    Ok(chunks)
}

fn split_names(data: &[u8], at: usize, n: usize) -> Vec<String> {
    let rest = data.get(at..).unwrap_or(&[]);
    rest.splitn(n + 1, |&c| c == 0).take(n).map(|s| String::from_utf8_lossy(s).to_lowercase()).collect()
}

/// The lower-case clip paths of a .dba without decoding its key data.
pub fn dba_names(data: &[u8]) -> Result<Vec<String>> {
    let chunks = chunks_of(data)?;
    let co = *chunks.get(&CH_NAMES).ok_or_else(|| Error::msg("no DBA name chunk"))?;
    let n = u32_at(data, co)? as usize;
    let names_off = u32_at(data, co + 4)? as usize;
    Ok(split_names(data, co + 8 + n * HEADER + names_off, n))
}

/// {lower-case clip path: Clip} of a .dba file.
pub struct Dba {
    pub order: Vec<String>,
    pub clips: HashMap<String, Arc<Clip>>,
}

impl Dba {
    pub fn get(&self, name: &str) -> Option<&Arc<Clip>> {
        self.clips.get(name)
    }
    /// (name, clip) in file order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Arc<Clip>)> {
        self.order.iter().map(move |n| (n, &self.clips[n]))
    }
    pub fn len(&self) -> usize {
        self.order.len()
    }
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

pub fn parse_dba(data: &[u8]) -> Result<Dba> {
    let chunks = chunks_of(data)?;
    let (cd, cn) = match (chunks.get(&CH_DATA), chunks.get(&CH_NAMES)) {
        (Some(a), Some(b)) => (*a, *b),
        _ => return Err(Error::msg("no DBA data / name chunk")),
    };
    let size = u32_at(data, cd)? as usize;
    let start = cd + 4;
    let blob: Arc<Vec<u8>> = Arc::new(data.get(start..(start + size).min(data.len())).unwrap_or(&[]).to_vec());
    let n_anim = u32_at(data, cn)? as usize;
    let names_off = u32_at(data, cn + 4)? as usize;
    let heads = cn + 8;
    let names = split_names(data, heads + n_anim * HEADER + names_off, n_anim);
    let (mut out, mut order, mut pos) = (HashMap::new(), vec![], 0usize);
    for i in 0..n_anim {
        if blob.get(pos..pos + 4) != Some(b"#dba") {
            return Err(Error::msg(format!("animation {}: no '#dba' block at {}", i, pos)));
        }
        let n = u16_at(&blob, pos + 4)? as usize;
        let bones: Vec<u32> = (0..n).map(|k| u32_at(&blob, pos + 12 + 4 * k)).collect::<Result<_>>()?;
        let frames = u32_at(data, heads + i * HEADER + 0x10)?;
        let name = names.get(i).cloned().unwrap_or_default();
        let c = Clip { seconds: frames as f64 / FPS, name: name.clone(), frames, bones, src: Some((blob.clone(), pos)), tracks: OnceLock::new() };
        if out.insert(name.clone(), Arc::new(c)).is_none() {
            order.push(name);
        }
        pos += n * 0x1C + 0xC;
    }
    Ok(Dba { order, clips: out })
}

/// The Clip of a stand-alone '.caf' (chunk 0x4733c6ed header, 0xa9496cb5 one '#caf' block; EMPIRICAL on the Idris ATC chair clip).
pub fn parse_caf(data: &[u8], name: &str) -> Result<Clip> {
    let chunks = chunks_of(data)?;
    let (ch, cb) = match (chunks.get(&0x4733_C6EDu32), chunks.get(&0xA949_6CB5u32)) {
        (Some(a), Some(b)) => (*a, *b),
        _ => return Err(Error::msg("no CAF header / data chunk")),
    };
    if data.get(cb..cb + 4) != Some(b"#caf") {
        return Err(Error::msg("no '#caf' block"));
    }
    let n = u16_at(data, cb + 4)? as usize;
    let bones: Vec<u32> = (0..n).map(|k| u32_at(data, cb + 12 + 4 * k)).collect::<Result<_>>()?;
    let frames = u32_at(data, ch + 0x10)?;
    Ok(Clip { seconds: frames as f64 / FPS, name: name.to_lowercase(), frames, bones, src: Some((Arc::new(data.to_vec()), cb)), tracks: OnceLock::new() })
}

/// `.chrparams`: tracks = dba paths, path = base folder, anims = {animation name (lower): file name} in file order.
#[derive(Debug, Clone, Default)]
pub struct ChrParams {
    pub tracks: Vec<String>,
    pub path: String,
    pub anims: Vec<(String, String)>,
}

impl ChrParams {
    pub fn anim(&self, name: &str) -> Option<&str> {
        self.anims.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

pub fn parse_chrparams(data: &[u8]) -> Result<ChrParams> {
    let root = if cryxml::is_cryxmlb(data) { cryxml::parse(data)? } else { plainxml::parse(data)? };
    let mut out = ChrParams::default();
    let mut all = vec![];
    root.iter("Animation", &mut all);
    for a in all {
        let name = a.get("name").unwrap_or("");
        let path = a.get("path").unwrap_or("").replace('\\', "/");
        if name.to_lowercase() == "$tracksdatabase" {
            out.tracks.push(path);
        } else if name == "#filepath" {
            out.path = path;
        } else if !name.is_empty() && !name.starts_with('$') {
            let k = name.to_lowercase();
            match out.anims.iter_mut().find(|(a, _)| *a == k) {
                Some(slot) => slot.1 = path,
                None => out.anims.push((k, path)),
            }
        }
    }
    Ok(out)
}

/// The Clip an animation name of a chrparams resolves to, looked up in the parsed databases.
pub fn clip_for(info: &ChrParams, dbas: &[Arc<Dba>], anim: &str) -> Option<Arc<Clip>> {
    let f = info.anim(&anim.to_lowercase())?;
    if f.is_empty() {
        return None;
    }
    let full = if f.contains('/') { f.to_string() } else { format!("{}/{}", info.path.trim_end_matches('/'), f) }.to_lowercase();
    dbas.iter().find_map(|d| d.get(&full).cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_and_sampling() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        let t = [0.0, 10.0, 20.0];
        let v = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [3.0, 0.0, 0.0]];
        assert_eq!(sample_pos(&t, &v, 5.0)[0], 0.5);
        assert_eq!(sample_pos(&t, &v, 99.0)[0], 3.0);
    }

    #[test]
    fn smallest_three_is_unit_for_zero() {
        let q = smallest_three(0, [10, 10, 10], 32);
        assert!(q.iter().all(|v| v.is_finite()));
    }
}
