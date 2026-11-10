//! Wwise soundbanks (.bnk, bank version 154): which events play which media, and the FNV-1 hash that names them.
//!
//! Chunks: `BKHD` {u32 version, u32 bank id, ...}, `DIDX` {u32 media id, u32 offset, u32 size}*, `DATA` (embedded .wem files, offsets
//! relative to the DATA payload) and `HIRC` {u32 count, then objects {u8 type, u32 size, body}} with body = u32 id + payload.
//! Everything below was checked against 2,060 banks of the live archive (all version 154); EMPIRICAL unless stated:
//!
//! * A bank's id is the FNV-1 (32 bit, offset 0x811c9dc5, prime 0x01000193, on the lower-cased name) of its file name without ".bnk".
//!   Event ids are the same hash of the event name; the names themselves are not stored.
//! * Object types (Wwise SDK): 2 sound, 3 action, 4 event, 5 random/sequence container, 6 switch container, 7 actor-mixer,
//!   9 layer container, 10/11/12/13 music segment/track/switch/random-sequence.
//! * Event (4): u8 action count, u32 action ids.  Action (3): u16 type (0x0403 = play, 0x0103 stop, 0x0203 pause, 0x0303 resume; high
//!   byte per the Wwise SDK's AkActionType, INFERRED), u32 target object id at +6 of the body.
//! * Sound (2): u32 plugin (0x00040001 = Vorbis; 0x65/0x66 generators have no media), u8 stream type at +8 (0 embedded in this bank's
//!   DATA, 1 streamed and 2 prefetched = file Media/<id>.wem), u32 media id at +9; node parameters start at +22.
//! * Node parameters: u8 override-FX, u8 FX count, u8 FX bypass bits, 6 bytes per FX, u8 override-attachment, u32 override bus,
//!   u32 DIRECT PARENT id.  Containers start them at +4.
//!
//! The music objects (types 10..13) and the node base layout: every layout was derived on the 14
//! `MUS_*` banks (EMPIRICAL, names INFERRED from the SDK): a parser is only accepted when it consumes the object to its last byte.
//! Every map keeps its insertion order (`IndexMap`), because the sound index lists media in the order of that traversal.

use std::fmt;

use indexmap::IndexMap;
use serde_json::{json, Map, Value};

use crate::error::{Error, Result};

/// Wwise's id for a name: 32-bit FNV-1 of the lower-cased bytes.
pub fn fnv1(name: &str) -> u32 {
    fnv1_bytes(name.as_bytes())
}

/// FNV-1 of the ASCII-lower-cased bytes (lower-casing the string first is identical for ASCII input, which every candidate is).
pub fn fnv1_bytes(name: &[u8]) -> u32 {
    let mut h: u32 = 0x811C_9DC5;
    for &c in name {
        h = h.wrapping_mul(0x0100_0193) ^ (c.to_ascii_lowercase() as u32);
    }
    h
}

pub const T_SOUND: u8 = 2;
pub const T_ACTION: u8 = 3;
pub const T_EVENT: u8 = 4;
pub const T_RANSEQ: u8 = 5;
pub const T_SWITCH: u8 = 6;
pub const T_MIXER: u8 = 7;
pub const T_LAYER: u8 = 9;
pub const T_MSEGMENT: u8 = 10;
pub const T_MTRACK: u8 = 11;
pub const T_MSWITCH: u8 = 12;
pub const T_MRANSEQ: u8 = 13;

pub const ENTRY_CUE: u32 = 43573010;
pub const EXIT_CUE: u32 = 1539036744;

pub fn action_name(t: u16) -> Option<&'static str> {
    match t {
        0x0403 => Some("play"),
        0x0103 => Some("stop"),
        0x0203 => Some("pause"),
        0x0303 => Some("resume"),
        _ => None,
    }
}

/// An object whose layout this module does not know (or that is truncated).
#[derive(Debug, Clone, PartialEq)]
pub struct BadObject(pub String);

impl fmt::Display for BadObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

type R<T> = std::result::Result<T, BadObject>;

fn bad<T>(s: impl Into<String>) -> R<T> {
    Err(BadObject(s.into()))
}

fn rd<const N: usize>(b: &[u8], o: usize) -> R<[u8; N]> {
    match o.checked_add(N).and_then(|e| b.get(o..e)) {
        Some(s) => Ok(s.try_into().unwrap()),
        None => bad(format!("truncated at {}", o)),
    }
}
/// Reads a multi-field record: the error names the start offset.
fn chk(b: &[u8], o: usize, n: usize) -> R<()> {
    if o.checked_add(n).map_or(true, |e| e > b.len()) {
        return bad(format!("truncated at {}", o));
    }
    Ok(())
}
fn r8(b: &[u8], o: usize) -> R<u8> {
    Ok(rd::<1>(b, o)?[0])
}
fn r16(b: &[u8], o: usize) -> R<u16> {
    Ok(u16::from_le_bytes(rd(b, o)?))
}
fn r32(b: &[u8], o: usize) -> R<u32> {
    Ok(u32::from_le_bytes(rd(b, o)?))
}
fn ri32(b: &[u8], o: usize) -> R<i32> {
    Ok(i32::from_le_bytes(rd(b, o)?))
}
fn rf32(b: &[u8], o: usize) -> R<f32> {
    Ok(f32::from_le_bytes(rd(b, o)?))
}
fn rf64(b: &[u8], o: usize) -> R<f64> {
    Ok(f64::from_le_bytes(rd(b, o)?))
}
fn ri16(b: &[u8], o: usize) -> R<i16> {
    Ok(i16::from_le_bytes(rd(b, o)?))
}

