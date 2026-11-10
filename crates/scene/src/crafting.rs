//! Crafting: the blueprints of Game2.dcb, which item each one makes, what it costs, how the quality of each ingredient changes the
//! result, and where a blueprint comes from.
//!
//! What the archive holds (EMPIRICAL on the live archive).  Nothing here is confirmed against the engine yet unless marked CONFIRMED;
//! semantics marked INFERRED are guesses from names and data shape.
//!
//! * `CraftingBlueprintRecord` (1,619): `blueprint` is a `CraftingBlueprint` with `category`, `processSpecificData` =
//!   `CraftingProcess_Creation{entityClass}` (the item made) and `tiers[]` (always exactly one).
//! * A tier's `recipe.costs` has `craftTime` and `mandatoryCost`, a `CraftingCost_Select` whose `options` are the SLOTS of the recipe.
//!   A slot option is `CraftingCost_Resource{resource, quantity.standardCargoUnits, minQuality}` or `CraftingCost_Item{entityClass,
//!   quantity, minQuality}`.  Its `context` may hold `CraftingCostContext_QuantityMultiplier{multiplier}`.
//! * A slot's `context` holds `CraftingCostContext_ResultGameplayPropertyModifiers`: a list of `{gameplayPropertyRecord, valueRanges[]}`.
//!   A value range is `..._Linear{startQuality, endQuality, modifierAtStart, modifierAtEnd}` (a factor) or
//!   `..._LinearIntegerAdditive{...}` (an integer added, used for power segments).  Qualities run 0..1000.  CONFIRMED (client
//!   exe): the client receives one quality per property and applies it with the per-property rule of `VALUE_RULES`; evaluating a
//!   slot's range at the quality of its input and combining the slots of one property happen server side: INFERRED, both readings offered.
//! * `CraftingCost_Select.count` = how many of its options must be filled (CONFIRMED: nearly all are all required); `minQuality` is the
//!   lowest input quality accepted; resource quantities are SCU, item quantities counts.
//! * `ResourceType.properties[]` holds a `qualityDistribution` (normal distribution min / max / mean / stddev over 0..1000).
//! * Pools: `BlueprintPoolRecord.blueprintRewards[] = BlueprintReward{weight, blueprintRecord}`, referenced by `ContractGenerator`
//!   records, `ScenarioProgress` reputation tiers and `MissionModifier_TemporaryBlueprint`.  CONFIRMED (exe): the weighted pick is
//!   server side.  Chance of one blueprint = weight / sum of weights (INFERRED).
//! * `LegacyCraftingRecipeDefRecord` (34) are older station recipes, indexed as `legacy` recipes without quality.

use std::collections::{BTreeMap, HashMap, HashSet};

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::dcbnav::DcbNav;
use crate::dynv::*;
use crate::gearstats::{craft_stats, stat_def, StatDef};
use crate::itemmaterials::ItemMaterials;
use crate::itemstats::{comp, is_ref3, ItemStats, RECORD};

pub const Q_MAX: f64 = 1000.0;
pub const KINDS: [(&str, &str); 7] = [
    ("ship_weapon", "Ship weapons"),
    ("fps_weapon", "FPS weapons"),
    ("fps_attachment", "FPS magazines and attachments"),
    ("component", "Ship components"),
    ("armour", "Armour"),
    ("clothing", "Clothing"),
    ("other", "Other items"),
];

/// What a property changes on the pages (INFERRED from the property names).  (stat id, label, unit, mode, properties): `m:` stats are weapon
/// metrics, `r:` stats are item card rows matched by label prefix.  Recoil Handling / Smoothness / Kick: CONFIRMED against the engine (the
/// weapon stats are built from the crafted properties and combined by multiplying per field; 1.0 = unchanged, lower = better).
pub fn stat_defs() -> Vec<StatDef> {
    let mut v = vec![
        stat_def("m:alpha", "Damage per shot", "", "mul", &["GPP_Weapon_Damage"]),
        stat_def("m:rate", "Fire rate", "rpm", "mul", &["GPP_Weapon_FireRate"]),
        stat_def("m:dps", "Damage per second", "", "mul", &["GPP_Weapon_Damage", "GPP_Weapon_FireRate"]),
        stat_def("m:spread_min", "Spread (min)", "deg", "mul", &["GPP_Weapon_Spread"]),
        stat_def("m:spread_max", "Spread (max)", "deg", "mul", &["GPP_Weapon_Spread"]),
        stat_def("m:recoil_mm", "Recoil kick", "mm", "mul", &["GPP_Weapon_Recoil_Kick"]),
        stat_def("m:aim_recoil_deg", "Aim recoil (pitch)", "deg", "mul", &["GPP_Weapon_Recoil_Handling"]),
        stat_def("m:aim_recoil", "Aim recoil", "x", "mul", &["GPP_Weapon_Recoil_Handling"]),
        stat_def("m:recoil_kick", "Recoil kick factor", "x", "mul", &["GPP_Weapon_Recoil_Kick"]),
        stat_def("m:recoil_handling", "Recoil handling factor", "x", "mul", &["GPP_Weapon_Recoil_Handling"]),
        stat_def("m:recoil_smooth", "Recoil smoothness factor", "x", "mul", &["GPP_Weapon_Recoil_Smoothness"]),
        stat_def("r:Max shield health", "Max shield health", "", "mul", &["GPP_Shield_MaxHealth"]),
        stat_def("r:Generates Power", "Power generated", "power segments", "add", &["GPP_ItemResource_PowerGeneration"]),
        stat_def("r:Generates Coolant", "Coolant generated", "units/s", "mul", &["GPP_ItemResource_CoolantGeneration"]),
        stat_def("r:Drive speed", "Quantum drive speed", "m/s", "mul", &["GPP_Quantum_Speed"]),
        stat_def("r:Health", "Health", "hp", "mul", &["GPP_Health_MaxHealth"]),
    ];
    v.extend(craft_stats());
    v
}

