//! Hull health and armor of a ship for the "Hull & armor" tab.
//!
//! The evidence (the engine's vehicle and armor damage code, vehicle XML `<Parts>` and `<DamageMultipliers>`, armor item records): hull HP = sum of the parts'
//! `damageMax` (CONFIRMED), effective multiplier = armor `damageMultiplier` x the `bullet` part multiplier (DERIVED), the max-effectiveness
//! factor 0.75 applies to body hits only (INFERRED which).
//!
//! Limits of this default-loadout view: the armor's display `name` is the class name (the
//! localised name comes from the item-stats identity), `path` is None and `swappable` is false (every armor port of the archive
//! is `uneditable`, CONFIRMED over the archive, so no swap applies), `options` (the what-if list of other armors) is empty, and the
//! shield total is read from the default loadout without swaps.

use serde_json::{json, Map, Value};

use p4k_formats::cryxml::{self, Element};
use p4k_formats::dcb::Value as Dv;
use p4k_formats::plainxml;

use crate::ships::Ships;

pub const TYPES: [&str; 6] = ["Physical", "Energy", "Distortion", "Thermal", "Biochemical", "Stun"];
/// cvar v_armor_max_effectiveness default (CONFIRMED against the engine)
pub const MAX_EFFECTIVENESS: f64 = 0.75;

fn pf(v: Option<&str>, default: f64) -> f64 {
    v.and_then(|s| s.trim().parse::<f64>().ok()).unwrap_or(default)
}

fn df(v: Option<&Dv>, default: f64) -> f64 {
    v.and_then(Dv::as_f64).unwrap_or(default)
}

fn per_type(d: Option<&Dv>, f: impl Fn(Option<&Dv>, &str) -> f64) -> Map<String, Value> {
    TYPES.iter().map(|t| (t.to_lowercase(), json!(f(d, t)))).collect()
}

/// {type: {mult, threshold, cap}} of an SHealthComponentParams (missing entries neutral: 1.0 / 0 / 0).
pub fn resistances(health: Option<&Dv>) -> Map<String, Value> {
    let dr = health.and_then(|h| h.get("DamageResistances"));
    TYPES
        .iter()
        .map(|t| {
            let e = dr.and_then(|d| d.get(&format!("{}Resistance", t)));
            let g = |k: &str, d: f64| df(e.and_then(|e| e.get(k)), d);
            (t.to_lowercase(), json!({"mult": g("Multiplier", 1.0), "threshold": g("Threshold", 0.0), "cap": g("DamageCap", 0.0)}))
        })
        .collect()
}

fn behaviors(part: &Element) -> Vec<Value> {
    let mut out = vec![];
    let Some(db) = part.find("DamageBehaviors") else { return out };
    for b in &db.children {
        let cls = b.get("class").unwrap_or("");
        let at = pf(b.get("damageRatioMin"), 0.0);
        match cls {
            "DetachPart" => {
                let mut targets = vec![];
                let (mut dps, mut ps) = (vec![], vec![]);
                b.iter("DetachPart", &mut dps);
                for dp in dps {
                    if let Some(p) = dp.get("part").filter(|p| !p.is_empty()) {
                        targets.push(json!([p, pf(dp.get("probability"), 1.0)]));
                    }
                }
                b.iter("Part", &mut ps);
                for p in ps {
                    if let Some(n) = p.get("name").filter(|n| !n.is_empty()) {
                        targets.push(json!([n, pf(p.get("probability"), 1.0)]));
                    }
                }
                out.push(json!({"kind": "detach", "at": at, "parts": targets}));
            }
            "Group" => {
                let mut gs = vec![];
                b.iter("Group", &mut gs);
                out.push(json!({"kind": "group", "at": at, "name": gs.first().and_then(|g| g.get("name")).unwrap_or("")}));
            }
            "Destroy" | "Explosion" | "Burn" => out.push(json!({"kind": cls.to_lowercase(), "at": at})),
            _ => {}
        }
    }
    out
}