/// Wwise variable-length integer (7 bits per byte, high bit = more).
fn var(b: &[u8], mut o: usize) -> R<(u64, usize)> {
    let mut v = 0u64;
    loop {
        let Some(&c) = b.get(o) else { return bad("truncated var") };
        o += 1;
        v = (v << 7) | (c & 0x7F) as u64;
        if c & 0x80 == 0 {
            return Ok((v, o));
        }
    }
}

/// `b[i]` with an out-of-range index mapped to the node-base error.
fn ix(b: &[u8], i: usize) -> R<u8> {
    b.get(i).copied().map_or_else(|| bad("truncated node base"), Ok)
}

/// Parses a node base (module docs) starting at `o`: (parent id, offset after it).  Errors for positioning / aux flags never seen.
pub fn node_base(b: &[u8], o: usize) -> R<(u32, usize)> {
    let n = ix(b, o + 1)? as usize;
    let mut o = o + 2 + (if n > 0 { 1 + 6 * n + 1 } else { 1 }) + 5;
    let parent = r32(b, o)?;
    o += 5; // parent + bit vector
    o += 1 + 5 * ix(b, o)? as usize; // property bundle
    o += 1 + 9 * ix(b, o)? as usize; // ranged property bundle
    let pos = ix(b, o)?;
    o += 1;
    if pos & 2 != 0 {
        let v = ix(b, o)?;
        if v != 8 {
            return bad(format!("3D positioning bits {:#x}", v));
        }
        o += 1;
    } else if pos != 0 {
        return bad(format!("positioning bits {:#x}", pos));
    }
    let aux = ix(b, o)?;
    o += 1;
    if aux & !0x0C != 0 {
        return bad(format!("aux bits {:#x}", aux));
    }
    if aux & 8 != 0 {
        o += 16;
    }
    o += 6 + 4; // advanced settings + 4 unknown bytes
    let (n, o2) = var(b, o)?;
    o = o2;
    for _ in 0..n {
        // state properties
        o = var(b, o)?.1;
        o += 2;
    }
    let (n, o2) = var(b, o)?;
    o = o2;
    for _ in 0..n {
        // state groups
        o += 5;
        let (k, o2) = var(b, o)?;
        o = o2 + 12 * k as usize;
    }
    let n = r16(b, o)?;
    o += 2;
    for _ in 0..n {
        // RTPCs
        o += 6;
        o = var(b, o)?.1;
        o += 5;
        o += 2 + 12 * r16(b, o)? as usize;
    }
    if o > b.len() {
        return bad("node base runs past the object");
    }
    Ok((parent, o))
}

/// u32 count followed by that many u32 ids: (ids, offset after).
fn ids_at(b: &[u8], o: usize) -> R<(Vec<u32>, usize)> {
    let n = r32(b, o)? as usize;
    if (n as i64) > (b.len() as i64 - o as i64).div_euclid(4) {
        return bad("id list longer than the object");
    }
    let mut v = Vec::with_capacity(n);
    for i in 0..n {
        v.push(r32(b, o + 4 + 4 * i)?);
    }
    Ok((v, o + 4 + 4 * n))
}

pub type Stinger = (u32, u32, u32, u32, i32, u32);

/// Common head of segment / switch / random-sequence containers.
#[derive(Debug, Clone, Default)]
pub struct MusicNode {
    pub parent: u32,
    pub children: Vec<u32>,
    pub tempo: f32,
    pub beats: u8,
    pub beat_unit: u8,
    pub grid_ms: f64,
    pub grid_offset_ms: f64,
    pub stingers: Vec<Stinger>,
    pub approx: bool,
}

fn music_node(b: &[u8], forced: Option<usize>) -> R<(MusicNode, usize)> {
    let (parent, mut o) = match forced {
        None => node_base(b, 5)?,
        Some(f) => (0, f),
    };
    let (kids, o2) = ids_at(b, o)?;
    o = o2;
    chk(b, o, 23)?;
    let (grid, off, tempo, beats, unit) = (rf64(b, o)?, rf64(b, o + 8)?, rf32(b, o + 16)?, r8(b, o + 20)?, r8(b, o + 21)?);
    r8(b, o + 22)?; // flags
    o += 23;
    let ns = r32(b, o)? as usize;
    o += 4;
    let mut stingers = Vec::new();
    for i in 0..ns {
        let q = o + 24 * i;
        chk(b, q, 24)?;
        stingers.push((r32(b, q)?, r32(b, q + 4)?, r32(b, q + 8)?, r32(b, q + 12)?, ri32(b, q + 16)?, r32(b, q + 20)?));
    }
    o += 24 * ns;
    let node = MusicNode {
        parent,
        children: kids,
        tempo,
        beats,
        beat_unit: unit,
        grid_ms: grid,
        grid_offset_ms: off,
        stingers,
        approx: forced.is_some(),
    };
    Ok((node, o))
}

