//! Index of the Wwise soundbanks: which events play which media, and (where it can be recovered) the event names.
//!
//! Banks store only 32-bit FNV-1 ids of event names (formats/bnk.rs).  Names are recovered by hashing candidate strings and keeping the
//! ones that equal an event id of some bank: every string of Game2.dcb, every path segment of the archive, the strings of the game
//! executable when it is next to the archive, "Play_/Stop_/Pause_/Resume_" prefixed variants of those, and every
//! "<prefix>_<bank name>_<suffix>" built from the suffixes of the names found so far.  EMPIRICAL on the live archive: about a third of
//! the 30,816 events get a name that way; the rest are shown by id and bank.  A hash match of a 32-bit id is strong evidence for names
//! of this length, but it is not proof.
//!
//! Music banks (MUS_*) add the Wwise music hierarchy: per music object its type, bank and the media below it (`music_nodes`,
//! `node_media`), and the state / switch groups, their values and the segment markers named by the same hashing (`names2`).
//!
//! The banks are parsed in parallel and merged in archive order; when two candidate strings hash to the same id (they differ only in
//! case, FNV-1 lower-cases) the lexicographically smallest is kept, so the result is deterministic; there is no disk cache here.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use p4k_formats::bnk::{self, Bank};
use p4k::P4k;
use indexmap::IndexMap;
use rayon::prelude::*;
use serde_json::{json, Value};

pub const PREFIXES: [&str; 4] = ["Play", "Stop", "Pause", "Resume"];

/// Pass-through hasher for ids that are already well mixed (FNV outputs).
#[derive(Default, Clone, Copy)]
pub struct IdHasher(u64);

