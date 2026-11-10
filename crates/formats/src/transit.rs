//! Transit managers and destinations (elevators, trams, shuttles): the part of an entity description that says where a car stops.
//! Callers pass the `<Entity>` element and get JSON values.
//!
//! What the data says (EMPIRICAL on the live archive; the engine's runtime behaviour is NOT confirmed):
//! * A `TransitManager` entity has `PropertiesDataCore/SCTransitManager` (`carriageInterior` = the object container of the car,
//!   `carriageAcceleration` (DataCore default 10), `carriageType`, `tagFilter` positive / negative tag GUIDs) and, baked by the editor, a second
//!   `SCTransitManager` element with `TransitDestinations`.
//! * Each `Destination` is a stop: `nameIdentifier`, `enabled`, `destinationPos` / `destinationQuat` (w,x,y,z in the MANAGER's local frame), and a
//!   `Gateway` with `gatewayPos` and `Peripheral` children (`eType` 0 = exterior call panel, 2 = gateway, 3 = limbo node).  The car is placed at the
//!   GATEWAY transform (INFERRED); engine side CONFIRMED: the initial carriage placement reads a per-carriage spec table at
//!   manager +0x150 (+0 dynamic destination index, +8 splinePointIndex, +0x20 gateway index); with either index -1 the car goes to the gateway place,
//!   else to the destination's world transform composed with the nav spline point `splinePointIndex`.
//! * Shafts with a `NavSpline` move the car along that spline; the other managers (`splineIndex` = FFFFFFFFFFFFFFFF, with a `limboPos`) have no path
//!   in the data.  `CarriageSpawnLocations` names where the car starts (`gatewayIndex`); absent = the first destination.
//! * A `TransitDestination` entity carries `Name` and `Tags`; a manager serves the destinations whose tags satisfy its `tagFilter` (INFERRED).

use std::collections::HashSet;

use serde_json::{json, Map, Value};

use crate::cryxml::Element;
use crate::mtl::py_float;
use crate::xmlq::{find, findall, iter};

pub const DEFAULT_ACCEL: f64 = 10.0;
pub const NO_SPLINE: &str = "FFFFFFFFFFFFFFFF";

fn floats(s: Option<&str>, n: usize) -> Option<Vec<f64>> {
    let v: Option<Vec<f64>> = s?.split(',').map(py_float).collect();
    v.filter(|v| v.len() == n)
}

fn tags(parent: Option<&Element>) -> Vec<String> {
    match parent {
        Some(p) => iter(p, "record").into_iter().filter_map(|r| r.get("value").filter(|v| !v.is_empty()).map(str::to_string)).collect(),
        None => vec![],
    }
}

fn tag_ids(root: &Element) -> Vec<String> {
    findall(root, "Tags/Tag").into_iter().filter_map(|t| t.get("TagId").filter(|v| !v.is_empty()).map(str::to_string)).collect()
}

/// `{'kind': 'dest', 'name': key, 'tags': [guid]}` for a `TransitDestination` entity, else None.
pub fn destination_info(root: &Element) -> Option<Value> {
    if root.get("EntityClass") != Some("TransitDestination") {
        return None;
    }
    let sc = find(root, "PropertiesDataCore/SCTransitDestination");
    Some(json!({"kind": "dest", "name": sc.and_then(|s| s.get("Name")), "tags": tag_ids(root)}))
}

/// (splineIndex, splinePointIndex) of a `Gateway` element, or None when it is not tied to a spline point.
fn index_pair(gw: Option<&Element>) -> Option<(i64, i64)> {
    let gw = gw?;
    let a = gw.get("splineIndex")?.trim().parse::<i64>().ok()?;
    let b = gw.get("splinePointIndex")?.trim().parse::<i64>().ok()?;
    Some((a, b))
}