fn expect_end(b: &[u8], o: usize, what: &str) -> R<()> {
    if o != b.len() {
        return bad(format!("{}: {} bytes left", what, b.len() as i64 - o as i64));
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Segment {
    pub node: MusicNode,
    pub duration: f64,
    pub markers: Vec<(u32, f64, String)>,
}

pub fn parse_music_segment(b: &[u8], forced: Option<usize>) -> R<Segment> {
    let (node, mut o) = music_node(b, forced)?;
    chk(b, o, 12)?;
    let (dur, n) = (rf64(b, o)?, r32(b, o + 8)?);
    o += 12;
    let mut markers = Vec::new();
    for _ in 0..n {
        chk(b, o, 12)?;
        let (mid, pos) = (r32(b, o)?, rf64(b, o + 4)?);
        let Some(rel) = b.get(o + 12..).and_then(|s| s.iter().position(|&c| c == 0)) else { return bad("marker name") };
        let end = o + 12 + rel;
        markers.push((mid, pos, b[o + 12..end].iter().map(|&c| c as char).collect()));
        o = end + 1;
    }
    expect_end(b, o, "segment")?;
    Ok(Segment { node, duration: dur, markers })
}

#[derive(Debug, Clone)]
pub struct Clip {
    pub subtrack: u32,
    pub source: u32,
    pub file: u32,
    pub event: u32,
    pub play_at: f64,
    pub begin_trim: f64,
    pub end_trim: f64,
    pub duration: f64,
}

#[derive(Debug, Clone)]
pub struct TrackSwitch {
    pub group: u32,
    pub default: u32,
    pub switches: Vec<u32>,
    pub group_type: u8,
}

#[derive(Debug, Clone)]
pub struct Track {
    pub parent: u32,
    /// (plugin, stream type, media id, file id, size)
    pub sources: Vec<(u32, u8, u32, u32, u32)>,
    pub clips: Vec<Clip>,
    pub subtracks: u32,
    pub ttype: i32,
    pub switch: Option<TrackSwitch>,
    pub look_ahead: i32,
    pub automations: u32,
}

pub fn parse_music_track(b: &[u8]) -> R<Track> {
    let ns = r32(b, 4)? as usize;
    let mut o = 8;
    if ns > b.len() / 18 {
        return bad("source count");
    }
    let mut sources = Vec::with_capacity(ns);
    for i in 0..ns {
        let q = o + 18 * i;
        chk(b, q, 17)?;
        sources.push((r32(b, q)?, r8(b, q + 4)?, r32(b, q + 5)?, r32(b, q + 9)?, r32(b, q + 13)?));
    }
    o += 18 * ns + 1;
    let npl = r32(b, o)? as usize;
    o += 4;
    if npl > b.len() / 48 {
        return bad("clip count");
    }
    let mut clips = Vec::with_capacity(npl);
    for i in 0..npl {
        let q = o + 48 * i;
        chk(b, q, 48)?;
        clips.push(Clip {
            subtrack: r32(b, q)?,
            source: r32(b, q + 4)?,
            file: r32(b, q + 8)?,
            event: r32(b, q + 12)?,
            play_at: rf64(b, q + 16)?,
            begin_trim: rf64(b, q + 24)?,
            end_trim: rf64(b, q + 32)?,
            duration: rf64(b, q + 40)?,
        });
    }
    o += 48 * npl;
    let mut subtracks = 0;
    if npl > 0 {
        subtracks = r32(b, o)?;
        o += 4;
    }
    let na = r32(b, o)?;
    o += 4;
    for _ in 0..na {
        // clip automations: not exposed
        let k = r32(b, o + 8)? as usize;
        o += 12 + 12 * k;
    }
    let (parent, o2) = node_base(b, o)?;
    o = o2;
    let ttype = b.get(o).map_or(-1, |&v| v as i32);
    o += 1;
    let mut sw = None;
    if ttype == 3 {
        chk(b, o, 13)?;
        let (group, default, n) = (r32(b, o + 1)?, r32(b, o + 5)?, r32(b, o + 9)? as usize);
        o += 13;
        if (n as i64) > (b.len() as i64 - o as i64).div_euclid(4) {
            return bad("switch list");
        }
        let mut switches = Vec::with_capacity(n);
        for i in 0..n {
            switches.push(r32(b, o + 4 * i)?);
        }
        sw = Some(TrackSwitch { group, default, switches, group_type: b[o - 13] });
        o += 4 * n + 32;
    }
    let look = ri32(b, o)?;
    o += 4;
    expect_end(b, o, "track")?;
    Ok(Track { parent, sources, clips, subtracks, ttype, switch: sw, look_ahead: look, automations: na })
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub sources: Vec<u32>,
    pub destinations: Vec<u32>,
    pub src_rule: (i32, u32, i32, u32, u32, u8),
    pub dst_rule: (i32, u32, i32, u32, u32, u16, u16, u8, u8),
    pub transition_segment: u32,
}

fn trans_rules(b: &[u8], mut o: usize) -> R<(Vec<Rule>, usize)> {
    let n = r32(b, o)?;
    o += 4;
    let mut rules = Vec::new();
    for _ in 0..n {
        let (src, o2) = ids_at(b, o)?;
        let (dst, o3) = ids_at(b, o2)?;
        o = o3;
        chk(b, o, 21)?;
        chk(b, o + 21, 26)?;
        let sr = (ri32(b, o)?, r32(b, o + 4)?, ri32(b, o + 8)?, r32(b, o + 12)?, r32(b, o + 16)?, r8(b, o + 20)?);
        let q = o + 21;
        let dr = (ri32(b, q)?, r32(b, q + 4)?, ri32(b, q + 8)?, r32(b, q + 12)?, r32(b, q + 16)?, r16(b, q + 20)?, r16(b, q + 22)?, r8(b, q + 24)?, r8(b, q + 25)?);
        o += 47;
        let flag = r8(b, o)?;
        o += 1;
        let mut seg = 0;
        if flag != 0 {
            seg = r32(b, o)?;
            o += 30;
        }
        rules.push(Rule { sources: src, destinations: dst, src_rule: sr, dst_rule: dr, transition_segment: seg });
    }
    Ok((rules, o))
}

#[derive(Debug, Clone)]
pub struct Tree {
    pub key: u32,
    /// leaves only (audio node id)
    pub target: Option<u32>,
    pub children: Vec<Tree>,
    pub weight: u16,
    pub probability: u16,
}

#[derive(Debug, Clone)]
pub struct MSwitch {
    pub node: MusicNode,
    pub rules: Vec<Rule>,
    pub continue_playback: u8,
    pub groups: Vec<(u32, u8)>,
    pub mode: u8,
    pub tree: Option<Tree>,
}

pub fn parse_music_switch(b: &[u8], forced: Option<usize>) -> R<MSwitch> {
    let (node, o) = music_node(b, forced)?;
    let (rules, mut o) = trans_rules(b, o)?;
    let cont = r8(b, o)?;
    let depth = r32(b, o + 1)? as usize;
    o += 5;
    if depth > 64 {
        return bad("tree depth");
    }
    chk(b, o, 4 * depth)?;
    let mut gids = Vec::with_capacity(depth);
    for i in 0..depth {
        gids.push(r32(b, o + 4 * i)?);
    }
    o += 4 * depth;
    let gtypes: Vec<u8> = b.get(o..).map_or(&[][..], |s| &s[..depth.min(s.len())]).to_vec();
    o += depth;
    chk(b, o, 5)?;
    let (size, mode) = (r32(b, o)? as usize, r8(b, o + 4)?);
    o += 5;
    if size % 12 != 0 || o + size != b.len() {
        return bad("decision tree size");
    }
    fn read(b: &[u8], base: usize, size: usize, depth: usize, i: usize, level: usize, seen: &mut usize) -> R<Tree> {
        if i >= size / 12 || *seen > size / 12 {
            return bad("decision tree index");
        }
        *seen += 1;
        let q = base + 12 * i;
        chk(b, q, 12)?;
        let (key, a, c, w, p) = (r32(b, q)?, r16(b, q + 4)? as usize, r16(b, q + 6)? as usize, r16(b, q + 8)?, r16(b, q + 10)?);
        if level == depth {
            return Ok(Tree { key, target: Some(r32(b, q + 4)?), children: vec![], weight: w, probability: p });
        }
        let mut kids = Vec::with_capacity(c);
        for j in 0..c {
            kids.push(read(b, base, size, depth, a + j, level + 1, seen)?);
        }
        Ok(Tree { key, target: None, children: kids, weight: w, probability: p })
    }
    let mut seen = 0;
    let tree = if size > 0 { Some(read(b, o, size, depth, 0, 0, &mut seen)?) } else { None };
    if size > 0 && seen != size / 12 {
        return bad(format!("decision tree has {} nodes, {} reachable", size / 12, seen));
    }
    let groups = gids.into_iter().zip(gtypes).collect();
    Ok(MSwitch { node, rules, continue_playback: cont, groups, mode, tree })
}

#[derive(Debug, Clone)]
pub struct RsItem {
    pub segment: u32,
    pub id: u32,
    pub children: u32,
    pub ty: u32,
    pub loop_: i16,
    pub loop_min: i16,
    pub loop_max: i16,
    pub weight: u32,
    pub avoid_repeat: u16,
    pub using_weight: u8,
    pub shuffle: u8,
    pub depth: usize,
}

#[derive(Debug, Clone)]
pub struct MRanSeq {
    pub node: MusicNode,
    pub rules: Vec<Rule>,
    pub items: Vec<RsItem>,
}

pub fn parse_music_ranseq(b: &[u8], forced: Option<usize>) -> R<MRanSeq> {
    let (node, o) = music_node(b, forced)?;
    let (rules, mut o) = trans_rules(b, o)?;
    let n = r32(b, o)? as usize;
    o += 4;
    if (n as i64) > (b.len() as i64 - o as i64).div_euclid(30) {
        return bad("playlist size");
    }
    let mut items = Vec::with_capacity(n);
    let mut stack: Vec<u32> = Vec::new();
    for _ in 0..n {
        chk(b, o, 30)?;
        let it = (
            r32(b, o)?,
            r32(b, o + 4)?,
            r32(b, o + 8)?,
            r32(b, o + 12)?,
            ri16(b, o + 16)?,
            ri16(b, o + 18)?,
            ri16(b, o + 20)?,
            r32(b, o + 22)?,
            r16(b, o + 26)?,
            r8(b, o + 28)?,
            r8(b, o + 29)?,
        );
        o += 30;
        while stack.last() == Some(&0) {
            stack.pop();
        }
        items.push(RsItem {
            segment: it.0,
            id: it.1,
            children: it.2,
            ty: it.3,
            loop_: it.4,
            loop_min: it.5,
            loop_max: it.6,
            weight: it.7,
            avoid_repeat: it.8,
            using_weight: it.9,
            shuffle: it.10,
            depth: stack.len(),
        });
        if let Some(l) = stack.last_mut() {
            *l -= 1;
        }
        if it.2 != 0 {
            stack.push(it.2);
        }
    }
    expect_end(b, o, "random/sequence container")?;
    Ok(MRanSeq { node, rules, items })
}

/// `parse(b)`; when the node base is not understood the child list is searched instead: the first offset holding a count and that many
/// ids of this bank for which the rest of the object parses to its exact end.  The result carries `approx` and no parent.
fn scan_music<T>(parse: &dyn Fn(&[u8], Option<usize>) -> R<T>, b: &[u8], ids: &std::collections::HashSet<u32>) -> R<T> {
    let err = match parse(b, None) {
        Ok(v) => return Ok(v),
        Err(e) => e,
    };
    if b.len() > 13 {
        for o in 5..b.len() - 8 {
            let n = u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as usize;
            if n >= 1 && (n as i64) <= (b.len() as i64 - o as i64).div_euclid(4) - 1 && (0..n).all(|i| ids.contains(&u32::from_le_bytes(b[o + 4 + 4 * i..o + 8 + 4 * i].try_into().unwrap()))) {
                if let Ok(v) = parse(b, Some(o)) {
                    return Ok(v);
                }
            }
        }
    }
    Err(err)
}

/// Action detail: the play action's bank id, or the group / value of a set-state / set-switch action.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ActionInfo {
    pub bank: Option<u32>,
    pub group: Option<u32>,
    pub value: Option<u32>,
}

impl ActionInfo {
    pub fn is_empty(&self) -> bool {
        self.bank.is_none() && self.group.is_none() && self.value.is_none()
    }
}

pub fn parse_action(b: &[u8]) -> ActionInfo {
    let mut out = ActionInfo::default();
    let go = || -> R<ActionInfo> {
        let mut out = ActionInfo::default();
        let t = r16(b, 4)?;
        let mut o = 10 + 1; // is-bus byte
        o += 1 + 5 * ix(b, o)? as usize;
        o += 1 + 9 * ix(b, o)? as usize;
        let high = t >> 8;
        let left = b.len() as i64 - o as i64;
        if high == 4 && left >= 9 {
            out.bank = Some(r32(b, o + 1)?);
        } else if (high == 0x12 || high == 0x19) && left == 8 {
            out.group = Some(r32(b, o)?);
            out.value = Some(r32(b, o + 4)?);
        }
        Ok(out)
    };
    if let Ok(v) = go() {
        out = v;
    }
    out
}

/// DIRECT PARENT id of a node whose parameters start at `start`; the computed offset is checked against the bank's ids.
fn parent_of(body: &[u8], start: usize, ids: &std::collections::HashSet<u32>) -> u32 {
    let u32_opt = |o: usize| body.get(o..o + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap()));
    let Some(&nfx) = body.get(start + 1) else { return 0 };
    let nfx = nfx as usize;
    let off = start + 2 + (if nfx > 0 { 1 + 6 * nfx + 1 } else { 1 }) + 5;
    let Some(v) = u32_opt(off) else { return 0 };
    if ids.contains(&v) || v == 0 {
        return v;
    }
    // an FX layout we do not know: the first id that fits
    let hi = body.len().saturating_sub(3).min(start + 12 + 6 * nfx);
    for o in start + 4..hi {
        match u32_opt(o) {
            Some(v) if ids.contains(&v) => return v,
            Some(_) => {}
            None => return 0,
        }
    }
    0
}