// ------------------------------------------------------------------------------------------------------------------ quality math
/// Value of one value range `[start, end, at_start, at_end, additive]` at quality `q`: linear between the ends (a degenerate range gives
/// its start value); None when `q` lies outside it.
pub fn range_value(rng: &Value, q: f64) -> Option<f64> {
    let (s_, e, a, b) = (f(&rng[0]), f(&rng[1]), f(&rng[2]), f(&rng[3]));
    if q < s_ || q > e {
        return None;
    }
    Some(if e <= s_ { a } else { a + (b - a) * (q - s_) / (e - s_) })
}

/// (value, additive) of a property modifier at quality `q` (clamped to 0..1000): the first range that contains it; a quality in no range
/// leaves the stat unchanged (1.0 for a factor, 0 for an additive modifier).  Additive values are rounded to integers (INFERRED from the
/// name LinearIntegerAdditive; the client applies `(int)(base + q)`, see `value_function`).
pub fn modifier_at(ranges: &[&Value], q: f64) -> (f64, bool) {
    let q = q.clamp(0.0, Q_MAX);
    let additive = ranges.first().map_or(false, |r| truthy(&r[4]));
    for r in ranges {
        if let Some(v) = range_value(r, q) {
            let add = truthy(&r[4]);
            return (if add { v.round_ties_even() } else { v }, add);
        }
    }
    (if additive { 0.0 } else { 1.0 }, additive)
}

pub const TEMP_NEUTRAL: f64 = 21.0; // degrees C (CONFIRMED, same table)

/// How the client applies a property's quality to the base stat (CONFIRMED against the client exe: a table of 29 rules); every property
/// not listed is 'mul'.
pub fn rule_of(gpp: &str) -> &'static str {
    match gpp {
        "GPP_Armor_DamageMitigation" => "mul_cap1",
        "GPP_Weapon_HullScraping_Efficiency" => "clamp01",
        "GPP_Armor_TemperatureMax" => "temp_max",
        "GPP_Armor_TemperatureMin" => "temp_min",
        "GPP_ItemResource_PowerGeneration" => "add",
        _ => "mul",
    }
}

/// Stat after the property's quality `q` (a factor) or `add` (additive) is applied to `base`, by `rule`.  `mul_cap1` caps at 1.0 for a
/// fraction (a base above 1 is read as a percentage and capped at 100: INFERRED for the pages that show percentages).
pub fn value_function(rule: &str, base: f64, q: f64, add: f64) -> f64 {
    match rule {
        "add" => (base + add).trunc(),
        "mul_cap1" => (base * q).min(if base <= 1.0 { 1.0 } else { 100.0 }),
        "clamp01" => (base * q).max(0.0).min(1.0),
        "temp_max" => {
            if base <= TEMP_NEUTRAL {
                base
            } else {
                TEMP_NEUTRAL + (base - TEMP_NEUTRAL) * q
            }
        }
        "temp_min" => {
            if base >= TEMP_NEUTRAL {
                base
            } else {
                TEMP_NEUTRAL + (base - TEMP_NEUTRAL) * q
            }
        }
        _ => base * q,
    }
}

/// Effect of the ingredient qualities on the item: `{property: {"factor": product, "factor_sum": 1 + sum of (factor - 1), "add": sum}}`.
/// `qualities` = `{slot index (as text): quality}`; a slot without one uses `default_quality`.  Only ranges of the property's own kind
/// count.  INFERRED: slot factors of one property multiply (`factor`); `factor_sum` is the alternative reading; additive ones add.
pub fn simulate(bp: &Value, qualities: &Map<String, Value>) -> Map<String, Value> {
    let mut out: Vec<(String, f64, f64, f64)> = vec![];
    let dq = if bp.get("default_quality").is_some() { f(&bp["default_quality"]) } else { 500.0 };
    for (i, slot) in arr(g(bp, "slots")).iter().enumerate() {
        let q = qualities.get(&i.to_string()).map_or(dq, f);
        for m in arr(g(slot, "mods")) {
            let gpp = s(g(m, "gpp"));
            let want_add = rule_of(gpp) == "add";
            let ranges: Vec<&Value> = arr(g(m, "ranges")).iter().filter(|r| truthy(&r[4]) == want_add).collect();
            if ranges.is_empty() {
                continue;
            }
            let (v, add) = modifier_at(&ranges, q);
            let pos = match out.iter().position(|e| e.0 == gpp) {
                Some(p) => p,
                None => {
                    out.push((gpp.to_string(), 1.0, 1.0, 0.0));
                    out.len() - 1
                }
            };
            let e = &mut out[pos];
            if add {
                e.3 += v;
            } else {
                e.1 *= v;
                e.2 += v - 1.0;
            }
        }
    }
    out.into_iter().map(|(k, a, b, c)| (k, json!({"factor": a, "factor_sum": b, "add": c}))).collect()
}

/// Stats with the crafting effects: `{stat id: new value}` for every stat in `base` that `stat_defs` ties to a property in `effects`: the
/// base goes through `value_function` of each of the stat's properties.  `combine` = 'product' or 'sum'.
pub fn apply_effects(effects: &Map<String, Value>, defs: &[StatDef], base: &Map<String, Value>, combine: &str) -> Map<String, Value> {
    let key = if combine == "sum" { "factor_sum" } else { "factor" };
    let mut out = Map::new();
    for d in defs {
        let Some(b) = base.get(&d.id).filter(|b| !b.is_null()) else { continue };
        if !d.gpps.iter().any(|g_| effects.contains_key(*g_)) {
            continue;
        }
        let mut v = f(b);
        for g_ in &d.gpps {
            if let Some(e) = effects.get(*g_) {
                v = value_function(rule_of(g_), v, f(&e[key]), f(&e["add"]));
            }
        }
        out.insert(d.id.clone(), json!(v));
    }
    out
}

