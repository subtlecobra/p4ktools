//! Characteristics of an armor or clothing item: protection per damage type, temperature / radiation / g-force resistance, storage,
//! signature, mass, visibility, from the item's EntityClassDefinition in Game2.dcb.
//!
//! Sources (EMPIRICAL on the live archive unless noted):
//!
//! * `SCItemSuitArmorParams` (armor only): `damageResistance` -> a `DamageResistanceMacro` record whose `damageResistance` has one
//!   `DamageResistanceEntry` per type with `Multiplier`, `Threshold`, `DamageCap`, and `impactForceResistance`.  EMPIRICAL (441 items
//!   compared with the description's 'Damage Reduction: 20%'): reduction = (1 - multiplier) x 100 is what the game shows; that the
//!   multiplier scales the damage taken is the name's reading (INFERRED).  `protectedBodyParts` -> `BodyPart` records (`jointName`),
//!   `signatureParams`, `restrictedMoveViewPenalty`, `integrityMilestoneToBreak`.
//! * `SCItemClothingParams`: `TemperatureResistance.MinResistance` / `MaxResistance` (description 'Temp. Rating'), `RadiationResistance`
//!   (capacity in REM; dissipation = 'Radiation Scrub Rate'), `Flight.gForceResistance`, `Chunks`, `HiddenParts`.
//! * Storage: `SCItemInventoryContainerComponentParams.containerParams` -> an `InventoryContainer` whose `inventoryType.capacity` is in
//!   micro-SCU and `interiorDimensions` (metres); the item's footprint is `AttachDef.inventoryOccupancyVolume.microSCU`.
//! * Mass: `SEntityPhysicsControllerParams.PhysType.Mass` (kg).  Health: `SHealthComponentParams`.
//! * Radar signature of packs: `SSCSignatureSystemParams.radarProperties.baseSignatureParams.signatures` (8 slots, shown by index and
//!   flagged uncertain).

use std::collections::BTreeSet;

use serde_json::{json, Map, Value};

use crate::dynv::*;
use crate::itemstats::{comp, ItemStats, DAMAGE_TYPES};

pub const MICRO: f64 = 1_000_000.0;

/// One entry of the crafting stat table (`crafting::stat_defs`): (stat id, label, unit, mode, properties).
#[derive(Debug, Clone, PartialEq)]
pub struct StatDef {
    pub id: String,
    pub label: String,
    pub unit: String,
    pub mode: &'static str,
    pub gpps: Vec<&'static str>,
}

pub fn stat_def(id: &str, label: &str, unit: &str, mode: &'static str, gpps: &[&'static str]) -> StatDef {
    StatDef { id: id.into(), label: label.into(), unit: unit.into(), mode, gpps: gpps.to_vec() }
}

/// The armour properties of crafting blueprints (EMPIRICAL: the 922 armour and 4 clothing blueprints use only these four).
/// DamageMitigation scales the damage REDUCTION in percent ((1 - multiplier) x 100), Temperature scales the stored Min / MaxResistance.
pub fn craft_stats() -> Vec<StatDef> {
    let mut v: Vec<StatDef> = DAMAGE_TYPES
        .iter()
        .map(|t| stat_def(&format!("r:Damage reduction {}", t), &format!("Damage reduction, {}", t.to_lowercase()), "%", "mul", &["GPP_Armor_DamageMitigation"]))
        .collect();
    v.push(stat_def("r:Temperature, minimum", "Temperature, minimum", "\u{b0}C", "mul", &["GPP_Armor_TemperatureMin"]));
    v.push(stat_def("r:Temperature, maximum", "Temperature, maximum", "\u{b0}C", "mul", &["GPP_Armor_TemperatureMax"]));
    v.push(stat_def("r:Radiation protection", "Radiation protection", "REM", "mul", &["GPP_Armor_RadiationCapacity"]));
    v.push(stat_def("r:Radiation scrub rate", "Radiation scrub rate", "REM/s", "mul", &["GPP_Armor_RadiationDissipation"]));
    v
}