/// Music objects of one bank, by kind.
#[derive(Debug, Clone, Default)]
pub struct Music {
    pub segments: IndexMap<u32, Segment>,
    pub tracks: IndexMap<u32, Track>,
    pub switches: IndexMap<u32, MSwitch>,
    pub ranseqs: IndexMap<u32, MRanSeq>,
    pub undecoded: IndexMap<u32, (u8, String)>,
}

#[derive(Debug, Clone, Default)]
pub struct Bank {
    pub version: u32,
    pub id: u32,
    /// embedded media: id -> (offset into the bank data, size)
    pub media: IndexMap<u32, (usize, usize)>,
    pub events: IndexMap<u32, Vec<u32>>,
    pub actions: IndexMap<u32, (u16, u32)>,
    pub action_info: IndexMap<u32, ActionInfo>,
    pub music: Music,
    /// sound id -> (media id or None, stream type, parent)
    pub sounds: IndexMap<u32, (Option<u32>, u8, u32)>,
    pub parents: IndexMap<u32, u32>,
    pub types: IndexMap<u32, u8>,
    pub music_media: IndexMap<u32, Vec<u32>>,
}

/// `{tag: (offset, size)}` of the top-level chunks (a repeated tag keeps the last).
pub fn chunks(data: &[u8]) -> IndexMap<String, (usize, usize)> {
    let mut out = IndexMap::new();
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let tag: String = data[pos..pos + 4].iter().map(|&c| c as char).collect();
        let size = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap()) as usize;
        out.insert(tag, (pos + 8, size));
        pos += 8 + size;
    }
    out
}