/// Description of a `TransitManager` entity, or None for other classes:
/// `{'kind': 'manager', 'interior', 'accel', 'pos_tags', 'neg_tags', 'tags', 'dests': [{'name', 'enabled', 'pos', 'quat', 'gate', 'gate_quat', 'limbo',
/// 'spline': bool, 'spline_at': [spline, point] or None, 'panels': [[pos, quat]], 'chain': [guid]}], 'splines': [{'pos', 'quat', 'pts'}],
/// 'spawn': destination index, 'spawn_spec': raw SpawnLocation attributes or None, 'speed': travel speed from the spline control points or None}`.
pub fn manager_info(root: &Element) -> Option<Value> {
    if root.get("EntityClass") != Some("TransitManager") {
        return None;
    }
    let p = find(root, "PropertiesDataCore/SCTransitManager");
    let mut interior: Option<String> = None;
    let mut accel = DEFAULT_ACCEL;
    let (mut pos_tags, mut neg_tags) = (vec![], vec![]);
    if let Some(p) = p {
        let ci = p.find("carriageInterior");
        interior = ci.and_then(|c| c.get("path")).filter(|s| !s.is_empty()).map(str::to_string);
        if let Some(a) = p.get("carriageAcceleration").filter(|s| !s.is_empty()) {
            if let Some(v) = py_float(a) {
                accel = v;
            }
        }
        pos_tags = tags(find(p, "tagFilter/positiveTags"));
        neg_tags = tags(find(p, "tagFilter/negativeTags"));
    }
    let mut dests: Vec<Value> = vec![];
    let mut spawn: i64 = 0;
    let mut speed: Option<f64> = None;
    let mut splines: Vec<Value> = vec![];
    let scs = findall(root, "SCTransitManager");
    // `dests` is built during the walk, the spawn index is validated against the final count afterwards
    for sc in &scs {
        for dat in iter(sc, "TransitSplineDatum") {
            let pts: Vec<Option<Vec<f64>>> = findall(dat, "ControlPoint").into_iter().map(|cp| floats(cp.get("position"), 3)).collect();
            splines.push(json!({
                "pos": floats(dat.get("pos"), 3).unwrap_or(vec![0.0, 0.0, 0.0]),
                "quat": floats(dat.get("rot"), 4).unwrap_or(vec![1.0, 0.0, 0.0, 0.0]),
                "pts": pts.into_iter().flatten().collect::<Vec<_>>(),
            }));
        }
        for cp in iter(sc, "ControlPoint") {
            if let Some(v) = cp.get("speed").and_then(py_float) {
                let a = speed.filter(|s| *s != 0.0).unwrap_or(0.0);
                speed = Some(if v > a { v } else { a });
            }
        }
        for d in iter(sc, "Destination") {
            let Some(pos) = floats(d.get("destinationPos"), 3) else { continue };
            let quat = floats(d.get("destinationQuat"), 4).unwrap_or(vec![1.0, 0.0, 0.0, 0.0]);
            let gw = d.find("Gateway");
            let mut panels: Vec<Value> = vec![];
            if let Some(gw) = gw {
                for pr in gw.children.iter().filter(|c| c.tag == "Peripheral") {
                    if pr.get("eType") == Some("0") {
                        if let Some(pp) = floats(pr.get("peripheralPos"), 3) {
                            panels.push(json!([pp, floats(pr.get("peripheralQuat"), 4).unwrap_or(vec![1.0, 0.0, 0.0, 0.0])]));
                        }
                    }
                }
            }
            let gate = gw.and_then(|g| floats(g.get("gatewayPos"), 3));
            let gate_quat = gw.map(|g| floats(g.get("gatewayQuat"), 4).unwrap_or(vec![1.0, 0.0, 0.0, 0.0]));
            let limbo = gw.and_then(|g| floats(g.get("limboPos"), 3));
            let chain: Vec<&str> = match gw {
                Some(g) => g.get("Entity").filter(|s| !s.is_empty()).unwrap_or("").split(',').collect(),
                None => vec![],
            };
            dests.push(json!({
                "name": d.get("nameIdentifier").unwrap_or(""),
                "enabled": d.get("enabled").unwrap_or("true") != "false",
                "pos": pos, "quat": quat, "gate": gate, "gate_quat": gate_quat, "limbo": limbo,
                "spline_at": index_pair(gw).map(|(a, b)| json!([a, b])),
                "spline": gw.map_or(false, |g| !matches!(g.get("splineIndex"), None | Some(NO_SPLINE))),
                "panels": panels, "chain": chain,
            }));
        }
        if let Some(sl) = iter(sc, "SpawnLocation").first() {
            if let Some(v) = sl.get("gatewayIndex").and_then(|s| s.trim().parse::<i64>().ok()) {
                spawn = v;
            }
        }
    }
    let mut spawn_spec: Option<Value> = None;
    for sc in &scs {
        if let Some(sl) = iter(sc, "SpawnLocation").first() {
            let mut m = Map::new();
            for k in ["splineIndex", "splinePointIndex", "gatewayIndex"] {
                m.insert(k.to_string(), sl.get(k).map_or(Value::Null, |s| json!(s)));
            }
            spawn_spec = Some(Value::Object(m));
        }
    }
    if !(0 <= spawn && (spawn as usize) < dests.len()) {
        spawn = 0;
    }
    Some(json!({"kind": "manager", "interior": interior, "accel": accel, "pos_tags": pos_tags, "neg_tags": neg_tags, "tags": tag_ids(root),
        "dests": dests, "spawn": spawn, "speed": speed, "splines": splines, "spawn_spec": spawn_spec}))
}