fn part(el: &Element) -> Option<Value> {
    let (mut kids, mut ports, mut plain) = (vec![], 0, 0);
    if let Some(sub) = el.find("Parts") {
        for c in sub.children.iter().filter(|c| c.tag == "Part") {
            if c.get("class") == Some("ItemPort") {
                ports += 1;
                continue;
            }
            match part(c) {
                None => plain += 1,
                Some(k) => kids.push(k),
            }
        }
    }
    let mx = pf(el.get("damageMax"), 0.0);
    if mx <= 0.0 && kids.is_empty() {
        return None;
    }
    let mut node = json!({"name": el.get("name").unwrap_or(""), "class": el.get("class").unwrap_or(""), "max": mx, "kids": kids,
                          "ports": ports, "plain": plain, "on_death": behaviors(el)});
    if el.get("mass").is_some_and(|m| !m.is_empty()) {
        node["mass"] = json!(pf(el.get("mass"), 0.0));
    }
    if el.get("detachRatio").is_some() || el.get("detachProbability").is_some() {
        node["detach"] = json!({"ratio": pf(el.get("detachRatio"), 0.0), "probability": pf(el.get("detachProbability"), 1.0)});
    }
    Some(node)
}

/// {parts: tree or null, multipliers: [...], groups: [...]} of a vehicle definition (CryXmlB or plain XML bytes).
pub fn parse_vehicle_damage(data: &[u8]) -> Option<Value> {
    let root = if cryxml::is_cryxmlb(data) { cryxml::parse(data).ok()? } else { plainxml::parse(data).ok()? };
    let tree = root.find("Parts").and_then(|top| {
        let mut kids: Vec<Value> = top.children.iter().filter(|p| p.tag == "Part" && p.get("class") != Some("ItemPort")).filter_map(part).collect();
        match kids.len() {
            0 => None,
            1 => kids.pop(),
            _ => Some(json!({"name": "", "class": "", "max": 0.0, "kids": kids, "ports": 0, "plain": 0, "on_death": []})),
        }
    });
    let mut ms = vec![];
    root.iter("DamageMultiplier", &mut ms);
    let multipliers: Vec<Value> = ms
        .iter()
        .map(|m| {
            let mut o = Map::new();
            o.insert("kind".into(), json!(m.get("damageType").unwrap_or("")));
            for (k, v) in &m.attrs {
                if k != "id" && k != "damageType" {
                    o.insert(k.clone(), json!(pf(Some(v), 0.0)));
                }
            }
            Value::Object(o)
        })
        .collect();
    let mut gs = vec![];
    root.iter("DamagesGroup", &mut gs);
    let groups: Vec<&str> = gs.iter().filter_map(|g| g.get("name")).filter(|n| !n.is_empty()).collect();
    Some(json!({"parts": tree, "multipliers": multipliers, "groups": groups}))
}

/// (sum of damageMax, number of parts with health) of a damage tree.
pub fn tree_total(node: &Value) -> (f64, usize) {
    if node.is_null() {
        return (0.0, 0);
    }
    let mx = node["max"].as_f64().unwrap_or(0.0);
    let (mut t, mut n) = (mx, usize::from(mx > 0.0));
    for k in node["kids"].as_array().into_iter().flatten() {
        let (a, b) = tree_total(k);
        t += a;
        n += b;
    }
    (t, n)
}

fn comp<'v>(d: &'v Dv, name: &str) -> Option<&'v Dv> {
    match d.get("Components") {
        Some(Dv::List(l)) => l.iter().find(|c| matches!(c, Dv::Obj(o) if o.struct_name == name)),
        _ => None,
    }
}