/// How the game shows a factor (`CraftingGameplayPropertyDef.displayTransformation` as the list of transformation struct names, applied in
/// order): a percent change `(f-1)*100`, its negation, or a scale.  Returns (number, unit) with unit '%' for the percent forms.
pub fn display_value(transform: &[&str], factor: f64) -> (f64, &'static str) {
    let (mut v, mut unit) = (factor, "");
    for t in transform {
        if *t == "CraftingDisplayTransformation_ConvertFactorToPercentChange" {
            v = (v - 1.0) * 100.0;
            unit = "%";
        } else if *t == "CraftingDisplayTransformation_ConvertFactorToNegatedPercentChange" {
            v = -(v - 1.0) * 100.0;
            unit = "%";
        }
    }
    (v, unit)
}

/// 'VehicleComponentS2' -> 'Vehicle component S2', 'FPSWeapons' -> 'FPS weapons'.
pub fn pretty_category(name: &str) -> String {
    let cs: Vec<char> = name.chars().collect();
    let mut sp = String::new();
    for i in 0..cs.len() {
        if i > 0 {
            let (a, b) = (cs[i - 1], cs[i]);
            let c1 = a.is_ascii_lowercase() && (b.is_ascii_uppercase() || b.is_ascii_digit());
            let c2 = a.is_ascii_uppercase() && b.is_ascii_uppercase() && cs.get(i + 1).map_or(false, |c| c.is_ascii_lowercase());
            if c1 || c2 {
                sp.push(' ');
            }
        }
        sp.push(cs[i]);
    }
    let mut it = sp.chars();
    let cap = match it.next() {
        Some(c) => c.to_uppercase().collect::<String>() + &it.as_str().to_lowercase(),
        None => String::new(),
    };
    let fps = Regex::new(r"^Fps\b").unwrap().replace(&cap, "FPS").to_string();
    Regex::new(r"\b[sS](\d)\b").unwrap().replace_all(&fps, "S$1").to_string()
}

/// Kind id of an output item from its AttachDef.Type, else from its blueprint category.
pub fn kind_of(att_type: &str, category: &str) -> &'static str {
    match att_type {
        "WeaponPersonal" => return "fps_weapon",
        "WeaponAttachment" => return "fps_attachment",
        "WeaponGun" | "Missile" | "Bomb" | "MissileLauncher" | "BombLauncher" | "Turret" | "TurretBase" => return "ship_weapon",
        _ => {}
    }
    if att_type.starts_with("Char_Armor") {
        "armour"
    } else if att_type.starts_with("Char_Clothing") {
        "clothing"
    } else if category.starts_with("VehicleComponent") {
        "component"
    } else if category.starts_with("VehicleWeapons") {
        "ship_weapon"
    } else {
        "other"
    }
}

pub fn time_seconds(t: &Value) -> f64 {
    if !t.is_object() {
        return 0.0;
    }
    f(g(t, "days")) * 86400.0 + f(g(t, "hours")) * 3600.0 + f(g(t, "minutes")) * 60.0 + f(g(t, "seconds"))
}

pub fn time_text(sec: f64) -> String {
    let sec = sec.round_ties_even() as i64;
    if sec < 60 {
        return format!("{} s", sec);
    }
    let (h, m, s_) = (sec / 3600, sec / 60 % 60, sec % 60);
    [(h, "h"), (m, "min"), (s_, "s")].iter().filter(|(n, _)| *n != 0).map(|(n, u)| format!("{} {}", n, u)).collect::<Vec<_>>().join(" ")
}

// ------------------------------------------------------------------------------------------------------------------ reading
/// Records of each struct type, in record order (one pass over the record table).
pub fn records_by_struct(items: &ItemMaterials) -> HashMap<String, Vec<usize>> {
    let mut m: HashMap<String, Vec<usize>> = HashMap::new();
    if let Some(d) = items.datacore() {
        for i in 0..d.n_rec {
            if let Ok(r) = d.record(i) {
                m.entry(r.struct_name).or_default().push(i);
            }
        }
    }
    m
}

/// Reads the crafting records out of the DataCore with `guid_record` to resolve references.
pub struct Reader<'a> {
    pub items: &'a ItemMaterials,
    pub stats: &'a ItemStats<'a>,
    pub by_struct: HashMap<String, Vec<usize>>,
}

fn short_of(name: Option<String>) -> Option<String> {
    name.map(|n| n.split_once('.').map_or(n.clone(), |x| x.1.to_string()))
}

impl<'a> Reader<'a> {
    pub fn new(items: &'a ItemMaterials, stats: &'a ItemStats<'a>) -> Self {
        Reader { items, stats, by_struct: records_by_struct(items) }
    }

    fn d(&self) -> &p4k_formats::dcb::Dcb {
        self.items.datacore().expect("DataCore")
    }

    pub fn rname(&self, r: &Value) -> Option<String> {
        self.stats.rname(r)
    }

    pub fn short(&self, r: &Value) -> Option<String> {
        short_of(self.rname(r))
    }

    fn jshort(&self, r: &Value) -> Value {
        self.short(r).map_or(Value::Null, |s_| json!(s_))
    }

    /// `{type, subtype, size, grade}` of an entity class (AttachDef), or None when the class has no record.
    pub fn entity_info(&self, cls: &str) -> Option<Value> {
        let data = self.stats.record(cls, 6)?;
        let att = g(comp(&data, "SAttachableComponentParams"), "AttachDef");
        Some(json!({"type": if truthy(g(att, "Type")) { g(att, "Type").clone() } else { json!("") }, "subtype": if truthy(g(att, "SubType")) { g(att, "SubType").clone() } else { json!("") },
                    "size": g(att, "Size"), "grade": g(att, "Grade")}))
    }