impl Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = self.0.rotate_left(8) ^ b as u64;
        }
    }
    fn write_u32(&mut self, i: u32) {
        self.0 = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

pub type IdSet = HashSet<u32, BuildHasherDefault<IdHasher>>;

#[derive(Debug, Clone, PartialEq)]
pub struct BankInfo {
    pub id: u32,
    pub events: usize,
    pub embedded: usize,
}

#[derive(Debug, Clone, Default)]
pub struct EventInfo {
    /// bank file names that define the event
    pub banks: Vec<String>,
    /// (action type, target object id) of the (last) bank's actions
    pub acts: Vec<(u16, u32)>,
}

#[derive(Debug, Default)]
pub struct SoundData {
    pub banks: IndexMap<String, BankInfo>,
    pub events: IndexMap<u32, EventInfo>,
    pub event_media: HashMap<u32, Vec<u32>>,
    pub media_events: HashMap<u32, Vec<u32>>,
    /// media id -> bank file name that embeds it
    pub embedded: HashMap<u32, String>,
    pub names: HashMap<u32, String>,
    pub names2: HashMap<u32, String>,
    /// music object id -> (object type, bank file name)
    pub music_nodes: HashMap<u32, (u8, String)>,
    pub node_media: HashMap<u32, Vec<u32>>,
    /// stinger trigger id -> [(host object id, segment id)]
    pub stingers: HashMap<u32, Vec<(u32, u32)>>,
    /// the played-by map, built on first use
    played: OnceLock<HashMap<u32, Vec<u32>>>,
}

impl SoundData {
    pub fn event_name(&self, eid: u32) -> Option<&str> {
        self.names.get(&eid).map(String::as_str)
    }

    /// `{target object id: [event ids]}` of the play actions of every event (built on first use).
    pub fn played_by(&self) -> &HashMap<u32, Vec<u32>> {
        self.played.get_or_init(|| {
            let mut out: HashMap<u32, Vec<u32>> = HashMap::new();
            for (&eid, ev) in &self.events {
                for &(t, tg) in &ev.acts {
                    if t >> 8 == 4 {
                        let v = out.entry(tg).or_default();
                        if !v.contains(&eid) {
                            v.push(eid);
                        }
                    }
                }
            }
            out
        })
    }

    /// Name of a state / switch group, a state / switch value or a marker, or None (events: `event_name`).
    pub fn name(&self, oid: u32) -> Option<&str> {
        self.names2.get(&oid).or_else(|| self.names.get(&oid)).map(String::as_str)
    }

    /// Short description of a media file: the names of the events that play it (or their bank), or None.
    pub fn label(&self, media_id: u32) -> Option<String> {
        let evs = self.media_events.get(&media_id).filter(|v| !v.is_empty())?;
        let more = if evs.len() > 1 { format!(" (+{} more)", evs.len() - 1) } else { String::new() };
        let mut named: Vec<&String> = evs.iter().filter_map(|e| self.names.get(e)).collect();
        named.sort();
        if let Some(first) = named.first() {
            return Some(format!("{}{}", first, more));
        }
        let bank = self.events[&evs[0]].banks[0].rsplit('/').next().unwrap_or("");
        Some(format!("{}: event {}{}", &bank[..bank.len().saturating_sub(4)], evs[0], more))
    }

    /// The index in the disk cache's binary form (`codec`); [`SoundData::from_bytes`] is the inverse.  Order-preserving for the two
    /// `IndexMap`s, the `HashMap`s are written in iteration order (their order is unspecified anyway).
    pub fn to_bytes(&self) -> Vec<u8> {
        use crate::codec::Writer;
        fn ids(w: &mut Writer, m: &HashMap<u32, Vec<u32>>) {
            w.uv(m.len() as u64);
            for (k, v) in m {
                w.uv(*k as u64);
                w.u32s(v);
            }
        }
        fn strs(w: &mut Writer, m: &HashMap<u32, String>) {
            w.uv(m.len() as u64);
            for (k, v) in m {
                w.uv(*k as u64);
                w.str(v);
            }
        }
        let mut w = Writer::new();
        w.uv(self.banks.len() as u64);
        for (k, b) in &self.banks {
            w.str(k);
            w.uv(b.id as u64);
            w.uv(b.events as u64);
            w.uv(b.embedded as u64);
        }
        w.uv(self.events.len() as u64);
        for (k, e) in &self.events {
            w.uv(*k as u64);
            w.strs(&e.banks);
            w.uv(e.acts.len() as u64);
            for &(t, tg) in &e.acts {
                w.uv(t as u64);
                w.uv(tg as u64);
            }
        }
        ids(&mut w, &self.event_media);
        ids(&mut w, &self.media_events);
        strs(&mut w, &self.embedded);
        strs(&mut w, &self.names);
        strs(&mut w, &self.names2);
        w.uv(self.music_nodes.len() as u64);
        for (k, (t, b)) in &self.music_nodes {
            w.uv(*k as u64);
            w.u8(*t);
            w.str(b);
        }
        ids(&mut w, &self.node_media);
        w.uv(self.stingers.len() as u64);
        for (k, v) in &self.stingers {
            w.uv(*k as u64);
            w.uv(v.len() as u64);
            for &(a, b) in v {
                w.uv(a as u64);
                w.uv(b as u64);
            }
        }
        w.buf
    }

    /// Inverse of [`SoundData::to_bytes`]; None for any damaged input.
    pub fn from_bytes(b: &[u8]) -> Option<SoundData> {
        use crate::codec::Reader;
        fn ids(r: &mut Reader) -> Option<HashMap<u32, Vec<u32>>> {
            let n = r.count()?;
            let mut m = HashMap::with_capacity(n);
            for _ in 0..n {
                let k = r.uv()? as u32;
                m.insert(k, r.u32s()?);
            }
            Some(m)
        }
        fn strs(r: &mut Reader) -> Option<HashMap<u32, String>> {
            let n = r.count()?;
            let mut m = HashMap::with_capacity(n);
            for _ in 0..n {
                let k = r.uv()? as u32;
                m.insert(k, r.string()?);
            }
            Some(m)
        }
        let mut r = Reader::new(b);
        let mut d = SoundData::default();
        for _ in 0..r.count()? {
            let k = r.string()?;
            d.banks.insert(k, BankInfo { id: r.uv()? as u32, events: r.uv()? as usize, embedded: r.uv()? as usize });
        }
        for _ in 0..r.count()? {
            let k = r.uv()? as u32;
            let banks = r.strs()?;
            let n = r.count()?;
            let mut acts = Vec::with_capacity(n);
            for _ in 0..n {
                acts.push((r.uv()? as u16, r.uv()? as u32));
            }
            d.events.insert(k, EventInfo { banks, acts });
        }
        d.event_media = ids(&mut r)?;
        d.media_events = ids(&mut r)?;
        d.embedded = strs(&mut r)?;
        d.names = strs(&mut r)?;
        d.names2 = strs(&mut r)?;
        for _ in 0..r.count()? {
            let k = r.uv()? as u32;
            let t = r.u8()?;
            d.music_nodes.insert(k, (t, r.string()?));
        }
        d.node_media = ids(&mut r)?;
        for _ in 0..r.count()? {
            let k = r.uv()? as u32;
            let n = r.count()?;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                v.push((r.uv()? as u32, r.uv()? as u32));
            }
            d.stingers.insert(k, v);
        }
        r.done().then_some(d)
    }

    /// The index, JSON-shaped (int keys as decimal strings, tuples as arrays) for the parity tests.
    pub fn to_json(&self) -> Value {
        fn m(it: impl Iterator<Item = (String, Value)>) -> Value {
            Value::Object(it.collect())
        }
        let banks = m(self.banks.iter().map(|(k, b)| (k.clone(), json!({"id": b.id, "events": b.events, "embedded": b.embedded}))));
        let events = m(self.events.iter().map(|(k, e)| (k.to_string(), json!({"banks": e.banks, "acts": e.acts.iter().map(|a| json!([a.0, a.1])).collect::<Vec<_>>()}))));
        json!({"banks": banks, "events": events,
               "event_media": m(self.event_media.iter().map(|(k, v)| (k.to_string(), json!(v)))),
               "media_events": m(self.media_events.iter().map(|(k, v)| (k.to_string(), json!(v)))),
               "embedded": m(self.embedded.iter().map(|(k, v)| (k.to_string(), json!(v)))),
               "names": m(self.names.iter().map(|(k, v)| (k.to_string(), json!(v)))),
               "names2": m(self.names2.iter().map(|(k, v)| (k.to_string(), json!(v)))),
               "music_nodes": m(self.music_nodes.iter().map(|(k, v)| (k.to_string(), json!([v.0, v.1])))),
               "node_media": m(self.node_media.iter().map(|(k, v)| (k.to_string(), json!(v)))),
               "stingers": m(self.stingers.iter().map(|(k, v)| (k.to_string(), json!(v.iter().map(|x| json!([x.0, x.1])).collect::<Vec<_>>()))))})
    }
}