/// True when a manager's tag filter accepts a destination with these tags (INFERRED: every positive tag present, no negative one).
pub fn serves(pos_tags: &[String], neg_tags: &[String], dest_tags: &[String]) -> bool {
    let tags: HashSet<&String> = dest_tags.iter().collect();
    pos_tags.iter().all(|t| tags.contains(t)) && !neg_tags.iter().any(|t| tags.contains(t))
}

/// Readable name for a localisation key: '@ui_interactor_elevator_exechangar_hangar_left' -> 'Exechangar hangar left'.  Strips the common
/// prefixes and the underscores.
pub fn label(key: &str) -> String {
    let mut s = key.strip_prefix('@').unwrap_or(key);
    s = s.strip_prefix("ui_interactor_").or_else(|| s.strip_prefix("ui_")).unwrap_or(s);
    s = s.strip_prefix("elevator_").unwrap_or(s);
    let s = s.replace('_', " ");
    let s = s.trim();
    let mut ch = s.chars();
    match ch.next() {
        Some(c) => c.to_uppercase().chain(ch).collect(),
        None => "Destination".to_string(),
    }
}

/// (seconds, accel fraction) of a trapezoidal speed profile over `dist` metres with acceleration `accel` and top speed `vmax`.  Triangle
/// (fa = 0.5) when the distance is too short to reach vmax.  INFERRED: the engine's carriage motion was not read.
pub fn travel(dist: f64, accel: f64, vmax: f64) -> (f64, f64) {
    if dist <= 1e-6 || accel <= 0.0 {
        return (0.0, 0.5);
    }
    let vmax = vmax.max(1e-6);
    let d_ramp = vmax * vmax / accel;
    if dist <= d_ramp {
        return (2.0 * (dist / accel).powf(0.5), 0.5);
    }
    let t_ramp = vmax / accel;
    let t = 2.0 * t_ramp + (dist - d_ramp) / vmax;
    (t, t_ramp / t)
}

/// Fraction of the distance covered at fraction `tau` (0..1) of the travel time under `travel`'s profile.
pub fn fraction(tau: f64, fa: f64) -> f64 {
    let tau = tau.max(0.0).min(1.0);
    let vp = 1.0 / (1.0 - fa);
    if fa <= 1e-9 {
        return tau;
    }
    if tau < fa {
        return vp * tau * tau / (2.0 * fa);
    }
    if tau <= 1.0 - fa {
        return vp * (tau - fa / 2.0);
    }
    1.0 - vp * (1.0 - tau).powi(2) / (2.0 * fa)
}