fn r4(v: &Value) -> Value {
    json!(pyround(f(v), 4))
}

/// `{type: {mult, thr, cap}}` of the item's damage resistance, `{}` for items without `SCItemSuitArmorParams`.
/// How the engine uses it (CONFIRMED): a body part holds ONE armor item (the last attached wins), so resistances of several worn items never
/// combine; the armor applies `mult` then the item's per-hit `cap`; `thr` is not used for bodies (only by the damage resistance of item /
/// vehicle health).
pub fn resistance(stats: &ItemStats, data: &Value) -> Map<String, Value> {
    let arm = comp(data, "SCItemSuitArmorParams");
    let rec = if truthy(arm) { stats.rref(g(arm, "damageResistance"), 6) } else { None }.unwrap_or(Value::Null);
    let dr = g(&rec, "damageResistance");
    let mut out = Map::new();
    for t in DAMAGE_TYPES {
        let e = g(dr, &format!("{}Resistance", t));
        if e.is_object() && num(g(e, "Multiplier")).is_some() {
            let thr = if truthy(g(e, "Threshold")) { g(e, "Threshold") } else { &Value::Null };
            let cap = if truthy(g(e, "DamageCap")) { g(e, "DamageCap") } else { &Value::Null };
            out.insert(t.into(), json!({"mult": r4(g(e, "Multiplier")), "thr": r4(thr), "cap": r4(cap)}));
        }
    }
    let imp = g(&rec, "impactForceResistance");
    if !out.is_empty() && num(g(imp, "impactForceResistance")).is_some() {
        out.insert("impact".into(), json!({"mult": r4(g(imp, "impactForceResistance")), "thr": 0.0, "cap": 0.0}));
    }
    out
}

/// (capacity in micro-SCU, interior dimensions (x, y, z)) of the item's own inventory container, or (0, None).
pub fn capacity(stats: &ItemStats, data: &Value) -> (i64, Option<(f64, f64, f64)>) {
    let inv = comp(data, "SCItemInventoryContainerComponentParams");
    let rec = if truthy(inv) { stats.rref(g(inv, "containerParams"), 6) } else { None };
    let Some(rec) = rec.filter(truthy) else { return (0, None) };
    let cap = g(g(g(&rec, "inventoryType"), "capacity"), "microSCU");
    let cap = if truthy(cap) { f(cap) as i64 } else { 0 };
    let d = g(&rec, "interiorDimensions");
    let dim = if d.is_object() && d.get("x").is_some() { Some((f(g(d, "x")), f(g(d, "y")), f(g(d, "z")))) } else { None };
    (cap, dim)
}

/// Flat facts of one item for the index (keys without data are absent): res, impact, tmin, tmax, rad, radrate, gforce, cap (uSCU), mass,
/// health, footprint (uSCU), ports, sig.
pub fn facts(stats: &ItemStats, data: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    let res = resistance(stats, data);
    if !res.is_empty() {
        let mut m = Map::new();
        for t in DAMAGE_TYPES {
            if let Some(e) = res.get(t) {
                m.insert(t.into(), e["mult"].clone());
            }
        }
        out.insert("res".into(), Value::Object(m));
        if let Some(i) = res.get("impact") {
            out.insert("impact".into(), i["mult"].clone());
        }
    }
    let cl = comp(data, "SCItemClothingParams");
    if truthy(cl) {
        let tr = g(cl, "TemperatureResistance");
        if num(g(tr, "MinResistance")).is_some() && (truthy(g(tr, "MinResistance")) || truthy(g(tr, "MaxResistance"))) {
            out.insert("tmin".into(), g(tr, "MinResistance").clone());
            out.insert("tmax".into(), g(tr, "MaxResistance").clone());
        }
        let rr = g(cl, "RadiationResistance");
        if truthy(g(rr, "MaximumRadiationCapacity")) {
            out.insert("rad".into(), g(rr, "MaximumRadiationCapacity").clone());
            out.insert("radrate".into(), g(rr, "RadiationDissipationRate").clone());
        }
        let gf = g(g(cl, "Flight"), "gForceResistance");
        if truthy(gf) {
            out.insert("gforce".into(), gf.clone());
        }
    }
    let (cap, _dim) = capacity(stats, data);
    if cap != 0 {
        out.insert("cap".into(), json!(cap));
    }
    let phys = g(g(comp(data, "SEntityPhysicsControllerParams"), "PhysType"), "Mass");
    if let Some(m) = num(phys) {
        out.insert("mass".into(), json!(pyround(m, 3)));
    }
    let health = comp(data, "SHealthComponentParams");
    if truthy(health) && num(g(health, "Health")).map_or(false, |h| h != 0.0) {
        out.insert("health".into(), g(health, "Health").clone());
    }
    let att = g(comp(data, "SAttachableComponentParams"), "AttachDef");
    let fp = g(g(att, "inventoryOccupancyVolume"), "microSCU");
    if truthy(fp) {
        out.insert("footprint".into(), fp.clone());
    }
    let pc = comp(data, "SItemPortContainerComponentParams");
    out.insert("ports".into(), json!(items(g(pc, "Ports")).filter(|p| truthy(g(p, "Types"))).count()));
    out
}

