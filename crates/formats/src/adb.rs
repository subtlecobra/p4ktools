//! Mannequin animation databases ('.adb') of seats: SeatState fragments, sequences EnterIdle / Exit, skipState from the seat access
//! entity.

use std::collections::HashSet;

use crate::cryxml;
use crate::error::*;
use crate::plainxml;

pub const SIT_STATES: [&str; 4] = ["Deploy", "Enter", "Retract", "Select"];
pub const STAND_STATES: [&str; 4] = ["Deselect", "Deploy", "Exit", "Retract"];

#[derive(Debug, Clone)]
pub struct AdbOption {
    pub tags: Vec<String>,
    pub frag: String,
    pub anim: String,
    pub speed: f64,
}

/// An entrance state: (skipState, fragmentTags, deployRetractFragmentTags).
pub type Skip = (String, String, String);

pub fn enter_default() -> Skip {
    ("SkipDeploy".into(), String::new(), String::new())
}
pub fn exit_default() -> Skip {
    ("SkipRetract".into(), String::new(), String::new())
}

pub fn parse_adb(data: &[u8]) -> Result<Vec<AdbOption>> {
    let root = if cryxml::is_cryxmlb(data) { cryxml::parse(data)? } else { plainxml::parse(data).map_err(|e| Error::msg(format!("bad adb: {}", e)))? };
    let mut frags = vec![];
    root.iter("Fragment", &mut frags);
    let mut out = vec![];
    for frag in frags {
        let tags: Vec<String> = frag.get("Tags").unwrap_or("").split('+').filter(|t| !t.is_empty()).map(str::to_owned).collect();
        let mut ans = vec![];
        frag.iter("Animation", &mut ans);
        for an in ans {
            if let Some(name) = an.get("name") {
                if name.is_empty() {
                    continue;
                }
                let speed = match an.get("speed") {
                    None | Some("") => 1.0,
                    Some(s) => s.trim().parse::<f64>().unwrap_or(1.0),
                };
                out.push(AdbOption { tags: tags.clone(), frag: frag.get("FragTags").unwrap_or("").to_string(), anim: name.to_lowercase(), speed });
            }
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq)]
pub struct StateOption {
    pub anim: String,
    pub speed: f64,
    pub state: String,
}

impl StateOption {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({"anim": self.anim, "speed": self.speed, "state": self.state})
    }
}

/// `{sit, stand}` JSON of the pair `seat_sequences` returns.
pub fn sequences_json(seqs: &(Vec<StateOption>, Vec<StateOption>)) -> serde_json::Value {
    serde_json::json!({"sit": seqs.0.iter().map(StateOption::to_json).collect::<Vec<_>>(), "stand": seqs.1.iter().map(StateOption::to_json).collect::<Vec<_>>()})
}

fn lset(s: &str) -> HashSet<String> {
    s.split('+').filter(|t| !t.is_empty()).map(|t| t.to_lowercase()).collect()
}

/// The best matching option of fragment `state` for a seat with mannequin class tag `class_tag`.
pub fn state_option(options: &[AdbOption], class_tag: &str, state: &str, request: &[String], loose: bool) -> Option<StateOption> {
    let ct = class_tag.to_lowercase();
    let st = state.to_lowercase();
    let req: HashSet<String> = request.iter().filter(|r| !r.is_empty()).map(|r| r.to_lowercase()).collect();
    let stricts: &[bool] = if loose { &[true, false] } else { &[true] };
    for &strict in stricts {
        let sets: [(HashSet<String>, bool); 3] = [
            ([ct.clone(), "player".to_string()].into_iter().collect(), true),
            ([ct.clone(), "ai".to_string()].into_iter().collect(), true),
            (["player".to_string()].into_iter().collect(), false),
        ];
        for (allowed, specific) in sets.iter() {
            let mut best: Option<((usize, i64), &AdbOption)> = None;
            for o in options {
                let ft = lset(&o.frag);
                let tags: HashSet<String> = o.tags.iter().map(|t| t.to_lowercase()).collect();
                let extra_ok = ft.iter().filter(|t| **t != st).all(|t| req.contains(t));
                if ft.contains(&st) && (extra_ok || !strict) && tags.is_subset(allowed) && (tags.contains(&ct) || !*specific) {
                    let second = if strict { ft.intersection(&req).count() as i64 } else { -(ft.len() as i64) };
                    let key = (tags.len(), second);
                    // the first of equal keys wins
                    if best.map_or(true, |(k, _)| key > k) {
                        best = Some((key, o));
                    }
                }
            }
            if let Some((_, o)) = best {
                return Some(StateOption { anim: o.anim.clone(), speed: o.speed, state: state.to_string() });
            }
        }
    }
    None
}

/// (sit, stand): the clips the engine plays, in order, when somebody sits down / stands up.
pub fn seat_sequences(options: &[AdbOption], class_tag: &str, templates: &[String], enter: &Skip, exit: &Skip, loose: bool) -> (Vec<StateOption>, Vec<StateOption>) {
    let mut res: Vec<Vec<StateOption>> = vec![];
    for (states, ent) in [(&SIT_STATES, enter), (&STAND_STATES, exit)] {
        let dropped: &[&str] = match ent.0.to_lowercase().as_str() {
            "skipdeploy" => &["Deploy"],
            "skipretract" => &["Retract"],
            "skipboth" => &["Deploy", "Retract"],
            _ => &[],
        };
        let mut seq: Vec<StateOption> = vec![];
        for st in states.iter() {
            let mut req: Vec<String> = templates.to_vec();
            req.extend(ent.1.split('+').filter(|t| !t.is_empty()).map(str::to_owned));
            if *st == "Deploy" || *st == "Retract" {
                req.extend(ent.2.split('+').filter(|t| !t.is_empty()).map(str::to_owned));
            }
            let o = if dropped.contains(st) { None } else { state_option(options, class_tag, st, &req, loose) };
            if let Some(o) = o {
                if !seq.last().map_or(false, |l| l.anim == o.anim && l.speed == o.speed) {
                    seq.push(o);
                }
            }
        }
        res.push(seq);
    }
    let stand = res.pop().unwrap();
    let sit = res.pop().unwrap();
    if sit.is_empty() && stand.is_empty() {
        let alt = |names: &[&str]| -> Vec<StateOption> { names.iter().filter_map(|st| state_option(options, class_tag, st, &[], false)).collect() };
        return (alt(&["Open"]), alt(&["Close"]));
    }
    (sit, stand)
}