/// Numbers of an armor item record, None without SCItemVehicleArmorParams.
pub fn armor_item(ships: &Ships, cls: &str) -> Option<Value> {
    let d = ships.data(cls, 8)?;
    let ap = comp(&d, "SCItemVehicleArmorParams")?;
    let hp = comp(&d, "SHealthComponentParams");
    let dm = ap.get("damageMultiplier");
    let pen = ap.get("armorPenetrationResistance");
    let defl = ap.get("armorDeflection").and_then(|a| a.get("deflectionValue"));
    Some(json!({
        "cls": cls, "health": df(hp.and_then(|h| h.get("Health")), 0.0),
        "resist": hp.map(|h| Value::Object(resistances(Some(h)))),
        "mult": per_type(dm, |x, t| df(x.and_then(|x| x.get(&format!("Damage{}", t))), 1.0)),
        "signal": {"ir": df(ap.get("signalInfrared"), 1.0), "em": df(ap.get("signalElectromagnetic"), 1.0), "cs": df(ap.get("signalCrossSection"), 1.0)},
        "deflection": per_type(defl, |x, t| df(x.and_then(|x| x.get(&format!("Damage{}", t))), 0.0)),
        "penetration": {"base": df(pen.and_then(|p| p.get("basePenetrationReduction")), 1.0),
                        "absorb": per_type(pen.and_then(|p| p.get("penetrationAbsorptionForType")), |x, t| df(x.and_then(|x| x.get(&format!("Damage{}", t))), 0.0))},
    }))
}

/// {type: multiplier} of the `bullet` hit kind of the vehicle XML DamageMultipliers (INFERRED mapping: `multiplier` = physical,
/// `multiplier_Energy` / `multiplier_Distortion` their types, the other types use `multiplier`).
pub fn part_multipliers(multipliers: &Value) -> Map<String, Value> {
    let b = multipliers.as_array().into_iter().flatten().find(|m| m["kind"] == "bullet").cloned().unwrap_or(json!({}));
    let base = b["multiplier"].as_f64().unwrap_or(1.0);
    let g = |k: &str| b[k].as_f64().unwrap_or(base);
    let mut m = Map::new();
    for (k, v) in [("physical", base), ("energy", g("multiplier_Energy")), ("distortion", g("multiplier_Distortion")), ("thermal", base), ("biochemical", base), ("stun", base)] {
        m.insert(k.into(), json!(v));
    }
    m
}

/// {type: part multiplier x armor damageMultiplier} (healthy armor, the engine's order); the armor may be null.
pub fn effective(part_mult: &Map<String, Value>, armor: &Value) -> Map<String, Value> {
    part_mult
        .iter()
        .map(|(t, v)| (t.clone(), json!(v.as_f64().unwrap_or(1.0) * if armor.is_null() { 1.0 } else { armor["mult"][t].as_f64().unwrap_or(1.0) })))
        .collect()
}

/// {type: total / effective multiplier} (null where the multiplier is 0); `full_armor` also divides by 1 - MAX_EFFECTIVENESS (body hits).
pub fn ehp(total: f64, eff: &Map<String, Value>, full_armor: bool) -> Value {
    let k = if full_armor { 1.0 - MAX_EFFECTIVENESS } else { 1.0 };
    Value::Object(eff.iter().map(|(t, m)| { let d = m.as_f64().unwrap_or(0.0) * k; (t.clone(), if d != 0.0 { json!(total / d) } else { Value::Null }) }).collect())
}

/// {type: armor HP lost per 1 damage of that type} = armor resistance x armor damageMultiplier (Physical and Energy only); null without armor.
pub fn armor_loss(armor: &Value) -> Value {
    if armor.is_null() || armor["resist"].is_null() {
        return Value::Null;
    }
    Value::Object(
        armor["mult"].as_object().into_iter().flatten().map(|(t, m)| {
            let v = if t == "physical" || t == "energy" { armor["resist"][t]["mult"].as_f64().unwrap_or(1.0) * m.as_f64().unwrap_or(1.0) } else { 0.0 };
            (t.clone(), json!(v))
        }).collect(),
    )
}

/// Class of the first loadout item whose record has `AttachDef.Type == typ`, depth first in tree order.
fn find_typed(ships: &Ships, nodes: &[crate::ships::TreeNode], typ: &str) -> Option<String> {
    for n in nodes {
        if !n.cls.is_empty() {
            let is = ships.data(&n.cls, 8).is_some_and(|d| {
                comp(&d, "SAttachableComponentParams").and_then(|a| a.get("AttachDef")).and_then(|a| a.get("Type")).and_then(Dv::as_str) == Some(typ)
            });
            if is {
                return Some(n.cls.clone());
            }
        }
        if let Some(c) = find_typed(ships, &n.children, typ) {
            return Some(c);
        }
    }
    None
}

