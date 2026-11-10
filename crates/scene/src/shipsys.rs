//! A ship's systems as the MFD pages need them: power plants and consumers, shield generators and faces, components with their health,
//! emission values and the hull outline for the hologram.  Everything here is read from Game2.dcb
//! records of the ship's default loadout; what the records do not say is marked INFERRED, and the client simulates the state (pip
//! allocation, damage, shield regeneration) on top of these numbers.
//!
//! EVIDENCE (live archive, AEGS_Gladius unless said otherwise)
//! * CONFIRMED  `ItemResourceComponentParams.states[].deltas[]` of a power plant holds a generation delta of resource `Power` in
//!   `SPowerSegmentResourceUnit.units` (Regulus: 16 segments).  Consumers hold consumption deltas: cooler 3 segments, shield generator 3,
//!   the flight controller 4 (`minimumConsumptionFraction` 0.25), radar 5, life support 1.  Weapons and the quantum drive consume `Power`
//!   in `SStandardResourceUnit` (not segments); thrusters consume fuel only.
//! * CONFIRMED  `powerRanges.low / medium / high.start` give the segment counts at which an item reaches its low / medium / high power
//!   range.  `ItemControlComponentParams.throttleParams` says how an item is throttled.
//! * CONFIRMED  `signatureParams.EMSignature / IRSignature.nominalSignature` per item state (plant EM 7920, cooler IR 7260 ...).
//! * CONFIRMED  `SCItemShieldGeneratorParams`: `MaxShieldHealth`, `MaxShieldRegen`, `DamagedRegenDelay`, `DownedRegenDelay` and per
//!   damage type resistance / absorption.  The face layout is `SCItemShieldEmitterParams.FaceType` on the ship's shield controller
//!   (`Bubble` or `Quadrant`), with `MaxReallocation` and `ReconfigurationCooldown` beside it.
//! * CONFIRMED  `SHealthComponentParams.Health` is the item's maximum health.
//! * CONFIRMED  the vehicle's `SSCSignatureSystemParams.radarProperties.crossSectionParams.crossSection` is a Vec3.
//! * INFERRED   which consumers belong to the three columns of the power page (guns -> weapons, the flight controller's segments ->
//!   engines, the shield generators' segments -> shields), the header numbers, the shield faces and the tab names of the component
//!   list.
//!
//! The swapped loadout tree is not built here; callers with a swapped tree pass it to [`ship_systems_for_tree`].

use std::collections::{HashMap, HashSet};

use serde_json::{json, Map, Value};

use crate::builder::{build_scene_named, GeoSource, Options};
use crate::dynv::{arr, f, g, s, truthy};
use crate::itemmaterials::ItemMaterials;
use crate::itemstats::{comp, ItemStats};
use crate::materials::MatRegistry;
use crate::ships::{Ships, TreeNode};
use p4k::P4k;

pub const TABS: [&str; 10] = ["PWR", "WPN", "THR", "SHLD", "COOL", "QT", "RADR", "MISL", "HIT", "PROX"];
pub const MAX_GUN_PIPS: usize = 8;
pub const QUADRANT_FACES: [&str; 4] = ["FRONT", "RIGHT", "BACK", "LEFT"];

/// The tab an item type belongs to (INFERRED from the names).
pub fn tab_of_type(t: &str) -> Option<&'static str> {
    Some(match t {
        "PowerPlant" => "PWR",
        "WeaponGun" | "WeaponDefensive" => "WPN",
        "MainThruster" | "ManneuverThruster" => "THR",
        "Shield" => "SHLD",
        "Cooler" => "COOL",
        "QuantumDrive" | "JumpDrive" => "QT",
        "Radar" => "RADR",
        "MissileLauncher" => "MISL",
        "Armor" => "HIT",
        _ => return None,
    })
}

#[derive(Clone)]
struct Power {
    gen: f64,
    con: f64,
    unit: Option<&'static str>,
    em: f64,
    ir: f64,
    ranges: [i64; 3],
}

/// `int(x or 0)` of a JSON number (truncates toward zero).
fn int_of(v: &Value) -> i64 {
    if truthy(v) {
        f(v) as i64
    } else {
        0
    }
}