fn printable(b: u8) -> bool {
    (0x21..=0x7e).contains(&b)
}

/// Every run of 3..120 printable ASCII bytes (the regex `[\x21-\x7e]{3,120}`: longer runs split into 120-byte pieces, a
/// remainder shorter than 3 is dropped).
fn strings_into(data: &[u8], out: &mut HashSet<String>) {
    let n = data.len();
    let mut i = 0;
    while i < n {
        if !printable(data[i]) {
            i += 1;
            continue;
        }
        let s = i;
        while i < n && printable(data[i]) {
            i += 1;
        }
        for ch in data[s..i].chunks(120) {
            if ch.len() >= 3 {
                // SAFETY-free: printable ASCII is valid UTF-8
                out.insert(String::from_utf8_lossy(ch).into_owned());
            }
        }
    }
}

/// `strings_into` over a large buffer in parallel (pieces split at a non-printable byte, so no run is cut).
fn strings_parallel(data: &[u8]) -> HashSet<String> {
    let pieces = rayon::current_num_threads().max(1) * 4;
    let step = (data.len() / pieces).max(1 << 20);
    let mut cuts = vec![0usize];
    let mut at = step;
    while at < data.len() {
        while at < data.len() && printable(data[at]) {
            at += 1;
        }
        if at < data.len() {
            cuts.push(at);
        }
        at += step;
    }
    cuts.push(data.len());
    cuts.dedup();
    cuts.windows(2)
        .collect::<Vec<_>>()
        .par_iter()
        .map(|w| {
            let mut s = HashSet::new();
            strings_into(&data[w[0]..w[1]], &mut s);
            s
        })
        .reduce(HashSet::new, |mut a, b| {
            if a.len() < b.len() {
                let mut b = b;
                b.extend(a);
                return b;
            }
            a.extend(b);
            a
        })
}