    fn mods(&self, context: &Value) -> Vec<Value> {
        let mut out = vec![];
        for c in items(context) {
            if struct_of(c) != "CraftingCostContext_ResultGameplayPropertyModifiers" {
                continue;
            }
            for m in items(g(g(c, "gameplayPropertyModifiers"), "gameplayPropertyModifiers")) {
                let gpp = self.short(g(m, "gameplayPropertyRecord"));
                let mut ranges = vec![];
                for r in items(g(m, "valueRanges")) {
                    let add = r.get("additiveModifierAtStart").is_some();
                    let dv = |k: &str, d: Value| r.get(k).cloned().unwrap_or(d);
                    ranges.push(json!([
                        dv("startQuality", json!(0)),
                        dv("endQuality", json!(1000)),
                        dv(if add { "additiveModifierAtStart" } else { "modifierAtStart" }, json!(1.0)),
                        dv(if add { "additiveModifierAtEnd" } else { "modifierAtEnd" }, json!(1.0)),
                        if add { 1 } else { 0 }
                    ]));
                }
                if let Some(gpp) = gpp {
                    out.push(json!({"gpp": gpp.rsplit('.').next().unwrap(), "ranges": ranges}));
                }
            }
        }
        out
    }

    fn leaf(&self, o: &Value) -> Value {
        let mult = items(g(o, "context")).find(|c| struct_of(c) == "CraftingCostContext_QuantityMultiplier").map(|c| g(c, "multiplier").clone()).unwrap_or(Value::Null);
        let minq = if o.get("minQuality").is_some() { g(o, "minQuality").clone() } else { json!(0) };
        if o.get("resource").is_some() {
            return json!({"kind": "resource", "ref": self.jshort(g(o, "resource")), "qty": g(g(o, "quantity"), "standardCargoUnits"), "min_q": minq, "mult": mult});
        }
        if o.get("entityClass").is_some() {
            let r = self.short(g(o, "entityClass")).unwrap_or_default().replace(RECORD, "");
            return json!({"kind": "item", "ref": r, "qty": g(o, "quantity"), "min_q": minq, "mult": mult});
        }
        json!({"kind": if o.get("__struct__").is_some() { g(o, "__struct__").clone() } else { json!("?") }, "ref": null, "qty": null, "min_q": 0, "mult": mult})
    }

    fn leaves(&self, node: &Value) -> Vec<Value> {
        if struct_of(node) == "CraftingCost_Select" {
            arr(g(node, "options")).iter().flat_map(|o| self.leaves(o)).collect()
        } else {
            vec![self.leaf(node)]
        }
    }

    /// Slots of a `mandatoryCost` node: `[{name, label, count, options: [leaf], mods}]`.  Each option of the root select is a slot; a leaf
    /// directly under the root forms one slot of its own (the shape never occurs in the live data).
    pub fn slots(&self, cost: &Value) -> Vec<Value> {
        if !truthy(cost) {
            return vec![];
        }
        let wrapped;
        let cost = if struct_of(cost) != "CraftingCost_Select" {
            wrapped = json!({"__struct__": "CraftingCost_Select", "options": [cost], "count": 1});
            &wrapped
        } else {
            cost
        };
        let mut out = vec![];
        for o in arr(g(cost, "options")) {
            let ni = g(o, "nameInfo");
            let sel = struct_of(o) == "CraftingCost_Select";
            out.push(json!({
                "name": if truthy(g(ni, "debugName")) { g(ni, "debugName").clone() } else { json!("") },
                "label": if truthy(g(ni, "displayName")) { g(ni, "displayName").clone() } else { json!("") },
                "count": if sel { if o.get("count").is_some() { g(o, "count").clone() } else { json!(1) } } else { json!(1) },
                "options": self.leaves(o), "mods": self.mods(g(o, "context"))
            }));
        }
        out
    }

    /// Compact dict of blueprint record `i` (a CraftingBlueprintRecord), or None for a record that is not an item recipe.
    pub fn blueprint(&self, i: usize) -> Option<Value> {
        let d = self.d();
        let rec = d.record(i).ok()?;
        let data = d.record_data(&rec, 30).ok()?.to_json();
        let bp = g(&data, "blueprint");
        let ps = g(bp, "processSpecificData");
        if struct_of(ps) != "CraftingProcess_Creation" {
            return None;
        }
        let out_cls = self.short(g(ps, "entityClass")).unwrap_or_default();
        let cat = self.short(g(bp, "category")).unwrap_or_default();
        let tiers = arr(g(bp, "tiers"));
        let t = tiers.first().unwrap_or(&Value::Null);
        let costs = g(g(t, "recipe"), "costs");
        let research = g(t, "research");
        let info = if !out_cls.is_empty() { self.entity_info(&out_cls) } else { None };
        let ig = |k: &str| info.as_ref().map_or(Value::Null, |x| g(x, k).clone());
        let sstr = |k: &str| info.as_ref().map_or(json!(""), |x| g(x, k).clone());
        Some(json!({
            "id": rec.name.split_once('.').map_or(rec.name.clone(), |x| x.1.to_string()), "idx": i, "cat": cat, "out": out_cls, "found": info.is_some(),
            "type": sstr("type"), "subtype": sstr("subtype"), "size": ig("size"), "grade": ig("grade"),
            "kind": kind_of(info.as_ref().map_or("", |x| s(g(x, "type"))), &cat),
            "time": time_seconds(g(costs, "craftTime")), "slots": self.slots(g(costs, "mandatoryCost")),
            "optional": arr(g(costs, "optionalCosts")).len(), "tiers": tiers.len(),
            "research": truthy(g(research, "unlockRequirements")) || truthy(g(research, "researchCosts")),
            "results": arr(g(g(g(t, "recipe"), "results"), "results")).len()
        }))
    }