/// (generation, consumption, unit, em, ir, ranges) of Power in the first state of an ItemResourceComponentParams: unit is segments
/// ('seg'), standard units ('std') or None.
fn power(rc: &Value) -> Power {
    let mut p = Power { gen: 0.0, con: 0.0, unit: None, em: 0.0, ir: 0.0, ranges: [0; 3] };
    for st in arr(g(rc, "states")) {
        if !truthy(st) {
            continue;
        }
        for dl in arr(g(st, "deltas")) {
            for kind in ["generation", "consumption"] {
                let x = g(dl, kind);
                if !truthy(x) || g(x, "resource").as_str() != Some("Power") {
                    continue;
                }
                let amt = g(x, "resourceAmountPerSecond");
                let (v, u) = match amt.as_object().filter(|o| o.contains_key("units")) {
                    Some(_) => (f(g(amt, "units")), "seg"),
                    None => (f(g(amt, "standardResourceUnits")), "std"),
                };
                if kind == "generation" {
                    p.gen += v;
                } else {
                    p.con += v;
                }
                p.unit = p.unit.or(Some(u));
            }
        }
        let sig = g(st, "signatureParams");
        p.em = f(g(g(sig, "EMSignature"), "nominalSignature"));
        p.ir = f(g(g(sig, "IRSignature"), "nominalSignature"));
        let pr = g(st, "powerRanges");
        for (i, k) in ["low", "medium", "high"].iter().enumerate() {
            p.ranges[i] = int_of(g(g(pr, k), "start"));
        }
        break; // the first (Online) state, like `ItemStats::resources`
    }
    p
}

/// INFERRED header CS value: the mean of the two largest axes of the vehicle's cross-section Vec3 ({x, y, z} or a 3-list).
pub fn cross_section(vec: &Value) -> f64 {
    let mut v: Vec<f64> = match vec {
        Value::Object(_) => "xyz".chars().map(|k| f(g(vec, &k.to_string()))).collect(),
        Value::Array(a) => a.iter().map(f).collect(),
        _ => vec![],
    };
    v.sort_by(|a, b| b.partial_cmp(a).unwrap());
    if v.len() >= 2 {
        (v[0] + v[1]) / 2.0
    } else {
        0.0
    }
}

fn walk<'a>(nodes: &'a [TreeNode], path: &str, out: &mut Vec<(&'a TreeNode, String)>) {
    for n in nodes {
        let here = if path.is_empty() { n.port.clone() } else { format!("{}/{}", path, n.port) };
        out.push((n, here.clone()));
        walk(&n.children, &here, out);
    }
}

struct Seen {
    power: Power,
    hp: Value,
    thr: Option<String>,
    d: Option<Value>,
}

/// JSON-able dict for one ship class: `plants`, `total` (segments), `columns` ({weapons, engines, shields}: max pips), `other` (base draw of
/// the fixed consumers), `comps` (every component of the tabs), `shields`, `cs` and `tabs`.  `total` 0 means the records gave no power
/// plant (the pages then say so).  The default loadout of the ship layer.
pub fn ship_systems(ships: &Ships, stats: &ItemStats, cls: &str) -> Value {
    let mut unplaced = 0usize;
    let tree = ships.loadout_tree(cls, &mut unplaced, None, None);
    ship_systems_for_tree(stats, cls, &tree)
}

/// The `port` / `cls` / `children` skeleton of an annotated loadout tree as `ShipSwaps::tree` returns it (JSON nodes); the systems
/// computation reads nothing else (the item type comes from the identity of the class).  Lets `/mfd/shipsys/<cls>?swap=`
/// use a swapped loadout.
pub fn tree_from_json(nodes: &[Value]) -> Vec<TreeNode> {
    nodes
        .iter()
        .map(|n| TreeNode {
            port: s(g(n, "port")).to_string(),
            cls: s(g(n, "cls")).to_string(),
            children: tree_from_json(arr(g(n, "children"))),
            geo: None,
            m: None,
            helper: None,
            sel: "",
        })
        .collect()
}