fn candidates(p4k: &P4k, exe_path: Option<&Path>, progress: &dyn Fn(&str)) -> HashSet<String> {
    let mut cands: HashSet<String> = p4k
        .entries
        .par_iter()
        .fold(HashSet::new, |mut s: HashSet<String>, e| {
            for seg in e.name.replace('\\', "/").split('/') {
                s.insert(seg.to_string());
                s.insert(seg.rsplit_once('.').map_or(seg, |(a, _)| a).to_string());
            }
            s
        })
        .reduce(HashSet::new, |mut a, b| {
            if a.len() < b.len() {
                let mut b = b;
                b.extend(a);
                return b;
            }
            a.extend(b);
            a
        });
    if let Some(e) = p4k.entries.iter().find(|e| e.name.to_lowercase().ends_with("game2.dcb")) {
        progress("reading Game2.dcb strings");
        if let Ok(d) = p4k.read(e) {
            cands.extend(strings_parallel(&d));
        }
    }
    if let Some(exe) = exe_path.filter(|p| p.exists()) {
        progress("reading executable strings");
        if let Ok(f) = std::fs::File::open(exe) {
            if let Ok(m) = unsafe { memmap2::Mmap::map(&f) } {
                cands.extend(strings_parallel(&m));
            }
        }
    }
    cands
}

const FNV_BASIS: u32 = 0x811C_9DC5;

fn fnv_run(mut h: u32, bytes: &[u8]) -> u32 {
    for &c in bytes {
        h = h.wrapping_mul(0x0100_0193) ^ c.to_ascii_lowercase() as u32;
    }
    h
}

/// FNV-1 of a candidate string after the running state `h0` (the name is lower-cased; Unicode lower-casing differs from the ASCII fold only
/// for non-ASCII text).
fn fnv_str(h0: u32, s: &str) -> u32 {
    if s.is_ascii() {
        fnv_run(h0, s.as_bytes())
    } else {
        fnv_run(h0, s.to_lowercase().as_bytes())
    }
}

/// Keeps the smallest string per id (deterministic, whatever the iteration order).
fn keep_min(into: &mut HashMap<u32, String>, h: u32, name: String) {
    match into.get(&h) {
        Some(old) if *old <= name => {}
        _ => {
            into.insert(h, name);
        }
    }
}

fn merge_min(mut a: HashMap<u32, String>, b: HashMap<u32, String>) -> HashMap<u32, String> {
    for (h, n) in b {
        keep_min(&mut a, h, n);
    }
    a
}