fn shield(ships: &Ships, nodes: &[crate::ships::TreeNode]) -> Value {
    let (mut total, mut gens) = (0.0, 0);
    fn walk(ships: &Ships, nodes: &[crate::ships::TreeNode], total: &mut f64, gens: &mut usize) {
        for n in nodes {
            if let Some(d) = (!n.cls.is_empty()).then(|| ships.data(&n.cls, 8)).flatten() {
                if comp(&d, "SAttachableComponentParams").and_then(|a| a.get("AttachDef")).and_then(|a| a.get("Type")).and_then(Dv::as_str) == Some("Shield") {
                    *total += df(comp(&d, "SCItemShieldGeneratorParams").and_then(|s| s.get("MaxShieldHealth")), 0.0);
                    *gens += 1;
                }
            }
            walk(ships, &n.children, total, gens);
        }
    }
    walk(ships, nodes, &mut total, &mut gens);
    if gens > 0 { json!({"max": total, "generators": gens}) } else { Value::Null }
}

/// The hull tab data of a ship (default loadout, no swaps): see the module docs for its limits.
pub fn ship_hull(ships: &Ships, cls: &str) -> Value {
    let mut unplaced = 0usize;
    let tree = ships.loadout_tree(cls, &mut unplaced, None, None);
    let node = find_typed(ships, &tree, "Armor").map(|c| ArmorNode { cls: c.clone(), name: c.clone(), port: Value::Null, path: Value::Null, swappable: false, default: c, swapped: false });
    build(ships, cls, shield(ships, &tree), node)
}

/// The loadout node the Hull tab describes as the equipped armor (the annotated tree of `ShipSwaps` supplies port, path and swap state).
struct ArmorNode {
    cls: String,
    name: String,
    port: Value,
    path: Value,
    swappable: bool,
    default: String,
    swapped: bool,
}

/// Shield total of an annotated loadout tree (a stack walk, the sum is taken in pop order).
fn shield_json(ships: &Ships, nodes: &[Value]) -> Value {
    let (mut total, mut gens) = (0.0, 0);
    let mut stack: Vec<&Value> = nodes.iter().collect();
    while let Some(n) = stack.pop() {
        stack.extend(n["children"].as_array().into_iter().flatten());
        let c = n["cls"].as_str().unwrap_or("");
        if n["type"].as_str() == Some("Shield") && !c.is_empty() {
            if let Some(d) = ships.data(c, 8) {
                total += df(comp(&d, "SCItemShieldGeneratorParams").and_then(|s| s.get("MaxShieldHealth")), 0.0);
            }
            gens += 1;
        }
    }
    if gens > 0 { json!({"max": total, "generators": gens}) } else { Value::Null }
}

/// First node of an annotated tree whose `type` is `typ` and that holds an item.
fn find_json<'v>(nodes: &'v [Value], typ: &str) -> Option<&'v Value> {
    for n in nodes {
        if n["type"].as_str() == Some(typ) && n["cls"].as_str().is_some_and(|c| !c.is_empty()) {
            return Some(n);
        }
        if let Some(f) = find_json(n["children"].as_array().map_or(&[][..], |a| &a[..]), typ) {
            return Some(f);
        }
    }
    None
}