/// `ship_systems` for a given loadout tree (for a swapped loadout).
pub fn ship_systems_for_tree(stats: &ItemStats, cls: &str, tree: &[TreeNode]) -> Value {
    let (mut total, mut other, mut guns) = (0i64, 0i64, 0usize);
    let (mut c_engines, mut c_shields) = (0i64, 0i64);
    let mut plants: Vec<Value> = vec![];
    let mut comps: Vec<Value> = vec![];
    let mut seen: HashMap<String, Seen> = HashMap::new();
    let mut gens: Vec<Value> = vec![];
    let mut shield_ctl: Option<String> = None;
    let mut nodes = vec![];
    walk(tree, "", &mut nodes);
    for (n, path) in nodes {
        let c = n.cls.as_str();
        if c.is_empty() {
            continue;
        }
        let ident = stats.identity(c);
        let t = s(g(&ident, "type"));
        if t == "ShieldController" {
            shield_ctl = Some(c.to_string());
        }
        let tab = tab_of_type(t);
        if tab.is_none() && t != "FlightController" && t != "LifeSupportGenerator" {
            continue;
        }
        if !seen.contains_key(c) {
            let d = stats.record(c, 12);
            let (rc, ic, hp) = match &d {
                Some(d) => (comp(d, "ItemResourceComponentParams").clone(), comp(d, "ItemControlComponentParams").clone(), g(comp(d, "SHealthComponentParams"), "Health").clone()),
                None => (Value::Null, Value::Null, Value::Null),
            };
            let thr = arr(g(&ic, "throttleParams")).first().map(|x| s(g(x, "__struct__")).to_string()).filter(|x| !x.is_empty());
            seen.insert(c.to_string(), Seen { power: power(&rc), hp, thr, d });
        }
        let sn = &seen[c];
        let p = sn.power.clone();
        let name = {
            let nm = s(g(&ident, "name"));
            if nm.is_empty() { c.to_string() } else { nm.to_string() }
        };
        match t {
            "PowerPlant" => {
                plants.push(json!({"port": n.port, "cls": c, "name": name, "units": p.gen, "hp": sn.hp}));
                total += p.gen as i64;
            }
            "FlightController" => {
                c_engines += if p.unit == Some("seg") { p.con as i64 } else { 0 };
                continue;
            }
            "LifeSupportGenerator" => {
                other += if p.unit == Some("seg") { p.con as i64 } else { 0 };
                continue;
            }
            "WeaponGun" => guns += 1,
            "Shield" => {
                let sg = sn.d.as_ref().map(|d| comp(d, "SCItemShieldGeneratorParams").clone()).unwrap_or(Value::Null);
                c_shields += if p.unit == Some("seg") { p.con as i64 } else { 0 };
                let pairs = |k: &str| -> Vec<Value> { arr(g(&sg, k)).iter().filter(|r| truthy(r)).map(|r| json!([f(g(r, "Min")), f(g(r, "Max"))])).collect() };
                gens.push(json!({"cls": c, "name": name, "max": f(g(&sg, "MaxShieldHealth")), "regen": f(g(&sg, "MaxShieldRegen")),
                    "damaged_delay": f(g(&sg, "DamagedRegenDelay")), "downed_delay": f(g(&sg, "DownedRegenDelay")),
                    "resist": pairs("ShieldResistance"), "absorb": pairs("ShieldAbsorption")}));
            }
            _ => {}
        }
        let mut base = 0i64;
        if (t == "Cooler" || t == "Radar") && p.unit == Some("seg") {
            base = if sn.thr.as_deref() == Some("ItemPowerSegmentThrottleParams") { 2 } else { p.ranges[0].max(1) }; // radar default 2; cooler low range start
            other += base;
        }
        comps.push(json!({"id": path, "port": n.port, "cls": c, "name": name, "type": t, "tab": tab.unwrap(), "hp": f(&sn.hp),
            "draw": if p.unit.is_some() { json!(p.con) } else { json!(0) }, "unit": p.unit, "em": p.em, "ir": p.ir, "ranges": p.ranges, "throttle": sn.thr, "base": base}));
    }
    let c_weapons = if guns > 0 { guns.min(MAX_GUN_PIPS).max(1) as i64 } else { 0 };
    if c_engines == 0 && total != 0 {
        c_engines = 4;
    }
    comps.sort_by(|a, b| {
        let key = |c: &Value| (TABS.iter().position(|t| *t == s(g(c, "tab"))).unwrap_or(0), s(g(c, "name")).to_string(), s(g(c, "id")).to_string());
        key(a).cmp(&key(b))
    });
    let ctl = shield_ctl.as_deref().and_then(|c| stats.record(c, 12));
    let em_p = ctl.as_ref().map(|c| comp(c, "SCItemShieldEmitterParams").clone()).unwrap_or(Value::Null);
    let shields = if gens.is_empty() {
        Value::Null
    } else {
        let quad = s(g(&em_p, "FaceType")) == "Quadrant";
        let face_type = if truthy(g(&em_p, "FaceType")) { g(&em_p, "FaceType").clone() } else { json!("Bubble") };
        let sum = |k: &str| gens.iter().map(|x| f(g(x, k))).sum::<f64>();
        let mx = |k: &str| gens.iter().map(|x| f(g(x, k))).fold(f64::MIN, f64::max);
        json!({"face_type": face_type, "faces": if quad { json!(QUADRANT_FACES) } else { json!(["ALL"]) }, "controller": shield_ctl,
            "max": sum("max"), "regen": sum("regen"), "damaged_delay": mx("damaged_delay"), "downed_delay": mx("downed_delay"),
            "realloc": f(g(&em_p, "MaxReallocation")), "cooldown": f(g(&em_p, "ReconfigurationCooldown")), "gens": gens})
    };
    let top = stats.record(cls, 9);
    let sc = top.as_ref().map(|t| comp(t, "SSCSignatureSystemParams").clone()).unwrap_or(Value::Null);
    let xs = g(g(g(&sc, "radarProperties"), "crossSectionParams"), "crossSection");
    let cs = if truthy(xs) { cross_section(xs) } else { 0.0 };
    json!({"ship": cls, "plants": plants, "total": total, "columns": {"weapons": c_weapons, "engines": c_engines, "shields": c_shields}, "other": other,
        "comps": comps, "shields": shields, "cs": cs, "tabs": TABS})
}

