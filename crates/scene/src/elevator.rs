//! Elevators of a zone: where the car stops, which container is the car, and how long a trip takes.
//!
//! Data (EMPIRICAL on the live archive; see `p4k_formats::transit` for the entity fields): a `TransitManager` entity lists its
//! destinations (`destinationPos` / `destinationQuat` in its own frame), names the car's container (`carriageInterior`, a
//! `Setup/elevator_setup` cab) and an acceleration.  The car is not placed in any container: the engine spawns it at runtime.  Here it is
//! drawn as an ordinary nested container with its origin at the stop transform.  APPROXIMATIONS, none of them engine data:
//!
//! * The car's origin is placed at the GATEWAY (the shaft door), with the gateway's rotation (or, for orphan destinations, the destination
//!   entity's rotation and the nearest `TransitGateway` entity's place).  The engine's attach rule was not verified against the game binary.
//! * A trip is a straight line between two stops with a trapezoidal speed profile (`transit::travel`).  Managers with a `NavSpline` would
//!   follow the spline; the straight line is the same for a plain shaft and wrong for a bend.
//! * A destination entity whose manager is NOT in the zone is tied to a manager found by scanning the archive: the manager must serve the
//!   same tags (INFERRED rule, `transit::serves`) AND list a destination with the same name key.  The scan reads every container once
//!   and is cached in `~/.cache/p4ktools` as JSON.
//! * Decals, lights and volumes inside the car container stay where the car first stood (they are drawn from static feeds); only meshes move.
//!
//! The engine rules the heuristics rest on are CONFIRMED against the engine's carriage placement for the initial placement only (see
//! `spline_point`); a survey of the stops keeps the door-alignment heuristic (`EXACT_RULE` false).

use std::collections::HashSet;
use std::sync::OnceLock;

use p4k_formats::socpak::{self, compose, quat_scale_pos_matrix, Entity, SocZip};
use p4k_formats::transit;
use p4k::P4k;
use rayon::prelude::*;
use regex::Regex;
use serde_json::{json, Value};

use crate::interact::inv34;
use crate::materials::rnd;
use crate::socpak_scene::TransitRec;

type Mat = [f64; 12];
type V3 = [f64; 3];

/// m: an exterior panel / gateway entity this close to a stop belongs to it (INFERRED, used when the manager is absent)
pub const STOP_CALL_RANGE: f64 = 12.0;
/// m/s: top speed when the manager has no spline (INFERRED)
pub const DEFAULT_SPEED: f64 = 40.0;
/// car-local position of the interior panel when the car container has none (INFERRED)
pub const FALLBACK_PANEL: V3 = [0.0, -1.5, 1.3];
/// m: an elevator door entity this close to a gateway is the door of that stop (EMPIRICAL: 0.15..1.4 m over the ship and Pyro stops that have one)
pub const DOOR_ALIGN_RANGE: f64 = 1.5;
/// degrees: two orientations closer than this are "the same facing"
pub const FACING_TOL: f64 = 5.0;
/// True = destination rotation at every spline stop (engine rule); False = door-alignment heuristic
pub const EXACT_RULE: bool = false;

fn cab_panel() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)TransitInteractionUIPanel_(Elevator|EmergencyServicesElevator)_Int").unwrap())
}

fn arr3(v: &Value) -> Option<V3> {
    let a = v.as_array()?;
    if a.len() != 3 {
        return None;
    }
    Some([a[0].as_f64()?, a[1].as_f64()?, a[2].as_f64()?])
}

fn arr4(v: &Value) -> Option<[f64; 4]> {
    let a = v.as_array()?;
    if a.len() != 4 {
        return None;
    }
    Some([a[0].as_f64()?, a[1].as_f64()?, a[2].as_f64()?, a[3].as_f64()?])
}

fn stop_matrix_of(manager_m: &Mat, pos: V3, quat: [f64; 4]) -> Mat {
    compose(manager_m, &quat_scale_pos_matrix(quat, [1.0, 1.0, 1.0], pos))
}