fn u32_at_e(b: &[u8], o: usize) -> Result<u32> {
    crate::error::u32_at(b, o)
}

/// Decodes one music object into `out.music`; one that cannot be decoded is recorded in `undecoded` with its error and keeps the
/// heuristic parent (and, for a track, the media of its sources) so that event -> media lookups still work.
fn music_object(out: &mut Bank, ty: u8, oid: u32, b: &[u8], ids: &std::collections::HashSet<u32>) -> std::result::Result<(), ()> {
    macro_rules! scan {
        ($f:expr, $field:ident) => {{
            match scan_music(&$f, b, ids) {
                Ok(mut node) => {
                    let parent = if node.node.approx { parent_of(b, 5, ids) } else { node.node.parent };
                    node.node.parent = parent;
                    out.parents.insert(oid, parent);
                    out.music.$field.insert(oid, node);
                    return Ok(());
                }
                Err(e) => {
                    out.music.undecoded.insert(oid, (ty, e.0));
                }
            }
        }};
    }
    match ty {
        T_MSEGMENT => scan!(parse_music_segment, segments),
        T_MSWITCH => scan!(parse_music_switch, switches),
        T_MRANSEQ => scan!(parse_music_ranseq, ranseqs),
        _ => match parse_music_track(b) {
            Ok(t) => {
                out.parents.insert(oid, t.parent);
                out.music_media.insert(oid, t.sources.iter().filter(|s| s.0 >> 16 == 4).map(|s| s.2).collect());
                out.music.tracks.insert(oid, t);
                return Ok(());
            }
            Err(e) => {
                out.music.undecoded.insert(oid, (ty, e.0));
            }
        },
    }
    if ty != T_MTRACK {
        out.parents.insert(oid, parent_of(b, 5, ids));
        return Ok(());
    }
    // a track that could not be decoded: the media of its sources; a truncated source table skips the object
    let n = u32_at_e(b, 4).map_err(|_| ())? as usize;
    let mut media = Vec::new();
    for i in 0..n {
        let q = 8 + 18 * i;
        let (Ok(plugin), Ok(m)) = (u32_at_e(b, q), u32_at_e(b, q + 5)) else { return Err(()) };
        if b.get(q + 4).is_none() {
            return Err(());
        }
        if plugin >> 16 == 4 {
            media.push(m);
        }
    }
    out.music_media.insert(oid, media);
    let start = 8 + 18 * n;
    let mut par = 0;
    for o in start..b.len().saturating_sub(3) {
        let v = u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        if ids.contains(&v) && v != oid {
            par = v;
            break;
        }
    }
    out.parents.insert(oid, par);
    Ok(())
}