/// `{event id: name}`; see the module docs for the candidate sources.
fn name_events(event_ids: &IdSet, bank_stems: &[String], cands: &[&String], progress: &dyn Fn(&str)) -> HashMap<u32, String> {
    progress(&format!("hashing {} strings", cands.len()));
    let mut found: HashMap<u32, String> = cands
        .par_iter()
        .fold(HashMap::new, |mut m, c| {
            let h = fnv_str(FNV_BASIS, c);
            if event_ids.contains(&h) {
                keep_min(&mut m, h, (*c).clone());
            }
            m
        })
        .reduce(HashMap::new, merge_min);
    for pre in PREFIXES {
        let h0 = fnv_run(FNV_BASIS, format!("{}_", pre).as_bytes());
        let add = cands
            .par_iter()
            .fold(HashMap::new, |mut m, c| {
                let h = fnv_str(h0, c);
                if event_ids.contains(&h) {
                    keep_min(&mut m, h, format!("{}_{}", pre, c));
                }
                m
            })
            .reduce(HashMap::new, merge_min);
        for (h, n) in add {
            found.entry(h).or_insert(n);
        }
    }
    let mut stems: Vec<&String> = bank_stems.iter().collect();
    stems.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    let stems_lower: Vec<String> = stems.iter().map(|s| s.to_lowercase()).collect();
    let mut suffixes: HashSet<String> = HashSet::new();
    for name in found.values() {
        let low = name.to_lowercase();
        let core = PREFIXES
            .iter()
            .find_map(|p| low.strip_prefix(&format!("{}_", p.to_lowercase())).map(|_| &name[p.len() + 1..]))
            .unwrap_or(name.as_str());
        let core_low = core.to_lowercase();
        for (st, stl) in stems.iter().zip(&stems_lower) {
            if core_low.starts_with(&format!("{}_", stl)) {
                suffixes.insert(core[st.len() + 1..].to_string());
                break;
            }
        }
    }
    progress(&format!("expanding {} bank names x {} suffixes", stems.len(), suffixes.len()));
    let suffixes: Vec<&String> = suffixes.iter().collect();
    let add = stems
        .par_iter()
        .fold(HashMap::new, |mut m, st| {
            for pre in PREFIXES {
                let h0 = fnv_str(FNV_BASIS, &format!("{}_{}_", pre, st));
                for suf in &suffixes {
                    let h = fnv_str(h0, suf);
                    if event_ids.contains(&h) {
                        keep_min(&mut m, h, format!("{}_{}_{}", pre, st, suf));
                    }
                }
            }
            m
        })
        .reduce(HashMap::new, merge_min);
    for (h, n) in add {
        found.entry(h).or_insert(n);
    }
    found
}