/// Angle in degrees between the rotations of two 12-float matrices (scale ignored; both are read as rotation + translation).
fn rot_angle(a: &Mat, b: &Mat) -> f64 {
    let col = |m: &Mat, j: usize| -> V3 {
        let v = [m[j], m[4 + j], m[8 + j]];
        let n = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        let n = if n == 0.0 { 1.0 } else { n };
        [v[0] / n, v[1] / n, v[2] / n]
    };
    let mut tr = 0.0;
    for j in 0..3 {
        let (x, y) = (col(a, j), col(b, j));
        tr += x[0] * y[0] + x[1] * y[1] + x[2] * y[2];
    }
    ((tr - 1.0) / 2.0).clamp(-1.0, 1.0).acos().to_degrees()
}

fn pos_of(m: &Mat) -> V3 {
    [m[3], m[7], m[11]]
}

fn dist(a: V3, b: V3) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

/// `place`'s translation with `rot`'s rotation (12-float row-major 3x4 matrices).
fn with_rotation(place: &Mat, rot: &Mat) -> Mat {
    [rot[0], rot[1], rot[2], place[3], rot[4], rot[5], rot[6], place[7], rot[8], rot[9], rot[10], place[11]]
}

fn xf(m: &Mat, p: V3) -> V3 {
    [m[0] * p[0] + m[1] * p[1] + m[2] * p[2] + m[3], m[4] * p[0] + m[5] * p[1] + m[6] * p[2] + m[7], m[8] * p[0] + m[9] * p[1] + m[10] * p[2] + m[11]]
}

/// One destination of a manager, read from the `transit::manager_info` JSON.
struct Dest<'a>(&'a Value);

impl Dest<'_> {
    fn name(&self) -> Option<&str> {
        self.0["name"].as_str()
    }
    fn enabled(&self) -> bool {
        self.0["enabled"].as_bool().unwrap_or(true)
    }
    fn pos(&self) -> V3 {
        arr3(&self.0["pos"]).unwrap_or([0.0; 3])
    }
    fn quat(&self) -> [f64; 4] {
        arr4(&self.0["quat"]).unwrap_or([1.0, 0.0, 0.0, 0.0])
    }
    fn gate(&self) -> Option<V3> {
        arr3(&self.0["gate"])
    }
    fn gate_quat(&self) -> Option<[f64; 4]> {
        arr4(&self.0["gate_quat"])
    }
    fn spline(&self) -> bool {
        self.0["spline"].as_bool().unwrap_or(false)
    }
    fn spline_at(&self) -> Option<(i64, i64)> {
        let a = self.0["spline_at"].as_array()?;
        Some((a.first()?.as_i64()?, a.get(1)?.as_i64()?))
    }
}

/// World matrix of a destination's stop: the gateway's place and rotation, except that a gateway rotation which disagrees with the
/// destination's AND with the stop's own shaft door is replaced by the destination's rotation when that one matches the door.  (The
/// engine rule is CONFIRMED only for the initial placement.)  Without a door nearby the gateway
/// rotation stays.
fn stop_matrix(manager_m: &Mat, d: &Dest, doors: &[Mat]) -> Mat {
    let (Some(gate), Some(gq)) = (d.gate(), d.gate_quat()) else { return stop_matrix_of(manager_m, d.pos(), d.quat()) };
    let gate_m = stop_matrix_of(manager_m, gate, gq);
    if doors.is_empty() || d.quat() == gq {
        return gate_m;
    }
    let own = stop_matrix_of(manager_m, d.pos(), d.quat());
    if rot_angle(&gate_m, &own) <= FACING_TOL {
        return gate_m;
    }
    let near: Vec<(f64, &Mat)> = doors.iter().map(|dm| (dist(pos_of(dm), pos_of(&gate_m)), dm)).filter(|t| t.0 <= DOOR_ALIGN_RANGE).collect();
    if near.is_empty() {
        return gate_m;
    }
    // min over (distance) keeps the first of equal distances
    let mut best = near[0];
    for t in &near[1..] {
        if t.0 < best.0 {
            best = *t;
        }
    }
    let door = best.1;
    if rot_angle(&gate_m, door) > FACING_TOL && rot_angle(&own, door) <= FACING_TOL {
        return with_rotation(&gate_m, &own);
    }
    gate_m
}

/// `place` moved to the spline point `pt` (rotation kept); unchanged without a point.
fn on_spline(place: &Mat, pt: Option<V3>) -> Mat {
    match pt {
        None => *place,
        Some(p) => with_rotation(&[0.0, 0.0, 0.0, p[0], 0.0, 0.0, 0.0, p[1], 0.0, 0.0, 0.0, p[2]], place),
    }
}