/// Parses a whole bank.  An error (truncated chunk table, HIRC object header past the end) means the bank is unusable
/// where the exception propagates; a single odd object is skipped.
pub fn parse_bank(data: &[u8]) -> Result<Bank> {
    let ch = chunks(data);
    let mut out = Bank::default();
    if let Some(&(o, _)) = ch.get("BKHD") {
        out.version = u32_at_e(data, o)?;
        out.id = u32_at_e(data, o + 4)?;
    }
    if let (Some(&(a, n)), Some(&(base, _))) = (ch.get("DIDX"), ch.get("DATA")) {
        for i in 0..n / 12 {
            let q = a + 12 * i;
            let (mid, off, size) = (u32_at_e(data, q)?, u32_at_e(data, q + 4)?, u32_at_e(data, q + 8)?);
            out.media.insert(mid, (base + off as usize, size as usize));
        }
    }
    let Some(&(a, _)) = ch.get("HIRC") else { return Ok(out) };
    let count = u32_at_e(data, a)?;
    let mut q = a + 4;
    let mut objs: Vec<(u8, &[u8])> = Vec::new();
    for _ in 0..count {
        let ty = *data.get(q).ok_or_else(|| Error::msg("index out of range"))?;
        let size = u32_at_e(data, q + 1)? as usize;
        let lo = (q + 5).min(data.len());
        let hi = (q + 5).saturating_add(size).min(data.len());
        objs.push((ty, &data[lo..hi]));
        q = q.saturating_add(5).saturating_add(size);
    }
    let ids: std::collections::HashSet<u32> = objs.iter().filter(|(_, b)| b.len() >= 4).map(|(_, b)| u32::from_le_bytes(b[..4].try_into().unwrap())).collect();
    for (ty, b) in objs {
        let oid = u32_at_e(b, 0)?;
        out.types.insert(oid, ty);
        let _ = (|| -> std::result::Result<(), ()> {
            match ty {
                T_EVENT => {
                    let n = *b.get(4).ok_or(())? as usize;
                    let mut v = Vec::with_capacity(n);
                    for i in 0..n {
                        v.push(u32_at_e(b, 5 + 4 * i).map_err(|_| ())?);
                    }
                    out.events.insert(oid, v);
                }
                T_ACTION => {
                    let t = crate::error::u16_at(b, 4).map_err(|_| ())?;
                    let target = u32_at_e(b, 6).map_err(|_| ())?;
                    out.actions.insert(oid, (t, target));
                    let extra = parse_action(b);
                    if !extra.is_empty() {
                        out.action_info.insert(oid, extra);
                    }
                }
                T_SOUND => {
                    let plugin = u32_at_e(b, 4).map_err(|_| ())?;
                    let stream = *b.get(8).ok_or(())?;
                    let mid = u32_at_e(b, 9).map_err(|_| ())?;
                    let mid = if plugin & 0xFFFF == 1 && plugin >> 16 == 4 { Some(mid) } else { None };
                    let par = parent_of(b, 22, &ids);
                    out.sounds.insert(oid, (mid, stream, par));
                    out.parents.insert(oid, par);
                }
                T_RANSEQ | T_SWITCH | T_MIXER | T_LAYER => {
                    out.parents.insert(oid, parent_of(b, 4, &ids));
                }
                T_MSEGMENT | T_MSWITCH | T_MRANSEQ | T_MTRACK => {
                    music_object(&mut out, ty, oid, b, &ids)?;
                }
                _ => {}
            }
            Ok(())
        })();
    }
    Ok(out)
}

