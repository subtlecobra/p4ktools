//! Audio Translation Layer (ATL) data: which Wwise event an audio trigger name plays, how far it is heard, in which bank.
//!
//! The game's code and the DataCore name audio *triggers* (`Play_SSTH_..`); `Data/Libs/GameAudio/*.xml` (2,392 files, plain XML or
//! CryXmlB) and `Data/Libs/CIGAudio` define them.  EMPIRICAL on the live archive (61,911 ATLTrigger elements, no name defined twice):
//!
//! * `<ATLTrigger atl_name path><WwiseEvent wwise_name wwise_duration_type wwise_radius_max [wwise_duration_min/max]/>
//!   <RTPCDependees><ATLRTPCDependee atl_name/>..</RTPCDependees><ATLPreload atl_name/></ATLTrigger>`.  `wwise_name` is the event name
//!   whose lower-case FNV-1 is the event id in the banks (formats/bnk.rs); in the samples looked at it equals `atl_name`.
//! * `wwise_duration_type` is `Infinite` for loops (started by Play_, ended by the matching Stop_ event), `OneShot` otherwise (INFERRED
//!   from the value names).  `wwise_radius_max` is the attenuation radius in metres the middleware was authored with (INFERRED).
//! * `ATLPreload atl_name` names an `<ATLPreloadRequest>` whose `WwiseFile wwise_name` is the bank file loaded for the trigger.
//! * `<ATLRTPCDependee>` lists game parameters the event can react to.  Most triggers list the same ~40 global ones, so
//!   `specific_rtpcs` drops the names that more than `COMMON` of all triggers share.

use std::collections::{BTreeSet, HashMap, HashSet};

use p4k_formats::cryxml::{self, Element};
use p4k_formats::plainxml;
use p4k::P4k;
use indexmap::IndexMap;
use rayon::prelude::*;
use serde_json::{json, Value};