/// The hull tab data with a live swap set: the shield and armor come
/// from the swapped loadout tree, the armor knows its port path and whether the port is swappable, and `options` lists the armors to compare
/// (the swap candidates, or every armor of the same size when the port is not swappable: EMPIRICAL, every armor port of the archive is
/// `uneditable`).  The localised armor name comes from the tree's identity.
pub fn ship_hull_swapped(ships: &Ships, ss: &crate::swaps::ShipSwaps, cls: &str, swaps: &std::collections::BTreeMap<String, String>) -> Value {
    const OPTIONS: usize = 200;
    let tree = if swaps.is_empty() { ss.default_tree(cls) } else { ss.tree(cls, swaps) };
    let node = find_json(&tree, "Armor").map(|n| {
        let c = n["cls"].as_str().unwrap_or("").to_string();
        let nm = |k: &str| n[k].as_str().filter(|s| !s.is_empty()).map_or(c.clone(), str::to_string);
        ArmorNode {
            name: nm("name"),
            default: nm("default"),
            port: n["port"].clone(),
            path: n["path"].clone(),
            swappable: n["slot"]["swappable"].as_bool().unwrap_or(false),
            swapped: n["swapped"].as_bool().unwrap_or(false),
            cls: c,
        }
    });
    let mut out = build(ships, cls, shield_json(ships, &tree), node);
    if out["armor"].is_null() {
        return out;
    }
    let armor = out["armor"].clone();
    let rows: Vec<Value> = if armor["swappable"].as_bool() == Some(true) {
        armor["path"].as_str().and_then(|p| ss.candidates(cls, p)).map_or(vec![], |c| c["items"].as_array().cloned().unwrap_or_default())
    } else {
        let size = ss.index.info(armor["cls"].as_str().unwrap_or("")).and_then(|i| i.size);
        let mut rows: Vec<Value> = ss
            .index
            .pool()
            .iter()
            .filter(|it| it.atype == "Armor" && it.size == size)
            .map(|it| json!({"cls": it.cls, "name": ss.stats.identity(&it.cls)["name"], "size": it.size, "grade": it.grade}))
            .collect();
        rows.sort_by(|a, b| (a["name"].as_str(), a["cls"].as_str()).cmp(&(b["name"].as_str(), b["cls"].as_str())));
        rows
    };
    let pm = part_multipliers(&out["multipliers"]);
    let total = out["parts_total"].as_f64().unwrap_or(0.0);
    let mut options = vec![];
    for it in rows.iter().take(OPTIONS) {
        let Some(a) = armor_item(ships, it["cls"].as_str().unwrap_or("")) else { continue };
        let eff = effective(&pm, &a);
        options.push(json!({
            "cls": it["cls"], "name": it["name"], "size": it["size"], "grade": it["grade"], "health": a["health"], "mult": a["mult"], "signal": a["signal"],
            "effective": eff, "loss": armor_loss(&a), "ehp": if total != 0.0 { ehp(total, &eff, false) } else { Value::Null },
            "resist": if a["resist"].is_null() { Value::Null } else { Value::Object(a["resist"].as_object().into_iter().flatten().map(|(t, r)| (t.clone(), r["mult"].clone())).collect()) },
        }));
    }
    out["options"] = json!(options);
    out
}