    pub fn records(&self, st: &str) -> &[usize] {
        self.by_struct.get(st).map_or(&[], Vec::as_slice)
    }

    fn short_name(&self, i: usize) -> String {
        let n = self.d().record_name(i).map(|c| c.into_owned()).unwrap_or_default();
        n.split_once('.').map_or(n.clone(), |x| x.1.to_string())
    }

    /// `{property: {name key, unit key, transform: [struct names in order], scale}}` of every CraftingGameplayPropertyDef.
    pub fn gpp_defs(&self) -> Map<String, Value> {
        let d = self.d();
        let mut out = Map::new();
        for &i in self.records("CraftingGameplayPropertyDef") {
            let Ok(rec) = d.record(i) else { continue };
            let Ok(r) = d.record_data(&rec, 4) else { continue };
            let r = r.to_json();
            let tr = g(&r, "displayTransformation");
            let steps: Vec<&Value> = if tr.is_object() {
                let t = g(tr, "transformations");
                if truthy(t) { arr(t).iter().filter(|x| x.is_object()).collect() } else { vec![tr] }
            } else {
                vec![]
            };
            let scale = steps.iter().find(|x| x.get("scale").is_some()).map(|x| g(x, "scale").clone()).unwrap_or(Value::Null);
            out.insert(
                self.short_name(i),
                json!({"name": g(&r, "propertyName"), "unit": g(&r, "unitFormat"), "transform": steps.iter().map(|x| g(x, "__struct__").clone()).collect::<Vec<_>>(), "scale": scale}),
            );
        }
        out
    }

    /// `{label, mean, stddev, min, max, dist}` of ResourceType `name`.
    pub fn resource(&self, name: &str) -> Option<Value> {
        let d = self.d();
        let ids = d.record_index().get(&format!("ResourceType.{}", name))?;
        let r = d.record_data(&d.record(*ids.first()?).ok()?, 8).ok()?.to_json();
        let mut out = json!({"label": g(&r, "displayName"), "mean": null, "stddev": null, "min": null, "max": null, "dist": null});
        if let Some(p) = arr(g(&r, "properties")).first() {
            let rec_ref = g(g(p, "qualityDistribution"), "qualityDistributionRecord");
            if is_ref3(rec_ref) {
                if let Some(i) = rec_ref[2].as_str().and_then(|gd| self.items.guid_record(gd)) {
                    if let Ok(rc) = d.record(i) {
                        if let Ok(rr) = d.record_data(&rc, 4) {
                            let nd = g(&rr.to_json(), "qualityDistribution").clone();
                            out["mean"] = g(&nd, "mean").clone();
                            out["stddev"] = g(&nd, "stddev").clone();
                            out["min"] = g(&nd, "min").clone();
                            out["max"] = g(&nd, "max").clone();
                            out["dist"] = json!(self.short_name(i));
                        }
                    }
                }
            }
        }
        Some(out)
    }

    pub fn pool(&self, i: usize) -> Vec<Value> {
        let d = self.d();
        let Ok(rec) = d.record(i) else { return vec![] };
        let Ok(r) = d.record_data(&rec, 4) else { return vec![] };
        let r = r.to_json();
        arr(g(&r, "blueprintRewards"))
            .iter()
            .map(|x| json!({"bp": self.jshort(g(x, "blueprintRecord")), "weight": if x.get("weight").is_some() { g(x, "weight").clone() } else { json!(1.0) }}))
            .collect()
    }

    /// `{pool name: [source]}` for the records `srcs` {pool record index: [referencing record indexes]}; a source is a contract generator
    /// (debugName, chance) or a reputation tier (faction, minPoints), found by walking the referencing record for `blueprintPool` fields.
    pub fn pool_sources(&self, srcs: &[(usize, Vec<usize>)]) -> Map<String, Value> {
        let d = self.d();
        let mut out: Vec<(String, Vec<Value>)> = vec![];
        for (pool_i, rec_ids) in srcs {
            let pname = self.short_name(*pool_i);
            let pos = match out.iter().position(|(k, _)| *k == pname) {
                Some(p) => p,
                None => {
                    out.push((pname.clone(), vec![]));
                    out.len() - 1
                }
            };
            for &ri in rec_ids {
                let Ok(rec) = d.record(ri) else { continue };
                let data = match d.record_data(&rec, 40) {
                    Ok(x) => x,
                    Err(e) => {
                        eprintln!("DBG pool_sources {}: {:?}", rec.name, e);
                        continue;
                    }
                };
                let mut found = vec![];
                self.walk(&data.to_json(), &Map::new(), &mut found);
                for (ctx, pools) in found {
                    if pools.contains(&pname) {
                        let mut m = ctx.clone();
                        m.insert("record".into(), json!(rec.name));
                        m.insert("rtype".into(), json!(rec.struct_name));
                        out[pos].1.push(Value::Object(m));
                    }
                }
            }
        }
        let mut res = Map::new();
        for (k, v) in out {
            let mut seen = HashSet::new();
            let mut uniq = vec![];
            for sv in v {
                let mut key: Vec<(String, String)> = sv.as_object().unwrap().iter().map(|(a, b)| (a.clone(), pystr(b))).collect();
                key.sort();
                if seen.insert(key) {
                    uniq.push(sv);
                }
            }
            res.insert(k, Value::Array(uniq));
        }
        res
    }