/// Python list indexing: negative counts from the end; None when out of range.
fn py_index(i: i64, len: usize) -> Option<usize> {
    let j = if i < 0 { i + len as i64 } else { i };
    (0 <= j && (j as usize) < len).then_some(j as usize)
}

/// World position of the spline point a destination's gateway is tied to (`splineIndex` / `splinePointIndex`), or None.
///
/// EMPIRICAL (Ironclad): the manager's baked `TransitSplineDatum` places each NavSpline in the manager frame; the elevator-shaft container
/// and the two cargo lifts put their car platform's rest position on these points.  CONFIRMED against the engine: for the
/// initial placement with valid destination and splinePointIndex the car's origin is `INavSplineData::GetPoint(splinePointIndex)` in the
/// destination's world transform; the baked control points carry no rotation, so only the position is applied here.  INFERRED: the
/// other stops use the same point.
fn spline_point(manager_m: &Mat, info: &Value, d: &Dest) -> Option<V3> {
    let (a, b) = d.spline_at()?;
    let sp = info["splines"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    // out of range -> None; negative indexes count from the end
    if a >= sp.len() as i64 {
        return None;
    }
    let s = &sp[py_index(a, sp.len())?];
    let pts = s["pts"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    if b >= pts.len() as i64 {
        return None;
    }
    let p = arr3(&pts[py_index(b, pts.len())?])?;
    let sm = stop_matrix_of(manager_m, arr3(&s["pos"])?, arr4(&s["quat"])?);
    Some(xf(&sm, p))
}

/// World matrix of a stop: the engine rule where the data has it, else the heuristic (the CONFIRMED rule covers the
/// initial placement; on the Starlancer the two disagree).
fn stop_place(manager_m: &Mat, info: &Value, d: &Dest, doors: &[Mat], at_gate: bool) -> Mat {
    if !at_gate {
        return stop_matrix_of(manager_m, d.pos(), d.quat());
    }
    let pt = spline_point(manager_m, info, d);
    if EXACT_RULE && pt.is_some() {
        return on_spline(&stop_matrix_of(manager_m, d.pos(), d.quat()), pt);
    }
    on_spline(&stop_matrix(manager_m, d, doors), pt)
}

// ---------------------------------------------------------------- the plan
#[derive(Debug, Clone)]
pub struct Stop {
    pub name: Option<String>,
    pub label: String,
    pub m: Mat,
    pub gate: Option<V3>,
    pub calls: Vec<V3>,
    pub spawn: bool,
}

impl Stop {
    /// The feed entry.
    pub fn to_json(&self) -> Value {
        json!({"name": self.name, "label": self.label, "m": self.m, "gate": self.gate, "calls": self.calls})
    }
}

/// One elevator: `{'name', 'cab': container path, 'accel', 'speed', 'source': 'manager' | 'tags', 'at': spawn stop, 'stops', 'slide', 'legs'}`.
#[derive(Debug, Clone)]
pub struct Plan {
    pub name: String,
    pub cab: String,
    pub accel: f64,
    pub speed: f64,
    pub source: &'static str,
    pub at: usize,
    pub stops: Vec<Stop>,
    pub slide: bool,
    /// n x n table of (seconds, ramp fraction) between the stops (straight line), rounded 3 / 4
    pub legs: Vec<Vec<(f64, f64)>>,
}

impl Plan {
    pub fn legs_json(&self) -> Value {
        Value::Array(self.legs.iter().map(|r| Value::Array(r.iter().map(|(t, fa)| json!({"t": t, "fa": fa})).collect())).collect())
    }
}

fn legs(stops: &[Stop], accel: f64, speed: f64) -> Vec<Vec<(f64, f64)>> {
    stops
        .iter()
        .map(|a| {
            stops
                .iter()
                .map(|b| {
                    let (t, fa) = transit::travel(dist(pos_of(&a.m), pos_of(&b.m)), accel, speed);
                    (rnd(t, 3), rnd(fa, 4))
                })
                .collect()
        })
        .collect()
}

// ---------------------------------------------------------------- managers that live outside the zone
/// A `TransitManager` of the archive that names a car container (the cache record).
#[derive(Debug, Clone)]
pub struct ExtManager {
    pub container: String,
    pub name: Option<String>,
    pub interior: String,
    pub accel: f64,
    pub speed: Option<f64>,
    pub pos_tags: Vec<String>,
    pub neg_tags: Vec<String>,
    pub dests: Vec<Option<String>>,
}

impl ExtManager {
    fn to_json(&self) -> Value {
        json!({"container": self.container, "name": self.name, "interior": self.interior, "accel": self.accel, "speed": self.speed,
               "pos_tags": self.pos_tags, "neg_tags": self.neg_tags, "dests": self.dests})
    }

    fn from_json(v: &Value) -> Option<ExtManager> {
        let strs = |k: &str| -> Vec<String> { v[k].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default() };
        Some(ExtManager {
            container: v["container"].as_str()?.to_string(),
            name: v["name"].as_str().map(str::to_string),
            interior: v["interior"].as_str()?.to_string(),
            accel: v["accel"].as_f64()?,
            speed: v["speed"].as_f64(),
            pos_tags: strs("pos_tags"),
            neg_tags: strs("neg_tags"),
            dests: v["dests"].as_array()?.iter().map(|x| x.as_str().map(str::to_string)).collect(),
        })
    }
}

/// `$XDG_CACHE_HOME` or `~/.cache`, then `p4ktools/transit-v1-<size>-<mtime>.json`.
pub fn cache_file(p4k: &P4k) -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME").map(std::path::PathBuf::from).filter(|p| !p.as_os_str().is_empty()).or_else(|| {
        std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache"))
    })?;
    let md = std::fs::metadata(&p4k.path).ok()?;
    let mtime = md.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    Some(base.join("p4ktools").join(format!("transit-v1-{}-{}.json", md.len(), mtime)))
}