/// Ids of a parsed bank that name something outside the hierarchy (state / switch groups and values, segment markers) and so can be
/// recovered by hashing candidate strings.
pub fn name_ids(bank: &Bank) -> std::collections::HashSet<u32> {
    let mut ids = std::collections::HashSet::new();
    for sw in bank.music.switches.values() {
        for (g, _t) in &sw.groups {
            ids.insert(*g);
        }
        let mut stack: Vec<&Tree> = sw.tree.iter().collect();
        while let Some(t) = stack.pop() {
            ids.insert(t.key);
            stack.extend(t.children.iter());
        }
    }
    for t in bank.music.tracks.values() {
        if let Some(s) = &t.switch {
            ids.insert(s.group);
            ids.extend(s.switches.iter().copied());
        }
    }
    for sg in bank.music.segments.values() {
        ids.extend(sg.markers.iter().map(|m| m.0));
    }
    for a in bank.action_info.values() {
        ids.extend(a.group);
        ids.extend(a.value);
    }
    ids.remove(&0);
    ids
}

/// Bytes of an embedded .wem (DIDX/DATA) of a bank, or None.
pub fn embedded_media(data: &[u8], media_id: u32) -> Result<Option<Vec<u8>>> {
    let b = parse_bank(data)?;
    Ok(b.media.get(&media_id).map(|&(o, s)| data[o.min(data.len())..(o + s).min(data.len())].to_vec()))
}

// ---- JSON forms ----------------------------------------------------------

fn key<K: ToString>(m: &IndexMap<K, Value>) -> Map<String, Value> {
    m.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
}

fn node_json(n: &MusicNode) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("parent".into(), json!(n.parent));
    m.insert("children".into(), json!(n.children));
    m.insert("tempo".into(), json!(n.tempo as f64));
    m.insert("beats".into(), json!(n.beats));
    m.insert("beat_unit".into(), json!(n.beat_unit));
    m.insert("grid_ms".into(), json!(n.grid_ms));
    m.insert("grid_offset_ms".into(), json!(n.grid_offset_ms));
    m.insert("stingers".into(), Value::Array(n.stingers.iter().map(|s| json!([s.0, s.1, s.2, s.3, s.4, s.5])).collect()));
    if n.approx {
        m.insert("approx".into(), json!(true));
    }
    m
}

fn rules_json(r: &[Rule]) -> Value {
    Value::Array(
        r.iter()
            .map(|r| {
                json!({"sources": r.sources, "destinations": r.destinations,
                       "src_rule": [r.src_rule.0, r.src_rule.1, r.src_rule.2, r.src_rule.3, r.src_rule.4, r.src_rule.5],
                       "dst_rule": [r.dst_rule.0, r.dst_rule.1, r.dst_rule.2, r.dst_rule.3, r.dst_rule.4, r.dst_rule.5, r.dst_rule.6, r.dst_rule.7, r.dst_rule.8],
                       "transition_segment": r.transition_segment})
            })
            .collect(),
    )
}