    fn walk(&self, v: &Value, ctx: &Map<String, Value>, found: &mut Vec<(Map<String, Value>, Vec<String>)>) {
        match v {
            Value::Object(o) => {
                let mut c = ctx.clone();
                for key in ["debugName", "minPoints", "chance"] {
                    if let Some(x) = o.get(key) {
                        if matches!(x, Value::String(_) | Value::Number(_)) {
                            c.insert(key.into(), x.clone());
                        }
                    }
                }
                if let Some(fa) = o.get("faction") {
                    if fa.is_array() {
                        c.insert("faction".into(), self.jshort(fa));
                    }
                }
                if let Some(bp) = o.get("blueprintPool") {
                    // a reference is itself a ["ref", n, guid] array, which is a single value and not a list of pools
                    let single = matches!(bp, Value::Array(a) if a.len() == 3 && a[0] == "ref");
                    let lst: Vec<&Value> = if bp.is_array() && !single { arr(bp).iter().collect() } else { vec![bp] };
                    found.push((c.clone(), lst.iter().filter_map(|x| self.short(x)).collect()));
                }
                for x in o.values() {
                    if x.is_object() || x.is_array() {
                        self.walk(x, &c, found);
                    }
                }
            }
            Value::Array(a) => {
                for x in a {
                    if x.is_object() || x.is_array() {
                        self.walk(x, ctx, found);
                    }
                }
            }
            _ => {}
        }
    }

    pub fn legacy(&self) -> Vec<Value> {
        let d = self.d();
        let mut out = vec![];
        for &i in self.records("LegacyCraftingRecipeDefRecord") {
            let Ok(rec) = d.record(i) else { continue };
            let Ok(data) = d.record_data(&rec, 8) else { continue };
            let data = data.to_json();
            let r = g(&data, "recipe");
            let mut outs = vec![];
            for o in arr(g(r, "outputs")) {
                if o.get("entityClass").is_some() {
                    outs.push(json!({"kind": "item", "ref": self.short(g(o, "entityClass")).unwrap_or_default(), "qty": g(o, "amount")}));
                } else if o.get("resource").is_some() {
                    outs.push(json!({"kind": "resource", "ref": self.jshort(g(o, "resource")), "qty": g(g(o, "amount"), "standardCargoUnits")}));
                }
            }
            let costs: Vec<Value> = arr(g(r, "costs"))
                .iter()
                .map(|c| json!({"kind": "resource", "ref": self.jshort(g(c, "resource")), "qty": g(g(c, "amount"), "standardCargoUnits"), "derived": struct_of(c).ends_with("DerivedFromAmmoContainer")}))
                .collect();
            if !outs.is_empty() {
                out.push(json!({"id": self.short_name(i), "time": if truthy(g(r, "timeSeconds")) { g(r, "timeSeconds").clone() } else { json!(0.0) }, "name_key": g(r, "recipeName"), "outputs": outs, "costs": costs}));
            }
        }
        out
    }
}

/// The whole crafting data set (see `Crafting`).  `back` = the back-reference index (`dcbnav::scan_references`), when available.
pub fn build(items: &ItemMaterials, stats: &ItemStats, nav: Option<&DcbNav>, back: Option<&HashMap<usize, Vec<u32>>>) -> Value {
    let rd = Reader::new(items, stats);
    let d = rd.d();
    let bps: Vec<Value> = rd.records("CraftingBlueprintRecord").iter().filter_map(|&i| rd.blueprint(i)).collect();
    let gpp = rd.gpp_defs();
    let mut resources = Map::new();
    for b in &bps {
        for sl in arr(&b["slots"]) {
            for o in arr(&sl["options"]) {
                if s(&o["kind"]) == "resource" && truthy(&o["ref"]) && !resources.contains_key(s(&o["ref"])) {
                    resources.insert(s(&o["ref"]).to_string(), rd.resource(s(&o["ref"])).unwrap_or(Value::Null));
                }
            }
        }
    }
    let mut pools = Map::new();
    let pool_ids = rd.records("BlueprintPoolRecord").to_vec();
    for &i in &pool_ids {
        pools.insert(rd.short_name(i), Value::Array(rd.pool(i)));
    }
    let sources = match (nav, back) {
        (Some(_), Some(back)) => {
            let srcs: Vec<(usize, Vec<usize>)> = pool_ids.iter().map(|&i| (i, back.get(&i).map(|v| v.iter().map(|&x| x as usize).collect()).unwrap_or_default())).collect();
            Value::Object(rd.pool_sources(&srcs))
        }
        _ => json!({}),
    };
    let mut start: Vec<Value> = vec![];
    for &i in rd.records("CraftingGlobalParams") {
        if let Ok(rec) = d.record(i) {
            if let Ok(r) = d.record_data(&rec, 6) {
                let r = r.to_json();
                let sel = g(&r, "defaultBlueprintSelection");
                start = arr(g(sel, "blueprintRecords")).iter().map(|x| rd.jshort(x)).collect();
            }
        }
    }
    json!({"v": 1, "blueprints": bps, "gpp": gpp, "resources": resources, "pools": pools, "sources": sources, "start": start, "legacy": rd.legacy()})
}

/// Re-spaced resource name: `re.sub(r"(?<=[a-z])(?=[A-Z])", " ", ref.replace("_", " "))`.
fn space_camel(t: &str) -> String {
    let t = t.replace('_', " ");
    let mut out = String::new();
    let mut prev: Option<char> = None;
    for c in t.chars() {
        if c.is_ascii_uppercase() && prev.map_or(false, |p| p.is_ascii_lowercase()) {
            out.push(' ');
        }
        out.push(c);
        prev = Some(c);
    }
    out
}