fn scan_one(p4k: &P4k, name: &str) -> Vec<ExtManager> {
    let Some(entry) = p4k.get(name) else { return vec![] };
    let Ok(data) = p4k.read(entry) else { return vec![] };
    let Ok(z) = SocZip::open(&data) else { return vec![] };
    let names: Vec<String> = z.namelist().filter(|n| n.ends_with(".soc") || n.ends_with(".entxml")).map(str::to_string).collect();
    let has = |nm: &String| z.read(nm).map(|b| memchr::memmem::find(&b, b"TransitManager").is_some()).unwrap_or(false);
    if !names.iter().any(has) {
        return vec![];
    }
    drop(z);
    let Ok((ents, _)) = socpak::read_entities(&data, None, None) else { return vec![] };
    let mut out = vec![];
    for e in ents {
        let Some(t) = &e.transit else { continue };
        if t["kind"] != "manager" {
            continue;
        }
        let Some(interior) = t["interior"].as_str().filter(|s| !s.is_empty()) else { continue };
        let strs = |k: &str| -> Vec<String> { t[k].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default() };
        out.push(ExtManager {
            container: name.to_string(),
            name: Some(e.name.clone()),
            interior: interior.to_string(),
            accel: t["accel"].as_f64().unwrap_or(transit::DEFAULT_ACCEL),
            speed: t["speed"].as_f64(),
            pos_tags: strs("pos_tags"),
            neg_tags: strs("neg_tags"),
            dests: t["dests"].as_array().map(|a| a.iter().map(|d| d["name"].as_str().map(str::to_string)).collect()).unwrap_or_default(),
        });
    }
    out
}

/// Every `TransitManager` of the archive that names a car container (cached on disk: the first call reads all
/// containers, in parallel).
pub fn external_managers(p4k: &P4k) -> Vec<ExtManager> {
    let cf = cache_file(p4k);
    if let Some(Ok(text)) = cf.as_ref().map(std::fs::read_to_string) {
        if let Ok(Value::Array(a)) = serde_json::from_str::<Value>(&text) {
            let v: Vec<ExtManager> = a.iter().filter_map(ExtManager::from_json).collect();
            if v.len() == a.len() {
                return v;
            }
        }
    }
    let names: Vec<&str> = p4k.entries.iter().map(|e| e.name.as_ref()).filter(|n| n.ends_with(".socpak")).collect();
    let mut out: Vec<ExtManager> = names.par_iter().flat_map_iter(|n| scan_one(p4k, n)).collect();
    out.sort_by(|a, b| (&a.container, &a.name).cmp(&(&b.container, &b.name)));
    if let Some(cf) = cf {
        if let Some(dir) = cf.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&cf, serde_json::to_vec(&Value::Array(out.iter().map(ExtManager::to_json).collect())).unwrap_or_default());
    }
    out
}