/// Hashable summary (as a string) of the numbers that define an item's performance: two items with the same model and the same key
/// differ only by look.
pub fn signature_key(f_: &Map<String, Value>) -> String {
    let mut res: Vec<(String, f64)> = f_.get("res").and_then(Value::as_object).map(|o| o.iter().map(|(k, v)| (k.clone(), f(v))).collect()).unwrap_or_default();
    res.sort_by(|a, b| a.0.cmp(&b.0));
    let k = |n: &str| f_.get(n).map_or("None".to_string(), pystr);
    format!("{:?}|{}|{}|{}|{}|{}|{}", res, k("impact"), k("tmin"), k("tmax"), k("rad"), k("cap"), k("mass"))
}

/// Damage reduction in percent of a resistance multiplier: 0.8 -> 20.0.
pub fn reduction(mult: f64) -> f64 {
    pyround((1.0 - mult) * 100.0, 1)
}

fn opt_join(v: BTreeSet<String>) -> Value {
    if v.is_empty() {
        Value::Null
    } else {
        json!(v.into_iter().collect::<Vec<_>>().join(", "))
    }
}

/// The detail sections of an item: Protection, Environment, Storage, Signature, Item, Visibility.  `craft` = optional
/// `{property key: value}` replacing the stored numbers (quality simulator); only the keys it carries change.
pub fn sections(stats: &ItemStats, cls: &str, data: Option<&Value>, craft: Option<&Map<String, Value>>) -> Vec<Value> {
    let owned;
    let data = match data {
        Some(d) => d,
        None => match stats.record(cls, 14) {
            Some(d) => {
                owned = d;
                &owned
            }
            None => return vec![],
        },
    };
    let empty = Map::new();
    let craft = craft.unwrap_or(&empty);
    let cget = |k: &str, dflt: &Value| craft.get(k).cloned().unwrap_or_else(|| dflt.clone());
    let fct = facts(stats, data);
    let mut secs = vec![];
    let res = resistance(stats, data);
    let mut rows: Vec<Option<Value>> = vec![];
    for t in DAMAGE_TYPES {
        let Some(e) = res.get(t) else { continue };
        let m = f(&cget(&format!("res.{}", t), &e["mult"]));
        let mut extra = vec![];
        if truthy(&e["thr"]) {
            extra.push(format!("threshold {}", gfmt(f(&e["thr"]))));
        }
        if truthy(&e["cap"]) {
            extra.push(format!("cap {}", gfmt(f(&e["cap"]))));
        }
        let tail = if extra.is_empty() { String::new() } else { format!(", {}", extra.join(", ")) };
        rows.push(Some(row(
            t,
            json!(format!("{} % reduction (x{:.2}){}", gfmt(pyround((1.0 - m) * 100.0, 1)), m, tail)),
            "",
            "",
            "reduction = 1 - Multiplier of the DamageResistanceEntry (the game's own description shows the same figure)",
        )));
    }
    if let Some(i) = res.get("impact") {
        rows.push(Some(row("Impact force", json!(format!("x{:.2}", f(&i["mult"]))), "", "uncertain", "impactForceResistance (meaning INFERRED)")));
    }
    if let Some(Value::Object(mut sec)) = section("Protection", rows) {
        let arm = comp(data, "SCItemSuitArmorParams");
        let mut names = BTreeSet::new();
        for p in arr(g(arm, "protectedBodyParts")) {
            if let Some(pr) = stats.rref(p, 4).filter(truthy) {
                let n = s(g(&pr, "jointName"));
                if !n.is_empty() {
                    names.insert(n.to_string());
                }
            }
        }
        let rws = sec.get_mut("rows").and_then(Value::as_array_mut).unwrap();
        if !names.is_empty() {
            rws.push(row1("Protects", json!(names.into_iter().collect::<Vec<_>>().join(", "))));
        }
        let mv = stats.rref(g(arm, "restrictedMoveViewPenalty"), 4).unwrap_or(Value::Null);
        for (k, label) in [("restrictedMotionPenalty", "Motion penalty"), ("restrictedViewPenalty", "View penalty")] {
            if truthy(g(&mv, k)) {
                rws.push(row1(label, json!(pyround(f(g(&mv, k)), 3))));
            }
        }
        if num(g(arm, "integrityMilestoneToBreak")).map_or(false, |x| x != 0.0) {
            rws.push(row("Breaks at integrity", json!(pyround(f(g(arm, "integrityMilestoneToBreak")), 3)), "", "uncertain", ""));
        }
        secs.push(Value::Object(sec));
    }
    let cl = comp(data, "SCItemClothingParams");
    let tr = g(cl, "TemperatureResistance");
    let rr = g(cl, "RadiationResistance");
    let has = |k: &str| fct.contains_key(k);
    let env = section(
        "Environment",
        vec![
            if has("tmin") { Some(row2("Temperature, minimum", cget("tmin", g(tr, "MinResistance")), "\u{b0}C")) } else { None },
            if has("tmax") { Some(row2("Temperature, maximum", cget("tmax", g(tr, "MaxResistance")), "\u{b0}C")) } else { None },
            if has("rad") { Some(row2("Radiation protection", cget("rad", g(rr, "MaximumRadiationCapacity")), "REM")) } else { None },
            if has("radrate") { Some(row2("Radiation scrub rate", g(rr, "RadiationDissipationRate").clone(), "REM/s")) } else { None },
            if has("gforce") { Some(row("G-force resistance", cget("gforce", &fct["gforce"]), "", "uncertain", "")) } else { None },
        ],
    );
    secs.extend(env);
    let (cap0, dim) = capacity(stats, data);
    let capv = cget("cap", &json!(cap0));
    let cap = f(&capv);
    let sto = section(
        "Storage",
        vec![
            if cap != 0.0 { Some(row2("Capacity", json!(cap / 1000.0), &format!("L ({} SCU)", fmt_g(cap / MICRO, 4)))) } else { None },
            match (cap != 0.0, dim) {
                (true, Some((x, y, z))) => Some(row1("Interior", json!(format!("{:.2} x {:.2} x {:.2} m", x, y, z)))),
                _ => None,
            },
            fct.get("footprint").map(|fp| row2("Footprint in an inventory", json!(f(fp) / 1000.0), "L")),
            if truthy(fct.get("ports").unwrap_or(&Value::Null)) { Some(row1("Item ports", fct["ports"].clone())) } else { None },
        ],
    );
    secs.extend(sto);
    let mut sig: Vec<Option<Value>> = vec![];
    let arm = comp(data, "SCItemSuitArmorParams");
    for sp in items(g(arm, "signatureParams")) {
        let ty = pystr(g(sp, "signatureType"));
        sig.push(Some(row(&format!("{} emission", ty), g(sp, "signatureEmission").clone(), "", "uncertain", "")));
        for (k, lab) in [("signatureReductionWeighted", "reduction (weighted)"), ("signatureReductionAbsolute", "reduction (absolute)")] {
            if truthy(g(sp, k)) {
                sig.push(Some(row(&format!("{} {}", ty, lab), g(sp, k).clone(), "", "uncertain", "")));
            }
        }
    }
    let ss = comp(data, "SSCSignatureSystemParams");
    let base = g(g(g(g(ss, "radarProperties"), "baseSignatureParams"), "signatures"), "");
    let _ = base;
    for (i, v) in arr(g(g(g(ss, "radarProperties"), "baseSignatureParams"), "signatures")).iter().enumerate() {
        if truthy(v) {
            sig.push(Some(row(&format!("Radar signature (slot {})", i), v.clone(), "", "uncertain", "which signature type the slot is, is not known")));
        }
    }
    secs.extend(section("Signature", sig));
    let health = comp(data, "SHealthComponentParams");
    secs.extend(section(
        "Item",
        vec![
            if has("mass") { Some(row2("Mass", cget("mass", &fct["mass"]), "kg")) } else { None },
            if truthy(health) && num(g(health, "Health")).map_or(false, |h| h != 0.0) { Some(row1("Health", g(health, "Health").clone())) } else { None },
        ],
    ));
    let chunks = g(cl, "Chunks");
    let zone = |c: &Value| s(g(c, "MeshChunk")).replace("_zone", "");
    let hidden: BTreeSet<String> = items(chunks).filter(|c| truthy(g(c, "MeshChunk"))).map(zone).collect();
    let fpv: BTreeSet<String> = items(chunks).filter(|c| truthy(g(c, "HideInFirstPerson"))).map(zone).collect();
    let hides: Vec<&str> = items(g(cl, "HiddenParts")).filter(|h| truthy(g(h, "PortName"))).map(|h| s(g(h, "PortName"))).collect();
    let layer = if arr(chunks).is_empty() {
        Value::Null
    } else {
        items(chunks).map(|c| if c.get("Layer").is_some() { g(c, "Layer").clone() } else { json!(0) }).fold(Value::Null, |a, b| {
            if a.is_null() || f(&b) > f(&a) {
                b
            } else {
                a
            }
        })
    };
    secs.extend(section(
        "Visibility",
        vec![
            Some(row1("Mesh zones hidden", opt_join(hidden))),
            Some(row1("Hidden in first person", opt_join(fpv))),
            Some(row1("Hides ports", if hides.is_empty() { Value::Null } else { json!(hides.join(", ")) })),
            Some(row1("Layer", layer)),
        ],
    ));
    secs
}

/// `[{label, value, unit}]` of the numbers crafting can change on a wearable (labels as CRAFT_STATS expects), `[]` for other items.
pub fn craft_rows(stats: &ItemStats, cls: &str) -> Vec<Value> {
    let Some(data) = stats.record(cls, 14) else { return vec![] };
    if !truthy(&data) || !(truthy(comp(&data, "SCItemSuitArmorParams")) || truthy(comp(&data, "SCItemClothingParams"))) {
        return vec![];
    }
    let fct = facts(stats, &data);
    let mut rows = vec![];
    if let Some(res) = fct.get("res").and_then(Value::as_object) {
        for (t, m) in res {
            rows.push(json!({"label": format!("Damage reduction {}", t), "value": pyround((1.0 - f(m)) * 100.0, 3), "unit": "%"}));
        }
    }
    for (key, label, unit) in [("tmin", "Temperature, minimum", "\u{b0}C"), ("tmax", "Temperature, maximum", "\u{b0}C"), ("rad", "Radiation protection", "REM"), ("radrate", "Radiation scrub rate", "REM/s")] {
        if let Some(v) = fct.get(key).filter(|v| !v.is_null()) {
            rows.push(json!({"label": label, "value": v, "unit": unit}));
        }
    }
    rows
}
