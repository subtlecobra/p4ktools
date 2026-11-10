//! Characteristics of a weapon with a given set of attachments: damage per fire mode, rate, ammunition, spread, recoil, zoom, what each
//! attachment changes, and the non-constant damage models (heat ramp, charge curve).
//!
//! Sources (EMPIRICAL on the live archive unless noted):
//!
//! * The weapon: `SCItemWeaponComponentParams.fireActions` (`fireRate` rounds per minute, `launchParams` pelletCount / ammoCost /
//!   damageMultiplier / spreadParams, `heatPerShot`, a `recoil` record whose hand kick is `weaponProceduralHandsRecoil.curveRecoil.
//!   positionCurves.yMaxValue`).  The mass is `SEntityPhysicsControllerParams.PhysType.Mass`.
//! * The ammunition of a first-person weapon is on the magazine (`SAmmoContainerComponentParams`: `maxAmmoCount`, `ammoParamsRecord` ->
//!   speed, lifetime, `projectileParams.damage`, `damageDropParams`, `penetrationParams`).  Ship guns carry the container themselves.
//! * An attachment's effect is `SWeaponModifierComponentParams.modifier.weaponStats`: multipliers (neutral 1.0) and additive fields
//!   (neutral 0).
//! * INFERRED: the modifiers of several attachments multiply; `damageMultiplier` scales every damage type of a hit;
//!   `projectileSpeedMultiplier` scales speed and so range; the recoil figure is the hand kick times the attachments' modifier.
//! * Heat model and charge model (both CONFIRMED against the engine): see the docs of `heat_curve` and `charge_curve`.  Per-axis recoil
//!   factors: CONFIRMED against the engine.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::{json, Map, Value};

use crate::dynv::*;
use crate::itemmaterials::ItemMaterials;
use crate::itemstats::{comp, damage, dsum, ItemStats, DAMAGE_TYPES};

pub const NEUTRAL_ADDITIVE: [&str; 6] = ["additiveModifier", "fireRate", "pellets", "burstShots", "ammoCost", "uiReticleIndex"];
/// (path below `modifier.weaponStats.`, label, kind)
pub const MOD_LABELS: [(&str, &str, &str); 21] = [
    ("damageMultiplier", "Damage", "x"),
    ("fireRateMultiplier", "Fire rate", "x"),
    ("projectileSpeedMultiplier", "Projectile speed", "x"),
    ("ammoCostMultiplier", "Ammo cost", "x"),
    ("heatGenerationMultiplier", "Heat generation", "x"),
    ("chargeTimeMultiplier", "Charge time", "x"),
    ("soundRadiusMultiplier", "Sound radius", "x"),
    ("spreadModifier.minMultiplier", "Spread (min)", "x"),
    ("spreadModifier.maxMultiplier", "Spread (max)", "x"),
    ("spreadModifier.firstAttackMultiplier", "Spread (first shot)", "x"),
    ("spreadModifier.attackMultiplier", "Spread (growth per shot)", "x"),
    ("spreadModifier.decayMultiplier", "Spread (recovery)", "x"),
    ("spreadModifier.additiveModifier", "Spread (added)", "+"),
    ("recoilModifier.aimRecoilModifier.curveRecoil.pitchMaxDegreesModifier", "Aim recoil (pitch)", "x"),
    ("recoilModifier.aimRecoilModifier.curveRecoil.yawMaxDegreesModifier", "Aim recoil (yaw)", "x"),
    ("recoilModifier.curveRecoil.positionModifiers.yMaxValueModifier", "Hand kick", "x"),
    ("recoilModifier.decayMultiplier", "Recoil recovery", "x"),
    ("aimModifier.zoomScale", "Zoom", "x"),
    ("aimModifier.zoomTimeScale", "Zoom time", "x"),
    ("regenModifier.maxAmmoLoadMultiplier", "Ammo load", "x"),
    ("regenModifier.powerRatioMultiplier", "Power ratio", "x"),
];
pub const MOD_STATS: [&str; 9] = [
    "damageMultiplier",
    "fireRateMultiplier",
    "fireRate",
    "pellets",
    "projectileSpeedMultiplier",
    "ammoCostMultiplier",
    "ammoCost",
    "heatGenerationMultiplier",
    "chargeTimeMultiplier",
];
const AIM_PITCH: &str = "recoilModifier.aimRecoilModifier.curveRecoil.pitchMaxDegreesModifier";

/// `{dotted path: number}` of the numeric leaves of a nested dict (booleans and strings skipped), in document order.
pub fn flatten(v: &Value) -> Vec<(String, f64)> {
    fn go(v: &Value, pre: &str, out: &mut Vec<(String, f64)>) {
        match v {
            Value::Object(o) => {
                for (k, x) in o {
                    if !k.starts_with("__") {
                        go(x, &format!("{}{}.", pre, k), out);
                    }
                }
            }
            Value::Number(n) => {
                let path = pre[..pre.len().saturating_sub(1)].to_string();
                let x = n.as_f64().unwrap_or(0.0);
                match out.iter_mut().find(|(k, _)| *k == path) {
                    Some(slot) => slot.1 = x,
                    None => out.push((path, x)),
                }
            }
            _ => {}
        }
    }
    let mut out = vec![];
    go(v, "", &mut out);
    out
}

fn lookup(flat: &[(String, f64)], k: &str) -> Option<f64> {
    flat.iter().find(|(p, _)| p == k).map(|(_, v)| *v)
}

/// True when `value` is the do-nothing value of the modifier leaf `path`.
pub fn is_neutral(path: &str, value: f64) -> bool {
    let leaf = path.rsplit('.').next().unwrap_or(path);
    if NEUTRAL_ADDITIVE.contains(&leaf) {
        value.abs() < 1e-9
    } else {
        (value - 1.0).abs() < 1e-6
    }
}

/// `{types, dominant, values}` of a per-shot damage dict: types with non-zero damage, largest first (ties in DAMAGE_TYPES order).
pub fn classify_damage(dmg: &[(String, f64)]) -> Value {
    let mut vals: Vec<(&'static str, f64)> = vec![];
    for (t, v) in dmg {
        if *v != 0.0 {
            if let Some(tt) = DAMAGE_TYPES.iter().find(|x| *x == t) {
                match vals.iter_mut().find(|(k, _)| k == tt) {
                    Some(slot) => slot.1 = *v,
                    None => vals.push((tt, *v)),
                }
            }
        }
    }
    let idx = |t: &str| DAMAGE_TYPES.iter().position(|x| *x == t).unwrap();
    let mut types: Vec<(&str, f64)> = vals.clone();
    types.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(idx(a.0).cmp(&idx(b.0))));
    let mut values = Map::new();
    for (t, v) in &types {
        values.insert((*t).into(), json!(pyround(*v, 3)));
    }
    json!({"types": types.iter().map(|(t, _)| *t).collect::<Vec<_>>(), "dominant": types.first().map_or("unknown", |t| t.0), "values": values})
}