/// Reads every bank and builds the index (rayon over the banks; merged in archive order).
pub fn build(p4k: &P4k, exe_path: Option<&Path>, progress: &dyn Fn(&str)) -> SoundData {
    let names: Vec<&str> = p4k.entries.iter().filter(|e| e.name.to_lowercase().ends_with(".bnk")).map(|e| &*e.name).collect();
    progress(&format!("reading {} banks", names.len()));
    // an unreadable or truncated bank is skipped, the rest still indexes
    let parsed: Vec<Option<Bank>> = names
        .par_iter()
        .map(|n| p4k.get(n).and_then(|e| p4k.read(e).ok()).and_then(|d| bnk::parse_bank(&d).ok()))
        .collect();
    let mut banks: IndexMap<String, BankInfo> = IndexMap::new();
    let mut events: IndexMap<u32, EventInfo> = IndexMap::new();
    let mut actions: HashMap<u32, (u16, u32)> = HashMap::new();
    let mut sounds: HashMap<u32, Option<u32>> = HashMap::new();
    let mut parents: IndexMap<u32, u32> = IndexMap::new();
    let mut music: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut music_nodes: HashMap<u32, (u8, String)> = HashMap::new();
    let mut other_ids: IdSet = IdSet::default();
    let mut stingers: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
    let mut embedded: HashMap<u32, String> = HashMap::new();
    let mut act_ids: HashMap<u32, Vec<u32>> = HashMap::new();
    for (n, b) in names.iter().zip(&parsed) {
        let Some(b) = b else { continue };
        banks.insert(n.to_string(), BankInfo { id: b.id, events: b.events.len(), embedded: b.media.len() });
        actions.extend(b.actions.iter().map(|(k, v)| (*k, *v)));
        for (eid, acts) in &b.events {
            events.entry(*eid).or_default().banks.push(n.to_string());
            act_ids.insert(*eid, acts.clone());
        }
        for (sid, (mid, _stream, _par)) in &b.sounds {
            sounds.insert(*sid, *mid);
        }
        for (k, v) in &b.parents {
            parents.insert(*k, *v);
        }
        for (k, v) in &b.music_media {
            music.insert(*k, v.clone());
        }
        for (oid, s) in &b.music.segments {
            music_nodes.insert(*oid, (bnk::T_MSEGMENT, n.to_string()));
            for st in &s.node.stingers {
                stingers.entry(st.0).or_default().push((*oid, st.1));
            }
        }
        for oid in b.music.tracks.keys() {
            music_nodes.insert(*oid, (bnk::T_MTRACK, n.to_string()));
        }
        for (oid, s) in &b.music.switches {
            music_nodes.insert(*oid, (bnk::T_MSWITCH, n.to_string()));
            for st in &s.node.stingers {
                stingers.entry(st.0).or_default().push((*oid, st.1));
            }
        }
        for (oid, s) in &b.music.ranseqs {
            music_nodes.insert(*oid, (bnk::T_MRANSEQ, n.to_string()));
            for st in &s.node.stingers {
                stingers.entry(st.0).or_default().push((*oid, st.1));
            }
        }
        for (oid, (ty, _err)) in &b.music.undecoded {
            music_nodes.insert(*oid, (*ty, n.to_string()));
        }
        other_ids.extend(bnk::name_ids(b));
        for mid in b.media.keys() {
            embedded.entry(*mid).or_insert_with(|| n.to_string());
        }
    }
    drop(parsed);
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&oid, &par) in &parents {
        if par != 0 {
            children.entry(par).or_default().push(oid);
        }
    }
    let media_under = |target: u32| -> Vec<u32> {
        let (mut out, mut seen, mut stack) = (Vec::new(), HashSet::new(), vec![target]);
        while let Some(o) = stack.pop() {
            if !seen.insert(o) {
                continue;
            }
            if let Some(m) = music.get(&o) {
                out.extend(m.iter().copied());
            }
            if let Some(Some(mid)) = sounds.get(&o) {
                if !music.contains_key(&o) {
                    out.push(*mid);
                }
            }
            if let Some(c) = children.get(&o) {
                stack.extend(c.iter().copied());
            }
        }
        out
    };
    let dedupe = |v: Vec<u32>| -> Vec<u32> {
        let mut seen = HashSet::new();
        v.into_iter().filter(|m| seen.insert(*m)).collect()
    };
    for (eid, ev) in events.iter_mut() {
        ev.acts = act_ids[eid].iter().filter_map(|a| actions.get(a).copied()).collect();
    }
    let ev_list: Vec<(u32, &EventInfo)> = events.iter().map(|(k, v)| (*k, v)).collect();
    let ev_media: Vec<(u32, Vec<u32>)> = ev_list
        .par_iter()
        .map(|(eid, ev)| {
            let mut found = Vec::new();
            for &(t, tg) in &ev.acts {
                if t == 0x0403 {
                    found.extend(media_under(tg));
                }
            }
            (*eid, dedupe(found))
        })
        .collect();
    let mut event_media: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut media_events: HashMap<u32, Vec<u32>> = HashMap::new();
    for (eid, seen) in ev_media {
        for &mid in &seen {
            media_events.entry(mid).or_default().push(eid);
        }
        event_media.insert(eid, seen);
    }
    let node_ids: Vec<u32> = music_nodes.keys().copied().collect();
    let node_media: HashMap<u32, Vec<u32>> = node_ids.par_iter().map(|&oid| (oid, dedupe(media_under(oid)))).collect();
    let stems: Vec<String> = {
        let mut s: Vec<String> = banks.keys().map(|n| {
            let base = n.rsplit('/').next().unwrap_or(n);
            base[..base.len().saturating_sub(4)].to_string()
        }).collect();
        s.sort();
        s.dedup();
        s
    };
    let mut cands = candidates(p4k, exe_path, progress);
    cands.extend(stems.iter().cloned());
    let mut cand_list: Vec<&String> = cands.iter().collect();
    cand_list.sort();
    let event_ids: IdSet = events.keys().copied().collect();
    let ev_names = name_events(&event_ids, &stems, &cand_list, progress);
    progress("naming state / switch groups and values");
    let names2 = cand_list
        .par_iter()
        .fold(HashMap::new, |mut m, c| {
            let h = fnv_str(FNV_BASIS, c);
            if other_ids.contains(&h) {
                keep_min(&mut m, h, (*c).clone());
            }
            m
        })
        .reduce(HashMap::new, merge_min);
    SoundData { banks, events, event_media, media_events, embedded, names: ev_names, names2, music_nodes, node_media, stingers, played: OnceLock::new() }
}