// ---- hologram: feature edges of the hull model
struct Edge {
    u: [f64; 3],
    w: [f64; 3],
    n1: [f64; 3],
    n2: Option<[f64; 3]>,
}

/// `{'seg': [x1, y1, z1, x2, y2, z2, ...] ints in -1000..1000, 'ext': [sx, sy, sz]}` of the ship's hull model: the edges where
/// neighbouring triangles of the render mesh bend by more than 35 degrees plus the open boundary edges, longest first, at most
/// `max_segments`.  Ship space axes as stored in the model (CryEngine: y forward, z up).  None when the class has no hull model or it
/// cannot be read.
pub fn hull_outline(ships: &Ships, p4k: &P4k, items: &ItemMaterials, cls: &str, max_segments: usize, tri_cap: usize) -> Option<Value> {
    let rec = ships.ship_record(cls)?;
    let geo = rec.geometry.as_deref().filter(|g| !g.is_empty())?;
    let ent = p4k.find_path(geo, ".cga").or_else(|| p4k.find_path(geo, ".cgf"))?;
    let mut reg = MatRegistry::with_options(p4k, 256, false, false);
    reg.items = Some(items);
    let opts = Options { with_render: true, with_collision: false, max_render_tris: Some(tri_cap), with_frames: false, ..Options::default() };
    let (_info, parts) = build_scene_named(&GeoSource::new(p4k), &ent.name, &opts, &mut reg).ok()?;
    let q = |v: [f64; 3]| -> (i64, i64, i64) { ((v[0] * 50.0).round_ties_even() as i64, (v[1] * 50.0).round_ties_even() as i64, (v[2] * 50.0).round_ties_even() as i64) };
    let mut edges: Vec<Edge> = vec![];
    let mut index: HashMap<((i64, i64, i64), (i64, i64, i64)), usize> = HashMap::new();
    let (mut lo, mut hi) = ([1e9f64; 3], [-1e9f64; 3]);
    for pt in &parts {
        if pt.kind != "render" {
            continue;
        }
        let v = &pt.verts;
        for tri in &pt.tris {
            let (a, b, c) = (v[tri[0] as usize], v[tri[1] as usize], v[tri[2] as usize]);
            let n = [
                (b[1] - a[1]) * (c[2] - a[2]) - (b[2] - a[2]) * (c[1] - a[1]),
                (b[2] - a[2]) * (c[0] - a[0]) - (b[0] - a[0]) * (c[2] - a[2]),
                (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]),
            ];
            let ln = (n[0].powi(2) + n[1].powi(2) + n[2].powi(2)).powf(0.5);
            if ln < 1e-12 {
                continue;
            }
            let n = [n[0] / ln, n[1] / ln, n[2] / ln];
            for p in [a, b, c] {
                for k in 0..3 {
                    lo[k] = lo[k].min(p[k]);
                    hi[k] = hi[k].max(p[k]);
                }
            }
            for (u, w) in [(a, b), (b, c), (c, a)] {
                let (ku, kw) = (q(u), q(w));
                if ku == kw {
                    continue;
                }
                let key = if ku < kw { (ku, kw) } else { (kw, ku) };
                match index.get(&key) {
                    None => {
                        index.insert(key, edges.len());
                        edges.push(Edge { u, w, n1: n, n2: None });
                    }
                    Some(&i) => {
                        if edges[i].n2.is_none() {
                            edges[i].n2 = Some(n);
                        }
                    }
                }
            }
        }
    }
    if lo[0] > hi[0] {
        return None;
    }
    let mut segs: Vec<(f64, [f64; 3], [f64; 3])> = vec![];
    for e in &edges {
        if let Some(n2) = e.n2 {
            if e.n1[0] * n2[0] + e.n1[1] * n2[1] + e.n1[2] * n2[2] > 0.8192 {
                continue; // cos 35 degrees: a smooth edge
            }
        }
        segs.push(((e.u[0] - e.w[0]).powi(2) + (e.u[1] - e.w[1]).powi(2) + (e.u[2] - e.w[2]).powi(2), e.u, e.w));
    }
    segs.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap()); // stable: ties keep the insertion order
    segs.truncate(max_segments);
    let cen = [(lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0, (lo[2] + hi[2]) / 2.0];
    let half = (0..3).map(|k| hi[k] - lo[k]).fold(f64::MIN, f64::max) / 2.0;
    let half = if half == 0.0 { 1.0 } else { half };
    let mut flat: Vec<i64> = vec![];
    for (_, u, w) in &segs {
        for p in [u, w] {
            for k in 0..3 {
                flat.push((((p[k] - cen[k]) / half) * 1000.0).round_ties_even() as i64);
            }
        }
    }
    let ext: Vec<f64> = (0..3).map(|k| crate::materials::rnd(hi[k] - lo[k], 2)).collect();
    let _ = (HashSet::<u8>::new(), Map::new());
    Some(json!({"seg": flat, "ext": ext}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cross_section_mean_of_two_largest() {
        assert_eq!(cross_section(&json!({"x": 6923, "y": 1731, "z": 8654})), (8654.0 + 6923.0) / 2.0);
        assert_eq!(cross_section(&json!([1, 2, 3])), 2.5);
        assert_eq!(cross_section(&json!([1])), 0.0);
    }

    #[test]
    fn power_first_state() {
        let rc = json!({"states": [null, {"deltas": [{"generation": {"resource": "Power", "resourceAmountPerSecond": {"units": 16}}}],
            "signatureParams": {"EMSignature": {"nominalSignature": 7920}}, "powerRanges": {"low": {"start": 1}, "medium": {"start": 2}, "high": {"start": 3}}}]});
        let p = power(&rc);
        assert_eq!((p.gen, p.con, p.unit, p.em, p.ranges), (16.0, 0.0, Some("seg"), 7920.0, [1, 2, 3]));
    }

    #[test]
    fn tabs() {
        assert_eq!(tab_of_type("Cooler"), Some("COOL"));
        assert_eq!(tab_of_type("FlightController"), None);
    }
}