/// The external managers (cache records) that serve a group of orphan destinations: tag filter satisfied AND a shared name key.
pub fn match_external<'a>(managers: &'a [ExtManager], dest_tags: &[String], dest_names: &[Option<String>]) -> Vec<&'a ExtManager> {
    managers
        .iter()
        .filter(|m| transit::serves(&m.pos_tags, &m.neg_tags, dest_tags) && m.dests.iter().flatten().any(|d| dest_names.iter().flatten().any(|n| n == d)))
        .collect()
}

// ---------------------------------------------------------------- planning
fn strs_of(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default()
}

/// Elevator descriptions from what `collect_instances` gathered (`Stats::transit`, `transit_ents`, `transit_doors`).  Cars are not
/// placed here (see `zone`'s `place_cars`).
///
/// Stop placement: spline point (position) when the gateway names one, else the gateway place, both with `stop_matrix`'s rotation.
pub fn plan(found: &[TransitRec], ents: &[(String, V3)], doors: &[Mat], p4k: Option<&P4k>, managers_lookup: Option<&dyn Fn() -> Vec<ExtManager>>) -> Vec<Plan> {
    let mut out: Vec<Plan> = vec![];
    let mut covered: HashSet<String> = HashSet::new();
    for rec in found {
        let info = &rec.info;
        if info["kind"] != "manager" || info["interior"].as_str().map_or(true, str::is_empty) {
            continue;
        }
        let dests: Vec<Dest> = info["dests"].as_array().map(|a| a.iter().map(Dest).collect()).unwrap_or_default();
        let spawn = info["spawn"].as_i64().unwrap_or(0).max(0) as usize;
        let mut stops = vec![];
        for (di, d) in dests.iter().enumerate() {
            if !d.enabled() {
                continue;
            }
            if let Some(n) = d.name() {
                covered.insert(n.to_string());
            }
            let at_gate = d.gate().is_some();
            stops.push(Stop {
                name: d.name().map(str::to_string),
                label: transit::label(d.name().unwrap_or("")),
                m: stop_place(&rec.m, info, d, doors, at_gate),
                gate: d.gate().map(|g| xf(&rec.m, g)),
                calls: d.0["panels"].as_array().map(|a| a.iter().filter_map(|p| arr3(&p[0])).map(|p| xf(&rec.m, p)).collect()).unwrap_or_default(),
                spawn: di == spawn,
            });
        }
        if stops.len() < 2 {
            continue;
        }
        let at = stops.iter().position(|s| s.spawn).unwrap_or(0);
        let speed = info["speed"].as_f64().filter(|s| *s != 0.0).unwrap_or(DEFAULT_SPEED);
        out.push(Plan {
            name: rec.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| "TransitManager".to_string()),
            cab: info["interior"].as_str().unwrap().to_string(),
            accel: info["accel"].as_f64().unwrap_or(transit::DEFAULT_ACCEL),
            speed,
            source: "manager",
            at,
            slide: dests.iter().filter(|d| d.enabled()).all(|d| d.spline()),
            stops,
            legs: vec![],
        });
    }
    // destinations whose manager is not in the zone
    let orphans: Vec<(&Value, &Mat, Option<&str>)> = found
        .iter()
        .filter(|r| r.info["kind"] == "dest" && r.info["name"].as_str().map_or(true, |n| !covered.contains(n)))
        .map(|r| (&r.info, &r.m, r.info["name"].as_str()))
        .collect();
    let mut groups: Vec<(Vec<String>, Vec<(&Value, &Mat, Option<&str>)>)> = vec![];
    for o in orphans {
        let mut tags = strs_of(&o.0["tags"]);
        tags.sort();
        match groups.iter_mut().find(|g| g.0 == tags) {
            Some(g) => g.1.push(o),
            None => groups.push((tags, vec![o])),
        }
    }
    if !groups.is_empty() && p4k.is_some() {
        let every = match managers_lookup {
            Some(f) => f(),
            None => external_managers(p4k.unwrap()),
        };
        for (tags, members) in &groups {
            let names: Vec<Option<String>> = members.iter().map(|m| m.2.map(str::to_string)).collect();
            let hit = match_external(&every, tags, &names);
            if hit.is_empty() || members.len() < 2 {
                continue;
            }
            let mg = hit[0];
            let mut stops = vec![];
            for (info, m, ename) in members {
                let near: Vec<(&str, V3)> = ents.iter().filter(|(_, p)| dist(pos_of(m), *p) <= STOP_CALL_RANGE).map(|(c, p)| (c.as_str(), *p)).collect();
                let mut gates: Vec<(f64, V3)> = near.iter().filter(|(c, _)| *c == "TransitGateway").map(|(_, p)| (dist(pos_of(m), *p), *p)).collect();
                gates.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap().then_with(|| a.1.partial_cmp(&b.1).unwrap()));
                let gate = gates.first().map(|g| g.1);
                let at = match gate {
                    Some(g) => [m[0], m[1], m[2], g[0], m[4], m[5], m[6], g[1], m[8], m[9], m[10], g[2]], // destination rotation, gateway place
                    None => **m,
                };
                let calls: Vec<V3> = near.iter().filter(|(c, _)| c.starts_with("TransitInteractionUIPanel") && c.contains("Ext")).map(|(_, p)| *p).collect();
                let _ = ename;
                stops.push(Stop {
                    name: info["name"].as_str().map(str::to_string),
                    label: transit::label(info["name"].as_str().unwrap_or("")),
                    m: at,
                    gate,
                    calls,
                    spawn: false,
                });
            }
            out.push(Plan {
                name: format!("{} (manager in {})", mg.name.clone().unwrap_or_else(|| "None".into()), mg.container.rsplit('/').next().unwrap()),
                cab: mg.interior.clone(),
                accel: mg.accel,
                speed: mg.speed.filter(|s| *s != 0.0).unwrap_or(DEFAULT_SPEED),
                source: "tags",
                at: 0,
                stops,
                slide: false,
                legs: vec![],
            });
        }
    }
    for e in out.iter_mut() {
        e.legs = legs(&e.stops, e.accel, e.speed);
    }
    if let Some(p) = p4k {
        localise_labels(p, &mut out);
    }
    out
}