fn build(ships: &Ships, cls: &str, shield: Value, node: Option<ArmorNode>) -> Value {
    let mut out = json!({"ship": cls, "health": null, "vehicle": null, "parts": null, "parts_total": 0.0, "parts_count": 0, "multipliers": [],
                         "armor": null, "effective": null, "options": [], "shield": shield, "missing": []});
    let missing = |out: &mut Value, s: &str| out["missing"].as_array_mut().unwrap().push(json!(s));
    let Some(rec) = ships.data(cls, 8) else {
        missing(&mut out, "no entity record");
        return out;
    };
    let hp = comp(&rec, "SHealthComponentParams");
    let res = resistances(hp);
    match hp {
        Some(h) => out["health"] = json!({"max": df(h.get("Health"), 0.0), "res": res}),
        None => missing(&mut out, "the ship entity has no health component"),
    }
    let vc = comp(&rec, "VehicleComponentParams");
    let dmg = vc
        .and_then(|v| v.get("vehicleDefinition"))
        .and_then(Dv::as_str)
        .filter(|p| !p.is_empty())
        .and_then(|p| ships.p4k.find_path(p, ".xml"))
        .and_then(|e| ships.p4k.read(e).ok())
        .and_then(|b| parse_vehicle_damage(&b));
    if let Some(vc) = vc {
        let mut v = Map::new();
        for k in ["vehicleHullDamageNormalizationValue", "componentPenetrationDamageMultiplier", "fusePenetrationDamageMultiplier", "criticalPartExplosionChance",
                  "emergencyStatusDamageThreshold"] {
            if let Some(x) = vc.get(k) {
                v.insert(k.into(), x.to_json());
            }
        }
        if let Some(x) = vc.get("vehicleArmourModifiers") {
            let mods: Vec<Value> = x.as_list().unwrap_or(&[]).iter().map(|m| json!({"types": m.get("types").map_or(json!([]), Dv::to_json), "modifier": df(m.get("modifier"), 0.0)})).collect();
            v.insert("vehicleArmourModifiers".into(), json!(mods));
        }
        out["vehicle"] = Value::Object(v);
    }
    match &dmg {
        Some(d) if !d["parts"].is_null() => {
            out["parts"] = d["parts"].clone();
            let (t, n) = tree_total(&d["parts"]);
            out["parts_total"] = json!(t);
            out["parts_count"] = json!(n);
            out["multipliers"] = d["multipliers"].clone();
            if n == 0 {
                missing(&mut out, "the vehicle definition has no part with damageMax");
            }
        }
        Some(_) => missing(&mut out, "the vehicle definition has no parts"),
        None => missing(&mut out, "no vehicle definition XML"),
    }
    let ar = node.as_ref().and_then(|n| armor_item(ships, &n.cls));
    match (&node, ar) {
        (Some(n), Some(mut a)) => {
            a["name"] = json!(n.name);
            a["port"] = n.port.clone();
            a["path"] = n.path.clone();
            a["swappable"] = json!(n.swappable);
            a["default"] = json!(n.default);
            a["swapped"] = json!(n.swapped);
            out["armor"] = a;
        }
        (Some(n), None) => missing(&mut out, &format!("armor item {} has no SCItemVehicleArmorParams", n.cls)),
        _ => missing(&mut out, "no armor in the loadout"),
    }
    let pm = part_multipliers(&out["multipliers"]);
    let eff = effective(&pm, &out["armor"]);
    let total = out["parts_total"].as_f64().unwrap_or(0.0);
    out["part_mult"] = Value::Object(pm);
    out["hull_hp"] = json!(total);
    out["ehp"] = if total != 0.0 { ehp(total, &eff, false) } else { Value::Null };
    out["ehp_full"] = if total != 0.0 { ehp(total, &eff, true) } else { Value::Null };
    out["effective"] = Value::Object(eff);
    out["armor_loss"] = armor_loss(&out["armor"]);
    out["max_effectiveness"] = json!(MAX_EFFECTIVENESS);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_and_ehp() {
        let pm = part_multipliers(&json!([{"kind": "bullet", "multiplier": 1.0, "multiplier_Energy": 0.5}]));
        assert_eq!(pm["energy"], json!(0.5));
        let armor = json!({"mult": {"physical": 0.75, "energy": 0.6, "distortion": 1.0, "thermal": 1.0, "biochemical": 1.0, "stun": 1.0}});
        let eff = effective(&pm, &armor);
        assert!((eff["physical"].as_f64().unwrap() - 0.75).abs() < 1e-12);
        let e = ehp(600.0, &eff, true);
        assert!((e["physical"].as_f64().unwrap() - 600.0 / (0.75 * 0.25)).abs() < 1e-9);
        assert!(ehp(1.0, &json!({"x": 0.0}).as_object().unwrap().clone(), false)["x"].is_null());
    }

    #[test]
    fn parts_tree_prunes_parts_without_health() {
        let xml = br#"<Vehicle><Parts><Part name="Body" damageMax="100"><Parts><Part name="Wing" damageMax="50"/><Part name="Door"/><Part name="p" class="ItemPort"/></Parts></Part></Parts>
            <DamageMultipliers><DamageMultiplier damageType="bullet" multiplier="0.5"/></DamageMultipliers></Vehicle>"#;
        let d = parse_vehicle_damage(xml).unwrap();
        assert_eq!(tree_total(&d["parts"]), (150.0, 2));
        assert_eq!(d["parts"]["plain"], json!(1));
        assert_eq!(d["parts"]["ports"], json!(1));
        assert_eq!(d["multipliers"][0]["multiplier"], json!(0.5));
    }
}