pub fn dmg_vec(d: &[(&str, f64)]) -> Vec<(String, f64)> {
    d.iter().map(|(t, v)| (t.to_string(), *v)).collect()
}

/// 'recoilModifier.curveRecoil.positionModifiers.xMaxValueModifier' -> 'recoil: curve recoil: position modifiers: x max value'.
pub fn humanize(path: &str) -> String {
    let mut parts = vec![];
    for seg in path.split('.') {
        let seg = seg.strip_suffix("Multiplier").or_else(|| seg.strip_suffix("Modifier")).unwrap_or(seg);
        let mut out = String::new();
        let mut prev: Option<char> = None;
        for c in seg.chars() {
            if c.is_ascii_uppercase() && prev.map_or(false, |p| p.is_ascii_lowercase() || p.is_ascii_digit()) {
                out.push(' ');
            }
            out.push(c);
            prev = Some(c);
        }
        let low = out.to_lowercase();
        if !low.is_empty() {
            parts.push(low);
        }
    }
    parts.join(": ")
}

fn capitalize(s: &str) -> String {
    let mut it = s.chars();
    match it.next() {
        Some(c) => c.to_uppercase().collect::<String>() + &it.as_str().to_lowercase(),
        None => String::new(),
    }
}

fn comparer_mode(mode: &Value) -> Option<i64> {
    match mode {
        Value::String(n) => ["Equal", "Greater", "Less", "GreaterOrEqual", "LessOrEqual"].iter().position(|x| x == n).map(|i| i as i64),
        Value::Number(n) => {
            let x = n.as_f64()?;
            if x.fract() == 0.0 && (0.0..5.0).contains(&x) {
                Some(x as i64)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn met(mode: &Value, level: f64, value: f64) -> bool {
    match comparer_mode(mode) {
        Some(0) => level == value,
        Some(1) => level > value,
        Some(2) => level < value,
        Some(3) => level >= value,
        Some(4) => level <= value,
        _ => false,
    }
}

/// Ship guns lack the carryable component of personal weapons (EMPIRICAL); the engine multiplies a ship weapon's shot heat by
/// `heatGenerationMultiplier` but not a personal weapon's (CONFIRMED).
fn is_ship_weapon(data: &Value) -> bool {
    !arr(g(data, "Components")).iter().any(|c| c.is_object() && struct_of(c) == "SEntityComponentCarryableParams")
}

type Stage = ([Option<f64>; 4], String);

pub struct WeaponStats<'a> {
    pub items: &'a ItemMaterials,
    pub stats: &'a ItemStats<'a>,
    mods: Mutex<HashMap<String, Vec<(String, f64)>>>,
}

/// (AmmoContainer component, AmmoParams, class that provides it)
pub type AmmoSource = (Value, Option<Value>, Option<String>);

impl<'a> WeaponStats<'a> {
    pub fn new(items: &'a ItemMaterials, stats: &'a ItemStats<'a>) -> Self {
        WeaponStats { items, stats, mods: Mutex::new(HashMap::new()) }
    }

    // ---- attachments
    /// `{path below modifier.weaponStats: value}` of the attachment `cls`, `[]` without a modifier component.  Cached.
    pub fn modifier(&self, cls: &str) -> Vec<(String, f64)> {
        if let Some(v) = self.mods.lock().unwrap().get(cls) {
            return v.clone();
        }
        let data = if cls.is_empty() { None } else { self.stats.record(cls, 10) };
        let flat = match data {
            Some(d) => {
                let mc = comp(&d, "SWeaponModifierComponentParams");
                if truthy(mc) {
                    flatten(g(g(mc, "modifier"), "weaponStats"))
                } else {
                    vec![]
                }
            }
            None => vec![],
        };
        self.mods.lock().unwrap().insert(cls.to_string(), flat.clone());
        flat
    }

    /// `[{path, label, value, kind, ...}]` of the non-neutral leaves of attachment `cls`.
    pub fn changes(&self, cls: &str) -> Vec<Value> {
        let flat = self.modifier(cls);
        let mut out = vec![];
        let mut other = 0;
        for (path, label, kind) in MOD_LABELS {
            if let Some(v) = lookup(&flat, path) {
                if !is_neutral(path, v) {
                    out.push(json!({"path": path, "label": label, "value": pyround(v, 4), "kind": kind}));
                }
            }
        }
        for (path, v) in &flat {
            if !MOD_LABELS.iter().any(|(p, _, _)| p == path) && !is_neutral(path, *v) {
                other += 1;
            }
        }
        if other > 0 {
            out.push(json!({"path": "", "label": "Other recoil / noise terms", "value": other, "kind": "n"}));
        }
        out
    }

    /// Product of the multiplier `path` over the attachment classes (additive leaves: the sum).
    pub fn factor(&self, classes: &[String], path: &str) -> f64 {
        let add = NEUTRAL_ADDITIVE.contains(&path.rsplit('.').next().unwrap_or(path));
        let mut acc = if add { 0.0 } else { 1.0 };
        for c in classes {
            if let Some(v) = lookup(&self.modifier(c), path) {
                acc = if add { acc + v } else { acc * v };
            }
        }
        acc
    }

    // ---- non-constant damage
    /// One fire action at heat level `h`: ([alpha, rate, dps, heat level per second], kind).
    #[allow(clippy::too_many_arguments)]
    fn stage(&self, action: &Value, ammos: &Value, fc: &dyn Fn(&str) -> f64, hs: &[(String, f64)], h: f64, overheat: f64, ship: bool) -> Option<Stage> {
        let kind = struct_of(action).replace("SWeaponAction", "").replace("Params", "").replace("Fire", "");
        let mix = |key: &str, neutral: f64| neutral + (lookup(hs, key).unwrap_or(neutral) - neutral) * h;
        let heat_mult = if ship { fc("heatGenerationMultiplier") * mix("heatGenerationMultiplier", 1.0) } else { 1.0 };
        if kind == "Beam" {
            let dps = dsum(&damage(g(action, "damagePerSecond"))) * fc("damageMultiplier") * mix("damageMultiplier", 1.0);
            let hp = num(g(action, "heatPerSecond"));
            let heat = match hp {
                Some(hp) if hp != 0.0 => hp * heat_mult / overheat,
                _ => 0.0,
            };
            return Some(([None, None, Some(dps), Some(heat)], kind));
        }
        let rate = num(g(action, "fireRate"));
        let launch = g(action, "launchParams");
        let rate = match rate {
            Some(r) if truthy(launch) => r,
            _ => return None,
        };
        let ptype = if truthy(g(launch, "projectileType")) { s(g(launch, "projectileType")) } else { "Primary" };
        let ammo = {
            let a = g(ammos, ptype);
            if truthy(a) {
                a
            } else {
                g(ammos, "Primary")
            }
        };
        let per_hit = damage(g(g(ammo, "projectileParams"), "damage"));
        if per_hit.is_empty() {
            return None;
        }
        let mult = if g(launch, "damageMultiplier").is_null() { 1.0 } else { f(g(launch, "damageMultiplier")) };
        let pellets = if truthy(g(launch, "pelletCount")) { f(g(launch, "pelletCount")) } else { 1.0 };
        let alpha = dsum(&per_hit) * pellets * mult * fc("damageMultiplier") * mix("damageMultiplier", 1.0);
        let rate = (rate + fc("fireRate") + (mix("fireRate", 0.0)).trunc()) * fc("fireRateMultiplier") * mix("fireRateMultiplier", 1.0);
        let hps = num(g(action, "heatPerShot"));
        let heat = match hps {
            Some(x) if x != 0.0 => x * rate / 60.0 * heat_mult / overheat,
            _ => 0.0,
        };
        Some(([Some(alpha), Some(rate), Some(alpha * rate / 60.0), Some(heat)], kind))
    }

    /// `{'Primary': AmmoParams, 'Secondary': AmmoParams}` of the equipped magazine / the weapon's own container (a missing one is absent).
    fn ammos(&self, data: &Value, classes: &[String]) -> Value {
        let (cont, ammo, _src) = self.ammo_source(data, classes);
        let mut out = Map::new();
        if let Some(a) = ammo.filter(truthy) {
            out.insert("Primary".into(), a);
        }
        if let Some(sec) = self.stats.rref(g(&cont, "secondaryAmmoParamsRecord"), 14).filter(truthy) {
            out.insert("Secondary".into(), sec);
        }
        Value::Object(out)
    }

    /// Damage model of a weapon whose damage depends on how hot it is (volt_lmg_energy_01 'Fresnel', volt_pistol_energy_01,
    /// volt_rifle_energy_01 'Parallax', klwe_pistol_energy_01 'Arclight', volt_shotgun_energy_01 'Prism'), or None.
    ///
    /// Two EMPIRICAL sources, both read from the first fire action: (1) `connectionParams.heatStats` (a SWeaponStats: e.g. Fresnel
    /// damageMultiplier 6, fireRateMultiplier 0.2) and (2) a `SWeaponActionDynamicConditionParams` whose `SWeaponConditionHeatLevel`
    /// conditions switch the action above a heat level.  CONFIRMED: heat level = temperature /
    /// overheatTemperature; heatStats blends linearly from neutral at 0 to its value at 1 (integer terms truncated); heat rises by the raw
    /// heatPerShot x (blended) shots/s with no cooling while firing; the conditions are tried in array order, first met wins, else the
    /// default action.  `{kind 'ramp', overheat, cooling, cool_delay, thresholds, stats, stages, curve: [[heat, alpha|None, rate|None,
    /// dps, heat level per second, stage index]...], t_overheat, t_switch}`; a switch is two curve rows at the same heat.
    pub fn heat_curve(&self, data: &Value, classes: &[String], wc: &Value, steps: usize) -> Option<Value> {
        let fas: Vec<&Value> = items(g(wc, "fireActions")).collect();
        let top = *fas.first()?;
        let fc = |p: &str| self.factor(classes, p);
        let heat_params = g(wc, "connectionParams");
        let hs: Vec<(String, f64)> = flatten(g(heat_params, "heatStats")).into_iter().filter(|(k, v)| MOD_STATS.contains(&k.as_str()) && !is_neutral(k, *v)).collect();
        let mut conds: Vec<(&Value, f64, &Value)> = vec![];
        let mut default: Option<&Value> = None;
        if struct_of(top) == "SWeaponActionDynamicConditionParams" {
            for c in arr(g(top, "conditionalWeaponActions")) {
                let cond = g(c, "condition");
                let cmp = g(cond, "comparer");
                let bad_mode = comparer_mode(g(cmp, "mode")).is_none();
                if struct_of(cond) != "SWeaponConditionHeatLevel" || bad_mode || !truthy(g(c, "weaponAction")) {
                    return None;
                }
                conds.push((g(cmp, "mode"), f(g(cmp, "value")), g(c, "weaponAction")));
            }
            default = Some(g(top, "defaultWeaponAction")).filter(|d| !d.is_null());
        }
        if conds.is_empty() && hs.is_empty() {
            return None;
        }
        let default = default.unwrap_or(top);
        let first = |action: &Value| -> Value {
            let tmp = json!({"fireActions": [action]});
            let m = ItemStats::modes(&tmp);
            m.first().map_or_else(|| action.clone(), |x| x.1.clone())
        };
        let ammos = self.ammos(data, classes);
        let sh = g(heat_params, "simplifiedHeatParams");
        let over = num(g(sh, "overheatTemperature")).filter(|x| *x != 0.0).unwrap_or(100.0);
        let mut cuts: Vec<f64> = vec![0.0, 1.0];
        for (_, v, _) in &conds {
            if 0.0 < *v && *v < 1.0 && !cuts.contains(v) {
                cuts.push(*v);
            }
        }
        cuts.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let ship = is_ship_weapon(data);
        let mut stages: Vec<Value> = vec![];
        let mut curve: Vec<[Option<f64>; 6]> = vec![];
        for w in cuts.windows(2) {
            let (lo, hi) = (w[0], w[1]);
            let mid = (lo + hi) / 2.0;
            let pick = conds.iter().find(|(m, v, _)| met(m, mid, *v)).map_or(default, |(_, _, a)| *a);
            if pick.is_null() {
                return None;
            }
            let act = first(pick);
            let n = ((steps as f64 * (hi - lo)).round_ties_even() as i64).max(2);
            for i in 0..=n {
                let h = lo + (hi - lo) * i as f64 / n as f64;
                let (row, kind) = self.stage(&act, &ammos, &fc, &hs, h, over, ship)?;
                if i == 0 {
                    let nm = if truthy(g(&act, "name")) { g(&act, "name").clone() } else { json!(kind) };
                    stages.push(json!({"name": nm, "kind": kind, "rate_scaled": row[1].is_some()}));
                }
                let r4 = |x: Option<f64>| x.map(|x| pyround(x, 4));
                curve.push([Some(pyround(h, 4)), r4(row[0]), r4(row[1]), r4(row[2]), r4(row[3]), Some((stages.len() - 1) as f64)]);
            }
        }
        let mut dps_set: Vec<u64> = curve.iter().map(|r| r[3].unwrap_or(f64::NAN).to_bits()).collect();
        dps_set.sort_unstable();
        dps_set.dedup();
        if dps_set.len() < 2 {
            return None;
        }
        let mut t: Option<f64> = Some(0.0);
        let mut t_switch: Option<f64> = None;
        let mut t_switch_set = false;
        for ab in curve.windows(2) {
            let (a, b) = (&ab[0], &ab[1]);
            if b[0].unwrap() > a[0].unwrap() {
                if let Some(tv) = t {
                    let rate = (a[4].unwrap() + b[4].unwrap()) / 2.0;
                    t = if rate > 0.0 { Some(tv + (b[0].unwrap() - a[0].unwrap()) / rate) } else { None };
                }
            }
            if !t_switch_set && a[5] != b[5] {
                t_switch = t.map(|x| pyround(x, 3));
                t_switch_set = t_switch.is_some();
            }
        }
        let mut stats_o = Map::new();
        for (k, v) in &hs {
            stats_o.insert(k.clone(), json!(pyround(*v, 4)));
        }
        let curve_j: Vec<Value> = curve.iter().map(|r| Value::Array(r.iter().enumerate().map(|(i, x)| if i == 5 { json!(x.unwrap() as i64) } else { jopt(*x) }).collect())).collect();
        Some(json!({
            "kind": "ramp", "overheat": over, "cooling": jopt(num(g(sh, "coolingPerSecond"))), "cool_delay": jopt(num(g(sh, "timeTillCoolingStarts"))),
            "ship": ship, "min_temperature": jopt(num(g(sh, "minTemperature"))), "fix_time": jopt(num(g(sh, "overheatFixTime"))),
            "temp_after_fix": jopt(num(g(sh, "temperatureAfterOverheatFix"))), "thresholds": cuts[1..cuts.len() - 1].to_vec(),
            "stats": stats_o, "stages": stages, "curve": curve_j,
            "t_overheat": jopt(t.map(|x| pyround(x, 3))), "t_switch": jopt(t_switch)
        }))
    }

    /// Damage model of a `SWeaponActionFireChargedParams` (Zenith: 42.5 per shot uncharged, x2 at full charge after 3.5 s), or None when the
    /// action neither gates the shot nor changes a stat.
    ///
    /// `{kind 'charge', charge_time, overcharge_time, overcharged_time, cooldown, interpolate, gated, auto_charge, max_modifier, curve:
    /// [[charge fraction, alpha, rate, dps], ...]}`.  CONFIRMED:
    /// `maxChargeModifier` (a SWeaponStats) is blended with the fraction (multipliers 1 + (m-1) x fraction, additive terms truncated to int)
    /// when `interpolateChargeBonus` is set, else it is a STEP at fraction >= 1.  `fireOnlyOnFullCharge` = releasing early fizzles: damage
    /// is constant, the curve has one point.
    pub fn charge_curve(&self, outer: &Value, inner: &Value, data: &Value, classes: &[String], points: usize) -> Option<Value> {
        let fc = |p: &str| self.factor(classes, p);
        let ammos = self.ammos(data, classes);
        let per_hit = damage(g(g(g(&ammos, "Primary"), "projectileParams"), "damage"));
        let launch = g(inner, "launchParams");
        let rate0 = num(g(inner, "fireRate"));
        let rate0 = match rate0 {
            Some(r) if !per_hit.is_empty() => r,
            _ => return None,
        };
        let mm = flatten(g(outer, "maxChargeModifier"));
        let modv: Vec<(String, f64)> = mm.into_iter().filter(|(k, v)| MOD_STATS.contains(&k.as_str()) && !is_neutral(k, *v)).collect();
        let gated = truthy(g(outer, "fireOnlyOnFullCharge"));
        let interp = truthy(g(outer, "interpolateChargeBonus"));
        if modv.is_empty() && !gated {
            return None;
        }
        let p0 = if truthy(g(launch, "pelletCount")) { f(g(launch, "pelletCount")) } else { 1.0 };
        let m0 = if g(launch, "damageMultiplier").is_null() { 1.0 } else { f(g(launch, "damageMultiplier")) };
        let md = |k: &str, d: f64| lookup(&modv, k).unwrap_or(d);
        let at = |c: f64| -> (f64, f64) {
            let dm = 1.0 + (md("damageMultiplier", 1.0) - 1.0) * c;
            let rm = 1.0 + (md("fireRateMultiplier", 1.0) - 1.0) * c;
            let rate = ((rate0 + (md("fireRate", 0.0) * c).trunc()) * rm + fc("fireRate")) * fc("fireRateMultiplier");
            (dsum(&per_hit) * (p0 + (md("pellets", 0.0) * c).trunc()) * m0 * dm * fc("damageMultiplier"), rate)
        };
        let pts: Vec<(f64, f64)> = if gated || modv.is_empty() {
            vec![(1.0, 1.0)]
        } else if interp {
            (0..points).map(|i| (i as f64 / (points - 1) as f64, i as f64 / (points - 1) as f64)).collect()
        } else {
            vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)]
        };
        let mut curve = vec![];
        for (x, c) in pts {
            let (a, r) = at(c);
            curve.push(json!([pyround(x, 4), pyround(a, 4), pyround(r, 3), pyround(a * r / 60.0, 3)]));
        }
        let mut mmod = Map::new();
        for (k, v) in &modv {
            mmod.insert(k.clone(), json!(pyround(*v, 4)));
        }
        Some(json!({
            "kind": "charge", "charge_time": pyround(num(g(outer, "chargeTime")).unwrap_or(0.0) * fc("chargeTimeMultiplier"), 4),
            "overcharge_time": jopt(num(g(outer, "overchargeTime"))), "overcharged_time": jopt(num(g(outer, "overchargedTime"))),
            "cooldown": jopt(num(g(outer, "cooldownTime"))), "interpolate": interp, "gated": gated, "auto_charge": truthy(g(outer, "chargeAutomatically")),
            "fire_on_full": truthy(g(outer, "fireAutomaticallyOnFullCharge")), "max_modifier": mmod, "curve": curve
        }))
    }

    /// `[{mode, kind, ...}]` non-constant damage models of the weapon (`heat_curve` and `charge_curve`), `[]` for a constant-damage
    /// weapon.  Each gets `alpha_min / alpha_max / dps_min / dps_max`; for a charged weapon `dps_max` is one full-charge shot per (charge
    /// time + cooldown) (INFERRED), `dps_min` the uncharged rate of fire.
    pub fn damage_models(&self, cls: &str, classes: &[String], data: Option<&Value>) -> Vec<Value> {
        let owned;
        let data = match data {
            Some(d) => d,
            None => match self.stats.record(cls, 14) {
                Some(d) => {
                    owned = d;
                    &owned
                }
                None => return vec![],
            },
        };
        let wc = comp(data, "SCItemWeaponComponentParams");
        if !truthy(wc) {
            return vec![];
        }
        let mut out = vec![];
        if let Some(mut heat) = self.heat_curve(data, classes, wc, 20) {
            let rows = heat["curve"].as_array().unwrap().clone();
            let al: Vec<f64> = rows.iter().filter_map(|r| num(&r[1])).collect();
            let dmax = rows.iter().map(|r| f(&r[3])).fold(f64::NEG_INFINITY, f64::max);
            let o = heat.as_object_mut().unwrap();
            o.insert("mode".into(), json!(""));
            o.insert("alpha_min".into(), rows[0][1].clone());
            o.insert("alpha_max".into(), if al.is_empty() { Value::Null } else { json!(al.iter().cloned().fold(f64::NEG_INFINITY, f64::max)) });
            o.insert("dps_min".into(), rows[0][3].clone());
            o.insert("dps_max".into(), json!(dmax));
            out.push(heat);
        }
        fn walk(me: &WeaponStats, fa: &Value, data: &Value, classes: &[String], out: &mut Vec<Value>) {
            match struct_of(fa) {
                "SWeaponActionFireChargedParams" => {
                    if let Some(mut m) = me.charge_curve(fa, g(fa, "weaponAction"), data, classes, 11) {
                        let c = m["curve"].as_array().unwrap().clone();
                        let gated = truthy(&m["gated"]);
                        let cyc = if gated { 0.0 } else { f(&m["charge_time"]) + num(&m["cooldown"]).unwrap_or(0.0) };
                        let last = c.last().unwrap();
                        let alt = if cyc > 0.0 { json!(pyround(f(&last[1]) / cyc, 3)) } else { last[3].clone() };
                        let dmax = if f(&c[0][3]) >= f(&alt) { c[0][3].clone() } else { alt };
                        let o = m.as_object_mut().unwrap();
                        o.insert("mode".into(), if truthy(g(fa, "name")) { g(fa, "name").clone() } else { json!("") });
                        o.insert("alpha_min".into(), c[0][1].clone());
                        o.insert("alpha_max".into(), last[1].clone());
                        o.insert("dps_min".into(), c[0][3].clone());
                        o.insert("dps_max".into(), dmax);
                        out.push(m);
                    }
                }
                "SWeaponActionSequenceParams" => {
                    for e in arr(g(fa, "sequenceEntries")) {
                        if truthy(e) && truthy(g(e, "weaponAction")) {
                            walk(me, g(e, "weaponAction"), data, classes, out);
                        }
                    }
                }
                _ => {}
            }
        }
        for fa in items(g(wc, "fireActions")) {
            walk(self, fa, data, classes, &mut out);
        }
        out
    }

    /// (AmmoContainer component, AmmoParams dict, class that provides it): the magazine among `classes` (an attachment with an ammo
    /// container), else the weapon's own container, else its default magazine (`ammoContainerRecord`).
    pub fn ammo_source(&self, data: &Value, classes: &[String]) -> AmmoSource {
        let st = self.stats;
        for c in classes {
            if let Some(d) = st.record(c, 6).filter(truthy) {
                let cont = comp(&d, "SAmmoContainerComponentParams");
                if truthy(cont) {
                    return (cont.clone(), st.rref(g(cont, "ammoParamsRecord"), 14), Some(c.clone()));
                }
            }
        }
        let cont = comp(data, "SAmmoContainerComponentParams");
        if truthy(cont) {
            return (cont.clone(), st.rref(g(cont, "ammoParamsRecord"), 14), None);
        }
        let wc = comp(data, "SCItemWeaponComponentParams");
        if let Some(mag) = st.rref(g(wc, "ammoContainerRecord"), 6).filter(truthy) {
            let cont = comp(&mag, "SAmmoContainerComponentParams");
            if truthy(cont) {
                return (cont.clone(), st.rref(g(cont, "ammoParamsRecord"), 14), None);
            }
        }
        (Value::Null, None, None)
    }

    /// `{type: damage of one shot}` of the item (non-zero types only): a missile / bomb = its explosion damage; a gun = the ammunition's
    /// projectile damage per type times pellets and the fire action's damageMultiplier.  `[]` when the record has no damage data.
    pub fn damage_by_type(&self, cls: &str, data: Option<&Value>) -> Vec<(String, f64)> {
        let owned;
        let data = match data {
            Some(d) => d,
            None => match self.stats.record(cls, 14) {
                Some(d) => {
                    owned = d;
                    &owned
                }
                None => return vec![],
            },
        };
        let mc = comp(data, "SCItemMissileParams");
        if truthy(mc) {
            return dmg_vec(&damage(g(g(mc, "explosionParams"), "damage")));
        }
        let wc = comp(data, "SCItemWeaponComponentParams");
        if !truthy(wc) {
            return vec![];
        }
        let (_c, ammo, _s) = self.ammo_source(data, &[]);
        let ammo = ammo.unwrap_or(Value::Null);
        let per_hit = damage(g(g(&ammo, "projectileParams"), "damage"));
        let modes = ItemStats::modes(wc);
        if per_hit.is_empty() || modes.is_empty() {
            return dmg_vec(&per_hit);
        }
        let launch = g(modes[0].1, "launchParams");
        let k = (if truthy(g(launch, "pelletCount")) { f(g(launch, "pelletCount")) } else { 1.0 })
            * (if g(launch, "damageMultiplier").is_null() { 1.0 } else { f(g(launch, "damageMultiplier")) });
        per_hit.iter().map(|(t, v)| (t.to_string(), v * k)).collect()
    }

    /// Numbers of the weapon with the attachments `classes` on it: rate, alpha, dps, speed, range, ammo, spread_min, spread_max,
    /// recoil_mm, aim_recoil, zoom, ammo_cost, heat, mass, falloff_start, falloff_per_m (keys without data are absent).  The first
    /// fire action is the primary mode.
    pub fn metrics(&self, cls: &str, classes: &[String], data: Option<&Value>) -> Map<String, Value> {
        let st = self.stats;
        let owned;
        let data: &Value = match data {
            Some(d) => d,
            None => match st.record(cls, 14) {
                Some(d) => {
                    owned = d;
                    &owned
                }
                None => {
                    owned = Value::Null;
                    &owned
                }
            },
        };
        let wc = comp(data, "SCItemWeaponComponentParams");
        let mut out = Map::new();
        if truthy(data) {
            let m = g(g(comp(data, "SEntityPhysicsControllerParams"), "PhysType"), "Mass");
            if num(m).map_or(false, |x| x != 0.0) {
                out.insert("mass".into(), json!(pyround(f(m), 3)));
            }
        }
        if !truthy(wc) {
            return out;
        }
        let fc = |p: &str| self.factor(classes, p);
        let modes = ItemStats::modes(wc);
        let (cont, ammo, _src) = self.ammo_source(data, classes);
        let ammo = ammo.unwrap_or(Value::Null);
        let proj = g(&ammo, "projectileParams");
        let per_hit = damage(g(proj, "damage"));
        if let Some((_outer, fa)) = modes.first() {
            let launch = g(fa, "launchParams");
            let pellets = if truthy(g(launch, "pelletCount")) { f(g(launch, "pelletCount")) } else { 1.0 };
            let mult = if g(launch, "damageMultiplier").is_null() { 1.0 } else { f(g(launch, "damageMultiplier")) };
            if let Some(rate) = num(g(fa, "fireRate")) {
                out.insert("rate".into(), json!(pyround((rate + fc("fireRate")) * fc("fireRateMultiplier"), 3)));
            }
            if !per_hit.is_empty() {
                // a Parallel first action (Tripledown: three barrels, `weaponActions`) fires all its children on one pull: the pull's alpha is their sum
                let first_top = arr(g(wc, "fireActions")).iter().find(|a| truthy(a));
                // likewise a personal weapon's `Automatically` Sequence (Deadrig "Burst": two barrels, one pull runs every entry; INFERRED, the lab fires them together)
                let pers = s(g(g(comp(data, "SAttachableComponentParams"), "AttachDef"), "Type")) == "WeaponPersonal";
                let seq = first_top.filter(|a| pers && struct_of(a) == "SWeaponActionSequenceParams" && s(g(a, "mode")) == "Automatically");
                // only when the entries are the same shot (rate, pellets, multiplier, ammo cost: the lab's `volley` rule, spread may differ); otherwise the first entry stands for the mode
                let sig = |e: &Value| {
                    let a = g(e, "weaponAction");
                    let l = g(a, "launchParams");
                    format!("{:?}|{:?}|{:?}|{:?}", g(a, "fireRate"), g(l, "pelletCount"), g(l, "damageMultiplier"), g(l, "ammoCost"))
                };
                let same = seq.map_or(false, |a| {
                    let es: Vec<&Value> = arr(g(a, "sequenceEntries")).iter().filter(|e| truthy(e) && truthy(g(e, "weaponAction"))).collect();
                    es.len() > 1 && es.iter().all(|e| sig(e) == sig(es[0]))
                });
                let par: f64 = if let (Some(a), true) = (seq, same) {
                    arr(g(a, "sequenceEntries"))
                        .iter()
                        .filter(|e| truthy(e) && truthy(g(e, "weaponAction")))
                        .map(|e| {
                            let l = g(g(g(e, "weaponAction"), "launchParams"), "pelletCount");
                            let m = g(g(g(e, "weaponAction"), "launchParams"), "damageMultiplier");
                            let reps = if truthy(g(e, "repetitions")) { f(g(e, "repetitions")).max(1.0) } else { 1.0 };
                            reps * (if truthy(l) { f(l) } else { 1.0 }) * (if m.is_null() { 1.0 } else { f(m) })
                        })
                        .sum()
                } else {
                    match first_top.filter(|a| struct_of(a) == "SWeaponActionParallelParams") {
                    Some(a) => arr(g(a, "weaponActions"))
                        .iter()
                        .filter(|c| truthy(c))
                        .map(|c| {
                            let l = g(g(c, "launchParams"), "pelletCount");
                            let m = g(g(c, "launchParams"), "damageMultiplier");
                            (if truthy(l) { f(l) } else { 1.0 }) * (if m.is_null() { 1.0 } else { f(m) })
                        })
                        .sum(),
                    None => pellets * mult,
                    }
                };
                let alpha = pyround(dsum(&per_hit) * par * fc("damageMultiplier"), 4);
                out.insert("alpha".into(), json!(alpha));
                if let Some(r) = out.get("rate").map(f).filter(|r| *r != 0.0) {
                    out.insert("dps".into(), json!(pyround(alpha * r / 60.0, 3)));
                }
            }
            let sp = g(launch, "spreadParams");
            if truthy(sp) {
                let add = fc("spreadModifier.additiveModifier");
                out.insert("spread_min".into(), json!(pyround(f(g(sp, "min")) * fc("spreadModifier.minMultiplier") + add, 4)));
                out.insert("spread_max".into(), json!(pyround(f(g(sp, "max")) * fc("spreadModifier.maxMultiplier") + add, 4)));
            }
            if num(g(launch, "ammoCost")).is_some() {
                out.insert("ammo_cost".into(), g(launch, "ammoCost").clone());
            }
            if num(g(fa, "heatPerShot")).map_or(false, |x| x != 0.0) {
                out.insert("heat".into(), json!(pyround(f(g(fa, "heatPerShot")) * fc("heatGenerationMultiplier"), 4)));
            }
            let rec = if truthy(g(fa, "recoil")) { st.rref(g(fa, "recoil"), 8) } else { None }.unwrap_or(Value::Null);
            let kick = g(g(g(g(&rec, "weaponProceduralHandsRecoil"), "curveRecoil"), "positionCurves"), "yMaxValue");
            if num(kick).map_or(false, |x| x != 0.0) {
                out.insert("recoil_mm".into(), json!(pyround(f(kick).abs() * 1000.0 * fc("recoilModifier.curveRecoil.positionModifiers.yMaxValueModifier"), 3)));
            }
            let at = s(g(g(comp(data, "SAttachableComponentParams"), "AttachDef"), "Type"));
            if truthy(&rec) && truthy(g(fa, "recoil")) && at == "WeaponPersonal" {
                // Crafting-only factor cards (neutral 1.0): GPP_Weapon_Recoil_Handling / _Smoothness (CONFIRMED).
                out.insert("recoil_handling".into(), json!(1.0));
                out.insert("recoil_smooth".into(), json!(1.0));
            }
        }
        let pm = fc(AIM_PITCH);
        let at = s(g(g(comp(data, "SAttachableComponentParams"), "AttachDef"), "Type"));
        let fps = at == "WeaponPersonal"; // baseline 1.0 so that attachments show as deltas, not as 'new'
        let detail = if fps { self.recoil_detail(cls, classes, Some(data)) } else { None };
        let pitch = detail.as_ref().and_then(|d| arr(g(d, "rows")).iter().find(|r| s(g(r, "id")) == "aim_pitch"));
        if pitch.map_or(false, |p| truthy(g(p, "base"))) {
            out.insert("aim_recoil_deg".into(), json!(pyround(f(g(pitch.unwrap(), "value")), 4)));
        } else if fps || classes.iter().any(|c| lookup(&self.modifier(c), AIM_PITCH).is_some()) || pm != 1.0 {
            out.insert("aim_recoil".into(), json!(pyround(pm, 4)));
        }
        if detail.is_some() && fps {
            out.insert("recoil_kick".into(), json!(1.0));
        }
        let mut zoom = 1.0;
        for c in classes {
            if let Some(z) = lookup(&self.modifier(c), "aimModifier.zoomScale").filter(|z| *z != 0.0) {
                zoom *= z;
            }
        }
        if fps || zoom != 1.0 {
            out.insert("zoom".into(), json!(pyround(zoom, 3)));
        }
        if truthy(&ammo) {
            let speed = num(g(&ammo, "speed"));
            let life = num(g(&ammo, "lifetime"));
            if let Some(sp) = speed.filter(|x| *x != 0.0) {
                let v = pyround(sp * fc("projectileSpeedMultiplier"), 3);
                out.insert("speed".into(), json!(v));
                if let Some(l) = life.filter(|x| *x != 0.0) {
                    out.insert("range".into(), json!(pyround(v * l, 2)));
                }
            }
            let drop = g(proj, "damageDropParams");
            let start = damage(g(drop, "damageDropMinDistance"));
            let per_m = damage(g(drop, "damageDropPerMeter"));
            if !start.is_empty() {
                out.insert("falloff_start".into(), json!(pyround(start.iter().map(|x| x.1).fold(f64::NEG_INFINITY, f64::max), 3)));
            }
            if !per_m.is_empty() {
                out.insert("falloff_per_m".into(), json!(pyround(per_m.iter().map(|x| x.1).fold(f64::NEG_INFINITY, f64::max), 5)));
            }
        }
        if truthy(&cont) && truthy(g(&cont, "maxAmmoCount")) {
            out.insert("ammo".into(), g(&cont, "maxAmmoCount").clone());
        }
        out
    }

    /// Per-axis recoil: (row id, group, label, unit, crafted property, path of the record below the recoil record, attachment modifier
    /// path, scale).  Crafted property (CONFIRMED,
    /// multiplicative, neutral 1.0): kick -> hands position / rotation max values, handling -> aim yaw / pitch max degrees + aim noise
    /// yaw / pitch / roll max, smooth -> the 12 head-recoil offset / noise floats.
    #[allow(clippy::type_complexity)]
    fn recoil_rows() -> Vec<(String, &'static str, String, &'static str, &'static str, String, String, f64)> {
        let mut rows = vec![];
        let hands = "weaponProceduralHandsRecoil.curveRecoil.";
        for ax in ["x", "y", "z"] {
            rows.push((
                format!("hand_pos_{}", ax),
                "Hand kick",
                format!("position {}", ax.to_uppercase()),
                "mm",
                "kick",
                format!("{}positionCurves.{}MaxValue", hands, ax),
                format!("recoilModifier.curveRecoil.positionModifiers.{}MaxValueModifier", ax),
                1000.0,
            ));
        }
        for ax in ["x", "y", "z"] {
            rows.push((
                format!("hand_rot_{}", ax),
                "Hand kick",
                format!("rotation {}", ax.to_uppercase()),
                "",
                "kick",
                format!("{}rotationCurves.{}MaxValue", hands, ax),
                format!("recoilModifier.curveRecoil.rotationModifiers.{}MaxValueModifier", ax),
                1.0,
            ));
        }
        let aim = "weaponProceduralAimRecoil.curveAimRecoil.";
        let am = "recoilModifier.aimRecoilModifier.curveRecoil.";
        for k in ["yaw", "pitch"] {
            rows.push((format!("aim_{}", k), "Aim recoil", format!("{} max", k), "deg", "handling", format!("{}{}MaxDegrees", aim, k), format!("{}{}MaxDegreesModifier", am, k), 1.0));
        }
        for ax in ["yaw", "pitch", "roll"] {
            rows.push((
                format!("aim_noise_{}", ax),
                "Aim recoil",
                format!("{} noise max", ax),
                "deg",
                "handling",
                format!("{}noiseCurves.{}NoiseMaxValue", aim, ax),
                format!("{}noiseCurvesModifier.{}NoiseMaxValueModifier", am, ax),
                1.0,
            ));
        }
        let head = "weaponProceduralHeadRecoil.curveRecoil.";
        let hm = "recoilModifier.curveRecoilHead.";
        for part in ["position", "rotation"] {
            for ax in ["x", "y", "z"] {
                rows.push((
                    format!("head_{}_off_{}", part, ax),
                    "Head recoil",
                    format!("{} offset {}", part, ax.to_uppercase()),
                    "",
                    "smooth",
                    format!("{}{}.offset.{}", head, part, ax),
                    format!("{}{}Modifier.offsetModifier.{}", hm, part, ax),
                    1.0,
                ));
            }
            for ax in ["x", "y", "z"] {
                rows.push((
                    format!("head_{}_noise_{}", part, ax),
                    "Head recoil",
                    format!("{} noise {}", part, ax.to_uppercase()),
                    "",
                    "smooth",
                    format!("{}{}.noise.{}Noise", head, part, ax),
                    format!("{}{}Modifier.noiseModifier.{}", hm, part, ax),
                    1.0,
                ));
            }
        }
        rows
    }

    /// `{rows: [{id, group, label, unit, prop, base, value}], notes: [str]}` of the first fire action's recoil record, or None without
    /// one.  `base` is the DataCore value, `value` is base x the attachments' modifier of the same field (the page then multiplies by the
    /// crafted factor of `prop`).  Rows whose DataCore base is 0 are kept unless the whole group is 0.  EMPIRICAL (218 FPS weapons with a
    /// recoil record): 198 have non-zero hands / aim curve values, 183 non-zero head values; the rest use the legacy non-curve recoil
    /// fields, which the crafted factors do not touch (INFERRED).
    pub fn recoil_detail(&self, cls: &str, classes: &[String], data: Option<&Value>) -> Option<Value> {
        let st = self.stats;
        let owned;
        let data: &Value = match data {
            Some(d) => d,
            None => {
                owned = st.record(cls, 14).unwrap_or(Value::Null);
                &owned
            }
        };
        let wc = comp(data, "SCItemWeaponComponentParams");
        let modes = if truthy(wc) { ItemStats::modes(wc) } else { vec![] };
        let rec = match modes.first() {
            Some((_, inner)) if truthy(g(inner, "recoil")) => st.rref(g(inner, "recoil"), 8),
            _ => None,
        }
        .filter(truthy)?;
        let mut rows: Vec<Value> = vec![];
        let mut groups: Vec<(&str, bool)> = vec![];
        for (rid, group, label, unit, prop, path, mpath, scale) in Self::recoil_rows() {
            let mut v = &rec;
            for part in path.split('.') {
                v = if v.is_object() { g(v, part) } else { &Value::Null };
            }
            let Some(x) = num(v) else { continue };
            let base = x * scale;
            let val = base * self.factor(classes, &mpath);
            match groups.iter_mut().find(|(k, _)| *k == group) {
                Some(slot) => slot.1 = slot.1 || base != 0.0,
                None => groups.push((group, base != 0.0)),
            }
            rows.push(json!({"id": rid, "group": group, "label": label, "unit": unit, "prop": prop, "base": pyround(base, 5), "value": pyround(val, 5)}));
        }
        let on = |gname: &str| groups.iter().find(|(k, _)| *k == gname).map_or(false, |x| x.1);
        let rows: Vec<Value> = rows.into_iter().filter(|r| on(s(g(r, "group")))).collect();
        let mut notes = vec![];
        for (gname, prop) in [("Hand kick", "Recoil kick"), ("Aim recoil", "Recoil handling"), ("Head recoil", "Recoil smoothness")] {
            if !on(gname) {
                notes.push(format!("{}: the DataCore record has no curve values (legacy recoil model), so crafted {} has nothing to scale.", gname, prop));
            }
        }
        Some(json!({"rows": rows, "notes": notes}))
    }

    // ---- the card
    /// The detailed sections of `ItemStats` for the weapon, with the ammunition read from the equipped magazine when the weapon has no
    /// ammo container of its own, plus the melee and item sections.
    pub fn sections(&self, cls: &str, classes: &[String]) -> Vec<Value> {
        let st = self.stats;
        let Some(mut data) = st.record(cls, 14) else { return vec![] };
        if comp(&data, "SAmmoContainerComponentParams").is_null() {
            let (cont, _a, _s) = self.ammo_source(&data, classes);
            if truthy(&cont) {
                if let Some(Value::Array(c)) = data.get_mut("Components") {
                    c.push(cont);
                }
            }
        }
        let mut secs: Vec<Value> = vec![];
        secs.extend(st.weapon_sections(&data).into_iter().flatten());
        secs.extend(st.missile(&data).into_iter().flatten());
        if comp(&data, "SCItemWeaponComponentParams").is_null() {
            secs.extend(self.melee_section(&data));
        }
        let phys = g(g(comp(&data, "SEntityPhysicsControllerParams"), "PhysType"), "Mass");
        let health = comp(&data, "SHealthComponentParams");
        secs.extend(section(
            "Item",
            vec![
                if num(phys).map_or(false, |x| x != 0.0) { Some(row2("Mass", json!(pyround(f(phys), 3)), "kg")) } else { None },
                Some(row1("Health", if truthy(health) { g(health, "Health").clone() } else { Value::Null })),
            ],
        ));
        secs.extend(st.distortion(&data).into_iter().flatten());
        secs
    }

    /// Rows of `SMeleeWeaponComponentParams` that are plain numbers (knives: the only weapon data of a melee item).
    pub fn melee_section(&self, data: &Value) -> Option<Value> {
        let mc = comp(data, "SMeleeWeaponComponentParams");
        if !truthy(mc) {
            return None;
        }
        let scalars: Map<String, Value> = mc.as_object()?.iter().filter(|(_, x)| !x.is_object() && !x.is_array()).map(|(k, x)| (k.clone(), x.clone())).collect();
        let mut rows: Vec<Option<Value>> = flatten(&Value::Object(scalars)).into_iter().map(|(k, v)| Some(row1(&capitalize(&humanize(&k)), json!(v)))).collect();
        let fld = g(mc, "meleeCombatConfig");
        if truthy(fld) {
            if let Some(r) = self.stats.rref(fld, 6).filter(truthy) {
                let mut dmg: Vec<(String, f64)> = flatten(&r).into_iter().filter(|(k, v)| k.rsplit('.').next().unwrap().starts_with("Damage") && *v != 0.0).collect();
                dmg.sort_by(|a, b| a.0.cmp(&b.0));
                rows.extend(dmg.into_iter().take(12).map(|(k, v)| Some(row1(&capitalize(&humanize(&k)), json!(v)))));
            }
        }
        section("Melee", rows)
    }

    /// Section 'Attachment effects' from `[{label, cls, name}]` of the equipped attachments.
    pub fn attachment_section(&self, ports: &Value) -> Option<Value> {
        let mut rows = vec![];
        for p in arr(ports) {
            for ch in self.changes(s(g(p, "cls"))) {
                let label = format!("{}: {}", s(g(p, "label")), s(&ch["label"]));
                if s(&ch["kind"]) == "n" {
                    rows.push(Some(row(&label, ch["value"].clone(), "terms", "", "")));
                } else {
                    let v = f(&ch["value"]);
                    let val = if s(&ch["kind"]) == "x" { format!("x{}", fmt_g(v, 3)) } else { fmt_gs(v, 3) };
                    rows.push(Some(row(&label, json!(val), "", "", s(g(p, "name")))));
                }
            }
        }
        section("Attachment effects", rows)
    }
}