/// The crafting index, built once from the DataCore with `build` (no cache file).
/// Reusable API for the item pages: `blueprints_for(cls)`, `blueprint_for(cls)`, `detail(bp)`, `targets(bp, base, ...)`; quality math:
/// the module functions `modifier_at` / `simulate` / `apply_effects` / `display_value`.
pub struct Crafting<'a> {
    pub stats: &'a ItemStats<'a>,
    pub data: Value,
    by_out: HashMap<String, Vec<usize>>,
    by_id: HashMap<String, usize>,
    granted: HashMap<String, Vec<Value>>,
    start: HashSet<String>,
}

impl<'a> Crafting<'a> {
    /// Install a data set (also used by tests with synthetic data) and build the lookups.
    pub fn new(stats: &'a ItemStats<'a>, mut data: Value) -> Self {
        let mut by_out: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_id = HashMap::new();
        if let Some(bps) = data["blueprints"].as_array_mut() {
            for (i, b) in bps.iter_mut().enumerate() {
                if b.get("default_quality").is_none() {
                    b["default_quality"] = json!(500);
                }
                by_id.insert(s(&b["id"]).to_string(), i);
                by_out.entry(s(&b["out"]).to_lowercase()).or_default().push(i);
            }
        }
        let mut granted: HashMap<String, Vec<Value>> = HashMap::new();
        for (pool, rows) in data["pools"].as_object().cloned().unwrap_or_default() {
            let total: f64 = arr(&rows).iter().map(|r| f(&r["weight"])).sum();
            let total = if total != 0.0 { total } else { 1.0 };
            for r in arr(&rows) {
                if let Some(bp) = r["bp"].as_str() {
                    granted.entry(bp.to_string()).or_default().push(json!({"pool": pool, "weight": r["weight"], "chance": f(&r["weight"]) / total, "size": arr(&rows).len()}));
                }
            }
        }
        let start = arr(&data["start"]).iter().filter_map(|x| x.as_str().map(str::to_owned)).collect();
        Crafting { stats, data, by_out, by_id, granted, start }
    }

    fn bp(&self, i: usize) -> &Value {
        &self.data["blueprints"][i]
    }

    /// Blueprints that make entity class `cls` (usually one).
    pub fn blueprints_for(&self, cls: &str) -> Vec<&Value> {
        self.by_out.get(&cls.to_lowercase()).map_or(vec![], |v| v.iter().map(|&i| self.bp(i)).collect())
    }

    pub fn blueprint_for(&self, cls: &str) -> Option<&Value> {
        self.blueprints_for(cls).into_iter().next()
    }

    /// Blueprints of the variants (skins) of `cls`: output classes named `<cls>_<suffix>`.
    pub fn variant_blueprints(&self, cls: &str) -> Vec<&Value> {
        let pre = format!("{}_", cls.to_lowercase());
        let mut keys: Vec<&String> = self.by_out.keys().filter(|k| k.starts_with(&pre)).collect();
        // First-insertion order = the order of each key's first blueprint index.
        keys.sort_by_key(|k| self.by_out[*k].first().copied().unwrap_or(usize::MAX));
        keys.into_iter().flat_map(|k| self.by_out[k].iter().map(|&i| self.bp(i))).collect()
    }

    pub fn get(&self, bp_id: &str) -> Option<&Value> {
        self.by_id.get(bp_id).map(|&i| self.bp(i))
    }

    pub fn has_blueprint(&self, cls: &str) -> bool {
        self.by_out.get(&cls.to_lowercase()).map_or(false, |v| !v.is_empty())
    }

    pub fn is_granted(&self, bp_id: &str) -> bool {
        self.granted.contains_key(bp_id)
    }

    // ---- presentation
    fn loc(&self, key: &Value) -> Option<String> {
        if truthy(key) {
            self.stats.localise(key)
        } else {
            None
        }
    }

    pub fn resource_name(&self, r: &str) -> String {
        let res = &self.data["resources"][r];
        self.loc(g(res, "label")).unwrap_or_else(|| space_camel(r))
    }

    pub fn item_name(&self, cls: &str) -> String {
        if cls.is_empty() {
            String::new()
        } else {
            s(&self.stats.identity(cls)["name"]).to_string()
        }
    }

    pub fn gpp_label(&self, gpp: &str) -> String {
        let gv = &self.data["gpp"][gpp];
        self.loc(g(gv, "name")).unwrap_or_else(|| gpp.replace("GPP_", "").replace('_', " "))
    }

    /// How a blueprint is obtained: `{pools: [{pool, weight, chance, size, sources: [{rtype, record, label, chance?, minPoints?, faction?}]}],
    /// starting}`.
    pub fn acquisition(&self, bp_id: &str) -> Value {
        let mut out = vec![];
        for gr in self.granted.get(bp_id).into_iter().flatten() {
            let mut srcs = vec![];
            for sc in arr(&self.data["sources"][s(&gr["pool"])]) {
                let rec = s(g(sc, "record"));
                let label = if truthy(g(sc, "debugName")) { g(sc, "debugName").clone() } else { json!(rec.split_once('.').map_or(rec, |x| x.1)) };
                srcs.push(json!({"rtype": g(sc, "rtype"), "record": g(sc, "record"), "label": label, "chance": g(sc, "chance"), "minPoints": g(sc, "minPoints"), "faction": g(sc, "faction")}));
            }
            let mut m = gr.as_object().unwrap().clone();
            m.insert("sources".into(), Value::Array(srcs));
            out.push(Value::Object(m));
        }
        json!({"pools": out, "starting": self.start.contains(bp_id)})
    }