/// Replace each stop's label by the game's own English text of its name key, when `global.ini` has it.
///
/// CONFIRMED present in Data/Localization/english/global.ini (read 2026-10): `vehicle_deck_cargo=Cargo Deck`,
/// `vehicle_deck_upper_cargo=Upper Cargo Deck` (the Ironclad lift's two stops, as its in-game panel shows them), and the other
/// `vehicle_deck_*` keys.  That the panel prints exactly this string is EMPIRICAL (user's photo).  A key without an entry keeps the
/// readable fallback of `transit::label`.
pub fn localise_labels(p4k: &P4k, plans: &mut [Plan]) {
    let mut keys: Vec<(String, usize, usize)> = vec![]; // (lowercase key, plan, stop)
    for (pi, e) in plans.iter().enumerate() {
        for (si, s) in e.stops.iter().enumerate() {
            if let Some(n) = s.name.as_deref().and_then(|n| n.strip_prefix('@')) {
                keys.push((n.to_lowercase(), pi, si));
            }
        }
    }
    if keys.is_empty() {
        return;
    }
    let Some(ent) = p4k.get("Data/Localization/english/global.ini") else { return };
    let Ok(raw) = p4k.read(ent) else { return };
    let raw = raw.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(&raw);
    let text = String::from_utf8_lossy(raw);
    // str.splitlines(): \n \r\n \r \x0b \x0c \x1c \x1d \x1e \x85
    for line in text.split(|c: char| matches!(c, '\n' | '\r' | '\u{0b}' | '\u{0c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}')) {
        let Some((k, v)) = line.split_once('=') else { continue };
        let k = k.trim().to_lowercase();
        let v = v.trim();
        if v.is_empty() {
            continue;
        }
        for (key, pi, si) in &keys {
            if *key == k {
                plans[*pi].stops[*si].label = v.replace("\\n", " ");
            }
        }
    }
}