/// Lazy, background-built index (`ready` is false while it builds).  It has no disk cache.
pub struct SoundIndex {
    data: OnceLock<SoundData>,
    pub phase: Mutex<String>,
    pub error: Mutex<Option<String>>,
    started: AtomicBool,
}

impl Default for SoundIndex {
    fn default() -> Self {
        SoundIndex { data: OnceLock::new(), phase: Mutex::new("not started".into()), error: Mutex::new(None), started: AtomicBool::new(false) }
    }
}

impl SoundIndex {
    pub fn new() -> Arc<SoundIndex> {
        Arc::new(SoundIndex::default())
    }

    /// Starts the build in a background thread (once).
    pub fn start(self: &Arc<Self>, p4k: Arc<P4k>, exe: Option<PathBuf>) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        *self.phase.lock().unwrap() = "starting".into();
        let me = self.clone();
        std::thread::spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| build(&p4k, exe.as_deref(), &|s| *me.phase.lock().unwrap() = s.to_string())));
            match r {
                Ok(d) => {
                    let _ = me.data.set(d);
                    *me.phase.lock().unwrap() = "ready".into();
                }
                Err(_) => {
                    *me.error.lock().unwrap() = Some("sound index build panicked".into());
                    *me.phase.lock().unwrap() = "failed".into();
                    me.started.store(false, Ordering::SeqCst);
                }
            }
        });
    }

    /// Builds synchronously (tests, the CLI).
    pub fn build_now(&self, p4k: &P4k, exe: Option<&Path>) -> &SoundData {
        self.started.store(true, Ordering::SeqCst);
        let d = self.data.get_or_init(|| build(p4k, exe, &|s| *self.phase.lock().unwrap() = s.to_string()));
        *self.phase.lock().unwrap() = "ready".into();
        d
    }

    pub fn data(&self) -> Option<&SoundData> {
        self.data.get()
    }

    pub fn ready(&self) -> bool {
        self.data.get().is_some()
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    #[test]
    fn sound_data_survives_the_cache_format() {
        let mut d = SoundData::default();
        d.banks.insert("b/one.bnk".into(), BankInfo { id: 7, events: 2, embedded: 1 });
        d.banks.insert("a/two.bnk".into(), BankInfo { id: 1 << 31, events: 0, embedded: 0 });
        d.events.insert(99, EventInfo { banks: vec!["b/one.bnk".into()], acts: vec![(0x0403, 5), (0x0301, 6)] });
        d.events.insert(3, EventInfo::default());
        d.event_media.insert(99, vec![1, 2, 3]);
        d.media_events.insert(1, vec![99]);
        d.embedded.insert(1, "b/one.bnk".into());
        d.names.insert(99, "Play_Door".into());
        d.names2.insert(5, "State\u{e9}".into());
        d.music_nodes.insert(8, (3, "mus.bnk".into()));
        d.node_media.insert(8, vec![4]);
        d.stingers.insert(2, vec![(8, 9), (10, 11)]);
        let b = d.to_bytes();
        let back = SoundData::from_bytes(&b).unwrap();
        assert_eq!(back.to_json(), d.to_json());
        assert_eq!(back.banks.keys().collect::<Vec<_>>(), d.banks.keys().collect::<Vec<_>>(), "IndexMap order is kept");
        assert_eq!(back.events.keys().collect::<Vec<_>>(), vec![&99, &3]);
        for n in [0, 1, b.len() / 2, b.len() - 1] {
            assert!(SoundData::from_bytes(&b[..n]).is_none(), "truncated at {}", n);
        }
    }
}