    /// JSON-able description of one blueprint with localised names (for /crafting/ pages).
    pub fn detail(&self, bp: &Value) -> Value {
        let mut slots = vec![];
        let mut gpps = Map::new();
        for sl in arr(&bp["slots"]) {
            let mut opts = vec![];
            for o in arr(&sl["options"]) {
                let mut x = o.clone();
                if s(&o["kind"]) == "resource" {
                    x["name"] = json!(self.resource_name(s(&o["ref"])));
                    let res = &self.data["resources"][s(&o["ref"])];
                    x["quality"] = if g(res, "mean").is_null() {
                        Value::Null
                    } else {
                        json!({"mean": res["mean"], "stddev": res["stddev"], "min": res["min"], "max": res["max"]})
                    };
                } else if s(&o["kind"]) == "item" {
                    x["name"] = json!(self.item_name(s(&o["ref"])));
                }
                opts.push(x);
            }
            let mut mods = vec![];
            for m in arr(&sl["mods"]) {
                let gp = s(&m["gpp"]);
                let gd = &self.data["gpp"][gp];
                let label = self.gpp_label(gp);
                gpps.insert(gp.to_string(), json!({"label": label, "transform": if truthy(g(gd, "transform")) { g(gd, "transform").clone() } else { json!([]) }, "scale": g(gd, "scale"), "rule": rule_of(gp)}));
                let mut mm = m.as_object().unwrap().clone();
                mm.insert("label".into(), json!(label));
                mods.push(Value::Object(mm));
            }
            let lab = self.loc(&sl["label"]).unwrap_or_else(|| {
                // str.title()
                let n = s(&sl["name"]);
                let mut out = String::new();
                let mut up = true;
                for c in n.chars() {
                    if c.is_alphabetic() {
                        if up { out.extend(c.to_uppercase()) } else { out.extend(c.to_lowercase()) }
                        up = false;
                    } else {
                        out.push(c);
                        up = true;
                    }
                }
                out
            });
            slots.push(json!({"name": sl["name"], "label": lab.trim_end_matches(|c| c == ':' || c == ' '), "count": sl["count"], "options": opts, "mods": mods}));
        }
        let nm = self.item_name(s(&bp["out"]));
        let name = if !nm.is_empty() {
            nm
        } else if !s(&bp["out"]).is_empty() {
            s(&bp["out"]).to_string()
        } else {
            s(&bp["id"]).to_string()
        };
        let kind_label = KINDS.iter().find(|(k, _)| *k == s(&bp["kind"])).map(|x| x.1);
        json!({"id": bp["id"], "out": bp["out"], "name": name, "kind": bp["kind"], "kind_label": kind_label,
               "category": bp["cat"], "category_label": pretty_category(s(&bp["cat"])), "type": bp["type"], "subtype": bp["subtype"], "size": bp["size"],
               "grade": bp["grade"], "found": bp["found"], "time": bp["time"], "time_text": time_text(f(&bp["time"])), "slots": slots,
               "gpps": gpps, "default_quality": bp["default_quality"], "acquire": self.acquisition(s(&bp["id"])), "idx": bp["idx"]})
    }

    /// The stats of the item that the blueprint's properties change: `[{id, label, unit, mode, gpps, base}]` for every stat-table entry
    /// whose properties the blueprint uses and whose base value is known: weapon metrics (`base_metrics`) or item card rows (`rows`,
    /// matched by label prefix).  INFERRED mapping (`stat_defs`).
    pub fn targets(bp: &Value, base_metrics: Option<&Map<String, Value>>, rows: &[Value], defs: Option<&[StatDef]>) -> Vec<Value> {
        let used: HashSet<&str> = arr(&bp["slots"]).iter().flat_map(|sl| arr(&sl["mods"]).iter().map(|m| s(&m["gpp"]))).collect();
        let owned;
        let defs = match defs {
            Some(d) => d,
            None => {
                owned = stat_defs();
                &owned
            }
        };
        let mut out = vec![];
        for d in defs {
            if !d.gpps.iter().any(|g_| used.contains(g_)) {
                continue;
            }
            let mut base = Value::Null;
            let mut unit = d.unit.clone();
            if let Some(k) = d.id.strip_prefix("m:") {
                base = base_metrics.and_then(|m| m.get(k)).cloned().unwrap_or(Value::Null);
            } else {
                let pre = &d.id[2..];
                for r in rows {
                    if s(&r["label"]).starts_with(pre) && matches!(r["value"], Value::Number(_) | Value::Bool(_)) {
                        base = r["value"].clone();
                        if truthy(g(r, "unit")) {
                            unit = s(g(r, "unit")).to_string();
                        }
                        break;
                    }
                }
            }
            if matches!(base, Value::Number(_) | Value::Bool(_)) {
                out.push(json!({"id": d.id, "label": d.label, "unit": unit, "mode": d.mode, "gpps": d.gpps.iter().filter(|g_| used.contains(**g_)).collect::<Vec<_>>(), "base": base}));
            }
        }
        out
    }

    /// One row per blueprint for the /crafting/ index.
    pub fn index_rows(&self) -> Vec<Value> {
        let mut rows = vec![];
        for b in arr(&self.data["blueprints"]) {
            let nm = if truthy(&b["found"]) { self.item_name(s(&b["out"])) } else { s(&b["out"]).to_string() };
            let name = if nm.is_empty() { s(&b["id"]).to_string() } else { nm };
            let mut res: Vec<String> = vec![];
            for sl in arr(&b["slots"]) {
                for o in arr(&sl["options"]) {
                    if s(&o["kind"]) == "resource" && truthy(&o["ref"]) {
                        res.push(self.resource_name(s(&o["ref"])));
                    }
                }
            }
            res.sort();
            res.dedup();
            rows.push(json!({"id": b["id"], "out": b["out"], "name": name, "kind": b["kind"], "cat": pretty_category(s(&b["cat"])), "type": b["type"], "size": b["size"],
                             "grade": b["grade"], "time": b["time"], "res": res, "starting": self.start.contains(s(&b["id"])), "granted": self.granted.contains_key(s(&b["id"]))}));
        }
        rows
    }
}

#[allow(dead_code)]
fn _unused(_: BTreeMap<u8, u8>) {}