/// The interior panel position (car-local), the fallback when the car container has none.
pub fn car_panel_local(p: Option<V3>) -> V3 {
    let p = p.unwrap_or(FALLBACK_PANEL);
    [rnd(p[0], 3), rnd(p[1], 3), rnd(p[2], 3)]
}

/// Car-local position of the interior control panel from the car container's entities, or None.
pub fn car_geometry(cab_ents: &[Entity]) -> Option<V3> {
    cab_ents.iter().find(|e| e.cls.as_deref().map_or(false, |c| !c.is_empty() && cab_panel().is_match(c))).map(|e| pos_of(&e.matrix))
}

/// Rider volume in car-local coordinates from the AreaBox volumes the car container produced while it stood at `stop_matrix`:
/// `[lo, hi]` of their union, or None.  EMPIRICAL: the rstop cabs carry one AreaBox the size of the cab.
pub fn car_box(volumes: &[(Value, Mat)], stop: &Mat) -> Option<[V3; 2]> {
    let inv = inv34(stop);
    let (mut lo, mut hi) = ([1e30f64; 3], [-1e30f64; 3]);
    for (vol, m) in volumes {
        if vol["kind"] != "areabox" {
            continue;
        }
        let local: Mat = {
            let a = vol["local"].as_array().map(Vec::as_slice).unwrap_or(&[]);
            let mut o = [0.0; 12];
            for (i, x) in a.iter().take(12).enumerate() {
                o[i] = x.as_f64().unwrap_or(0.0);
            }
            o
        };
        let full = compose(&inv, &compose(m, &local));
        let (b0, b1) = (arr3(&vol["box"][0]).unwrap_or([0.0; 3]), arr3(&vol["box"][1]).unwrap_or([0.0; 3]));
        for x in [b0[0], b1[0]] {
            for y in [b0[1], b1[1]] {
                for z in [b0[2], b1[2]] {
                    let p = xf(&full, [x, y, z]);
                    for k in 0..3 {
                        lo[k] = lo[k].min(p[k]);
                        hi[k] = hi[k].max(p[k]);
                    }
                }
            }
        }
    }
    (lo[0] <= hi[0]).then(|| [[rnd(lo[0], 3), rnd(lo[1], 3), rnd(lo[2], 3)], [rnd(hi[0], 3), rnd(hi[1], 3), rnd(hi[2], 3)]])
}

#[cfg(test)]
mod tests {
    use super::*;
    use p4k_formats::socpak::IDENT;

    #[test]
    fn rotation_angle() {
        assert!(rot_angle(&IDENT, &IDENT).abs() < 1e-9);
        let rz180 = [-1.0, 0.0, 0.0, 5.0, 0.0, -1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0];
        assert!((rot_angle(&IDENT, &rz180) - 180.0).abs() < 1e-6);
    }

    #[test]
    fn rotation_swap() {
        let place = [1.0, 0.0, 0.0, 1.0, 0.0, 1.0, 0.0, 2.0, 0.0, 0.0, 1.0, 3.0];
        let rot = [0.0, -1.0, 0.0, 9.0, 1.0, 0.0, 0.0, 9.0, 0.0, 0.0, 1.0, 9.0];
        assert_eq!(with_rotation(&place, &rot), [0.0, -1.0, 0.0, 1.0, 1.0, 0.0, 0.0, 2.0, 0.0, 0.0, 1.0, 3.0]);
        assert_eq!(on_spline(&rot, Some([1.0, 2.0, 3.0])), [0.0, -1.0, 0.0, 1.0, 1.0, 0.0, 0.0, 2.0, 0.0, 0.0, 1.0, 3.0]);
    }

    #[test]
    fn python_indexing() {
        assert_eq!(py_index(-1, 3), Some(2));
        assert_eq!(py_index(3, 3), None);
        assert_eq!(py_index(-4, 3), None);
    }

    #[test]
    fn travel_legs() {
        let s = |x: f64| Stop { name: None, label: String::new(), m: [1.0, 0.0, 0.0, x, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0], gate: None, calls: vec![], spawn: false };
        let l = legs(&[s(0.0), s(10.0)], 10.0, 40.0);
        assert_eq!(l[0][0], (0.0, 0.5));
        assert!(l[0][1].0 > 1.9 && l[0][1].1 == 0.5);
    }
}