/// A dependee shared by more than this fraction of all triggers is a global, not specific to the event.
pub const COMMON: f64 = 0.2;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Trigger {
    pub event: Option<String>,
    pub dur: Option<String>,
    pub preload: Option<String>,
    pub path: Option<String>,
    /// sorted, unique ATLRTPCDependee names
    pub deps: Vec<String>,
    pub rmax: Option<f64>,
    pub dmin: Option<f64>,
    pub dmax: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rtpc {
    pub wwise: Option<String>,
    pub min: Option<String>,
    pub max: Option<String>,
    pub default: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Atl {
    pub triggers: IndexMap<String, Trigger>,
    pub preloads: IndexMap<String, Vec<String>>,
    pub rtpcs: IndexMap<String, Rtpc>,
    pub switches: IndexMap<String, Vec<String>>,
    pub common: HashSet<String>,
}

enum Op {
    Trigger(String, Trigger),
    Preload(String, Vec<String>),
    Rtpc(String, Rtpc),
    Switch(String, Vec<String>),
}

fn root_of(raw: &[u8]) -> Option<Element> {
    if cryxml::is_cryxmlb(raw) {
        cryxml::parse(raw).ok()
    } else {
        plainxml::parse(raw).ok()
    }
}

fn parse_f(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok()
}

/// The triggers, preload requests and RTPC definitions of one ATL document, in document order.
fn parse_into(root: &Element) -> Vec<Op> {
    let mut ops = Vec::new();
    for sec in &root.children {
        match sec.tag.as_str() {
            "AudioTriggers" => {
                for t in &sec.children {
                    let Some(ev) = t.find("WwiseEvent") else { continue };
                    let Some(name) = t.get("atl_name").filter(|s| !s.is_empty()) else { continue };
                    let mut dv = Vec::new();
                    t.iter("ATLRTPCDependee", &mut dv);
                    let deps: BTreeSet<String> = dv.iter().filter_map(|d| d.get("atl_name").map(str::to_string)).collect();
                    let pre = t.find("ATLPreload");
                    let mut rec = Trigger {
                        event: ev.get("wwise_name").map(str::to_string),
                        dur: ev.get("wwise_duration_type").map(str::to_string),
                        preload: pre.and_then(|p| p.get("atl_name")).map(str::to_string),
                        path: t.get("path").map(str::to_string),
                        deps: deps.into_iter().collect(),
                        ..Default::default()
                    };
                    for (k, slot) in [("wwise_radius_max", 0), ("wwise_duration_min", 1), ("wwise_duration_max", 2)] {
                        if let Some(v) = ev.get(k).filter(|s| !s.is_empty()).and_then(parse_f) {
                            match slot {
                                0 => rec.rmax = Some(v),
                                1 => rec.dmin = Some(v),
                                _ => rec.dmax = Some(v),
                            }
                        }
                    }
                    ops.push(Op::Trigger(name.to_string(), rec));
                }
            }
            "AudioPreloads" => {
                for r in &sec.children {
                    let mut ws = Vec::new();
                    r.iter("WwiseFile", &mut ws);
                    ops.push(Op::Preload(
                        r.get("atl_name").unwrap_or("None").to_string(),
                        ws.iter().map(|w| w.get("wwise_name").unwrap_or("None").to_string()).collect(),
                    ));
                }
            }
            "AudioRTPCs" => {
                for r in &sec.children {
                    if let Some(w) = r.find("WwiseRtpc") {
                        let g = |k| w.get(k).map(str::to_string);
                        ops.push(Op::Rtpc(
                            r.get("atl_name").unwrap_or("None").to_string(),
                            Rtpc { wwise: g("wwise_name"), min: g("min"), max: g("max"), default: g("default") },
                        ));
                    }
                }
            }
            "AudioSwitches" => {
                for r in &sec.children {
                    let mut ss = Vec::new();
                    r.iter("ATLSwitchState", &mut ss);
                    ops.push(Op::Switch(
                        r.get("atl_name").unwrap_or("None").to_string(),
                        ss.iter().map(|s| s.get("atl_name").unwrap_or("None").to_string()).collect(),
                    ));
                }
            }
            _ => {}
        }
    }
    ops
}

impl Atl {
    /// The data in the disk cache's binary form (`codec`); [`Atl::from_bytes`] is the inverse.  `common` is written sorted.
    pub fn to_bytes(&self) -> Vec<u8> {
        use crate::codec::Writer;
        let mut w = Writer::new();
        w.uv(self.triggers.len() as u64);
        for (k, t) in &self.triggers {
            w.str(k);
            w.opt_str(&t.event);
            w.opt_str(&t.dur);
            w.opt_str(&t.preload);
            w.opt_str(&t.path);
            w.strs(&t.deps);
            w.opt_f64(t.rmax);
            w.opt_f64(t.dmin);
            w.opt_f64(t.dmax);
        }
        for m in [&self.preloads, &self.switches] {
            w.uv(m.len() as u64);
            for (k, v) in m {
                w.str(k);
                w.strs(v);
            }
        }
        w.uv(self.rtpcs.len() as u64);
        for (k, t) in &self.rtpcs {
            w.str(k);
            for o in [&t.wwise, &t.min, &t.max, &t.default] {
                w.opt_str(o);
            }
        }
        let mut common: Vec<&String> = self.common.iter().collect();
        common.sort();
        w.uv(common.len() as u64);
        for c in common {
            w.str(c);
        }
        w.buf
    }

    pub fn from_bytes(b: &[u8]) -> Option<Atl> {
        use crate::codec::Reader;
        let mut r = Reader::new(b);
        let mut a = Atl::default();
        for _ in 0..r.count()? {
            let k = r.string()?;
            let t = Trigger { event: r.opt_str()?, dur: r.opt_str()?, preload: r.opt_str()?, path: r.opt_str()?, deps: r.strs()?, rmax: r.opt_f64()?, dmin: r.opt_f64()?, dmax: r.opt_f64()? };
            a.triggers.insert(k, t);
        }
        for which in 0..2 {
            for _ in 0..r.count()? {
                let k = r.string()?;
                let v = r.strs()?;
                if which == 0 { a.preloads.insert(k, v); } else { a.switches.insert(k, v); }
            }
        }
        for _ in 0..r.count()? {
            let k = r.string()?;
            a.rtpcs.insert(k, Rtpc { wwise: r.opt_str()?, min: r.opt_str()?, max: r.opt_str()?, default: r.opt_str()? });
        }
        for _ in 0..r.count()? {
            a.common.insert(r.string()?);
        }
        r.done().then_some(a)
    }

    /// Reads every ATL document of the archive (rayon over the files, merged in archive order).
    pub fn build(p4k: &P4k) -> Atl {
        let files: Vec<_> = p4k
            .entries
            .iter()
            .filter(|e| {
                let low = e.name.to_lowercase();
                low.ends_with(".xml") && (low.starts_with("data/libs/gameaudio/") || low.starts_with("data/libs/cigaudio/"))
            })
            .collect();
        let docs: Vec<Vec<Op>> = files
            .par_iter()
            .map(|e| match p4k.read(e) {
                // one broken file must not hide the others
                Ok(raw) => root_of(&raw).map(|r| parse_into(&r)).unwrap_or_default(),
                Err(_) => vec![],
            })
            .collect();
        let mut out = Atl::default();
        for ops in docs {
            for op in ops {
                match op {
                    Op::Trigger(n, t) => {
                        out.triggers.insert(n, t);
                    }
                    Op::Preload(n, v) => {
                        out.preloads.insert(n, v);
                    }
                    Op::Rtpc(n, v) => {
                        out.rtpcs.insert(n, v);
                    }
                    Op::Switch(n, v) => {
                        out.switches.insert(n, v);
                    }
                }
            }
        }
        let mut freq: HashMap<&str, usize> = HashMap::new();
        for t in out.triggers.values() {
            for d in &t.deps {
                *freq.entry(d).or_insert(0) += 1;
            }
        }
        let total = out.triggers.len().max(1) as f64;
        out.common = freq.into_iter().filter(|&(_, c)| c as f64 / total > COMMON).map(|(d, _)| d.to_string()).collect();
        out
    }

    pub fn trigger(&self, name: &str) -> Option<&Trigger> {
        self.triggers.get(name)
    }

    /// Bank file names (`X.bnk`) the trigger's preload request loads.
    pub fn banks(&self, trig: &Trigger) -> &[String] {
        trig.preload.as_deref().and_then(|p| self.preloads.get(p)).map_or(&[], Vec::as_slice)
    }

    pub fn specific_rtpcs(&self, trig: &Trigger) -> Vec<String> {
        trig.deps
            .iter()
            .filter(|d| !self.common.contains(*d))
            .map(|d| self.rtpcs.get(d).and_then(|r| r.wwise.clone()).filter(|w| !w.is_empty()).unwrap_or_else(|| d.clone()))
            .collect()
    }

    /// JSON of the index (the parity tests compare it; `common` is sorted).
    pub fn to_json(&self) -> Value {
        let trig: serde_json::Map<String, Value> = self
            .triggers
            .iter()
            .map(|(k, t)| {
                let mut m = serde_json::Map::new();
                m.insert("event".into(), json!(t.event));
                m.insert("dur".into(), json!(t.dur));
                m.insert("preload".into(), json!(t.preload));
                m.insert("path".into(), json!(t.path));
                m.insert("deps".into(), json!(t.deps));
                for (key, v) in [("rmax", t.rmax), ("dmin", t.dmin), ("dmax", t.dmax)] {
                    if let Some(v) = v {
                        m.insert(key.into(), json!(v));
                    }
                }
                (k.clone(), Value::Object(m))
            })
            .collect();
        let rt: serde_json::Map<String, Value> =
            self.rtpcs.iter().map(|(k, r)| (k.clone(), json!({"wwise": r.wwise, "min": r.min, "max": r.max, "default": r.default}))).collect();
        let mut common: Vec<&String> = self.common.iter().collect();
        common.sort();
        let lists = |m: &IndexMap<String, Vec<String>>| Value::Object(m.iter().map(|(k, v)| (k.clone(), json!(v))).collect());
        json!({"triggers": trig, "preloads": lists(&self.preloads), "rtpcs": rt, "switches": lists(&self.switches), "common": common})
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    #[test]
    fn atl_survives_the_cache_format() {
        let mut a = Atl::default();
        a.triggers.insert("Play_X".into(), Trigger { event: Some("Play_X".into()), dur: Some("OneShot".into()), preload: None, path: Some("p".into()), deps: vec!["a".into(), "b".into()], rmax: Some(25.5), dmin: None, dmax: Some(0.0) });
        a.triggers.insert("Stop_X".into(), Trigger::default());
        a.preloads.insert("pre".into(), vec!["f.bnk".into()]);
        a.switches.insert("sw".into(), vec![]);
        a.rtpcs.insert("r".into(), Rtpc { wwise: Some("w".into()), min: None, max: Some("1".into()), default: None });
        a.common.insert("zz".into());
        a.common.insert("aa".into());
        let b = a.to_bytes();
        let back = Atl::from_bytes(&b).unwrap();
        assert_eq!(back.triggers, a.triggers);
        assert_eq!(back.triggers.keys().collect::<Vec<_>>(), vec!["Play_X", "Stop_X"]);
        assert_eq!((back.preloads, back.switches, back.rtpcs, back.common), (a.preloads.clone(), a.switches.clone(), a.rtpcs.clone(), a.common.clone()));
        assert_eq!(Atl::from_bytes(&b).unwrap().to_bytes(), b);
        assert!(Atl::from_bytes(&b[..b.len() - 1]).is_none());
    }
}