fn tree_json(t: &Tree) -> Value {
    match t.target {
        Some(tg) => json!({"key": t.key, "target": tg, "weight": t.weight, "probability": t.probability}),
        None => json!({"key": t.key, "children": t.children.iter().map(tree_json).collect::<Vec<_>>(), "weight": t.weight, "probability": t.probability}),
    }
}

impl Bank {
    /// The parse as a JSON value (int keys as decimal strings, tuples as arrays).
    pub fn to_json(&self) -> Value {
        let mut segs = IndexMap::new();
        for (k, s) in &self.music.segments {
            let mut m = node_json(&s.node);
            m.insert("duration".into(), json!(s.duration));
            m.insert("markers".into(), Value::Array(s.markers.iter().map(|x| json!([x.0, x.1, x.2])).collect()));
            segs.insert(*k, Value::Object(m));
        }
        let mut tracks = IndexMap::new();
        for (k, t) in &self.music.tracks {
            let sw = t.switch.as_ref().map_or(Value::Null, |s| json!({"group": s.group, "default": s.default, "switches": s.switches, "group_type": s.group_type}));
            let clips: Vec<Value> = t
                .clips
                .iter()
                .map(|c| json!({"subtrack": c.subtrack, "source": c.source, "file": c.file, "event": c.event, "play_at": c.play_at, "begin_trim": c.begin_trim, "end_trim": c.end_trim, "duration": c.duration}))
                .collect();
            tracks.insert(
                *k,
                json!({"parent": t.parent, "sources": t.sources.iter().map(|s| json!([s.0, s.1, s.2, s.3, s.4])).collect::<Vec<_>>(), "clips": clips,
                       "subtracks": t.subtracks, "type": t.ttype, "switch": sw, "look_ahead": t.look_ahead, "automations": t.automations}),
            );
        }
        let mut switches = IndexMap::new();
        for (k, s) in &self.music.switches {
            let mut m = node_json(&s.node);
            m.insert("rules".into(), rules_json(&s.rules));
            m.insert("continue_playback".into(), json!(s.continue_playback));
            m.insert("groups".into(), Value::Array(s.groups.iter().map(|g| json!([g.0, g.1])).collect()));
            m.insert("mode".into(), json!(s.mode));
            m.insert("tree".into(), s.tree.as_ref().map_or(Value::Null, tree_json));
            switches.insert(*k, Value::Object(m));
        }
        let mut ranseqs = IndexMap::new();
        for (k, s) in &self.music.ranseqs {
            let mut m = node_json(&s.node);
            m.insert("rules".into(), rules_json(&s.rules));
            let items: Vec<Value> = s
                .items
                .iter()
                .map(|i| {
                    json!({"segment": i.segment, "id": i.id, "children": i.children, "type": i.ty, "loop": i.loop_, "loop_min": i.loop_min, "loop_max": i.loop_max,
                           "weight": i.weight, "avoid_repeat": i.avoid_repeat, "using_weight": i.using_weight, "shuffle": i.shuffle, "depth": i.depth})
                })
                .collect();
            m.insert("items".into(), Value::Array(items));
            ranseqs.insert(*k, Value::Object(m));
        }
        let mut undec = IndexMap::new();
        for (k, (t, e)) in &self.music.undecoded {
            undec.insert(*k, json!([t, e]));
        }
        let media: IndexMap<u32, Value> = self.media.iter().map(|(k, v)| (*k, json!([v.0, v.1]))).collect();
        let events: IndexMap<u32, Value> = self.events.iter().map(|(k, v)| (*k, json!(v))).collect();
        let actions: IndexMap<u32, Value> = self.actions.iter().map(|(k, v)| (*k, json!([v.0, v.1]))).collect();
        let ainfo: IndexMap<u32, Value> = self
            .action_info
            .iter()
            .map(|(k, a)| {
                let mut m = Map::new();
                if let Some(b) = a.bank {
                    m.insert("bank".into(), json!(b));
                }
                if let Some(g) = a.group {
                    m.insert("group".into(), json!(g));
                }
                if let Some(v) = a.value {
                    m.insert("value".into(), json!(v));
                }
                (*k, Value::Object(m))
            })
            .collect();
        let sounds: IndexMap<u32, Value> = self.sounds.iter().map(|(k, v)| (*k, json!([v.0, v.1, v.2]))).collect();
        let parents: IndexMap<u32, Value> = self.parents.iter().map(|(k, v)| (*k, json!(v))).collect();
        let types: IndexMap<u32, Value> = self.types.iter().map(|(k, v)| (*k, json!(v))).collect();
        let mm: IndexMap<u32, Value> = self.music_media.iter().map(|(k, v)| (*k, json!(v))).collect();
        json!({"version": self.version, "id": self.id, "media": key(&media), "events": key(&events), "actions": key(&actions), "action_info": key(&ainfo),
               "sounds": key(&sounds), "parents": key(&parents), "types": key(&types), "music_media": key(&mm),
               "music": {"segments": key(&segs), "tracks": key(&tracks), "switches": key(&switches), "ranseqs": key(&ranseqs), "undecoded": key(&undec)}})
    }
}
