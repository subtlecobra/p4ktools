//! Characteristics of ship items (weapons, missiles, shields, power plants, coolers, quantum drives, thrusters ...) for the ship view's
//! loadout panel, read from the item's EntityClassDefinition record in Game2.dcb.
//!
//! Every number comes from a record field; numbers computed here are flagged `derived` (damage per shot, DPS, range = speed x lifetime
//! ...).  Layout facts (EMPIRICAL, from the live archive; the meaning of a field is the field name's, nothing was checked against engine
//! code unless noted):
//!
//! * `SAttachableComponentParams.AttachDef`: Type, SubType, Size, Grade, Manufacturer (ref -> SCItemManufacturer record) and
//!   Localization.Name / Description, keys of `Data/Localization/english/global.ini` ("@item_NameX" -> "item_NameX=...").
//! * Weapon: `SCItemWeaponComponentParams.fireActions` (Rapid / Single / Beam / Charged / Sequence), the item's
//!   `SAmmoContainerComponentParams.ammoParamsRecord` (-> AmmoParams: speed, lifetime, projectileParams.damage, penetrationParams ...);
//!   `fireRate` is rounds per minute (every sampled Rapid / Single action: 100..1600).
//! * Missile: `SCItemMissileParams`; shield: `SCItemShieldGeneratorParams`; power plant / cooler / shield power:
//!   `ItemResourceComponentParams.states[].deltas`; quantum drive `SCItemQuantumDriveParams.params`; thruster `SCItemThrusterParams`.
//! * The order of ShieldResistance / ShieldAbsorption entries (physical, energy, distortion, thermal, biochemical, stun) follows the
//!   DamageInfo field order and is INFERRED.
//!
//! The records are handled as `serde_json::Value` dicts (`Value::to_json`), see `dynv`.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use p4k::P4k;
use serde_json::{json, Map, Value};

use crate::dynv::*;
use crate::itemmaterials::ItemMaterials;

pub const DAMAGE_TYPES: [&str; 6] = ["Physical", "Energy", "Distortion", "Thermal", "Biochemical", "Stun"];
pub const CATEGORIES: [(&str, &str); 11] = [
    ("weapons", "Weapons"),
    ("missiles", "Missiles and racks"),
    ("defensive", "Countermeasures"),
    ("shields", "Shields"),
    ("power", "Power plants"),
    ("coolers", "Coolers"),
    ("qd", "Quantum / jump drives"),
    ("thrusters", "Thrusters"),
    ("sensors", "Radar and computers"),
    ("fuel", "Fuel"),
    ("other", "Other (seats, doors, controllers ...)"),
];
const TYPE_CATEGORY: [(&[&str], &str); 10] = [
    (&["WeaponGun", "WeaponMining", "WeaponPersonal", "Turret", "TurretBase", "WeaponRegenPool"], "weapons"),
    (&["Missile", "MissileLauncher", "Bomb", "BombLauncher"], "missiles"),
    (&["WeaponDefensive", "Countermeasure"], "defensive"),
    (&["Shield"], "shields"),
    (&["PowerPlant"], "power"),
    (&["Cooler"], "coolers"),
    (&["QuantumDrive", "JumpDrive"], "qd"),
    (&["MainThruster", "ManneuverThruster", "Thruster"], "thrusters"),
    (&["Radar", "Scanner", "Ping", "Computer"], "sensors"),
    (&["FuelTank", "FuelIntake", "QuantumFuelTank", "ExternalFuelTank"], "fuel"),
];
pub const RECORD: &str = "EntityClassDefinition.";

pub fn category(type_name: &str) -> &'static str {
    TYPE_CATEGORY.iter().find(|(names, _)| names.contains(&type_name)).map_or("other", |(_, c)| c)
}

/// `{type: value}` of the non-zero entries of a DamageInfo dict, in DAMAGE_TYPES order.
pub fn damage(d: &Value) -> Vec<(&'static str, f64)> {
    if !d.is_object() {
        return vec![];
    }
    DAMAGE_TYPES
        .iter()
        .filter_map(|t| {
            let v = g(d, &format!("Damage{}", t));
            if truthy(v) {
                Some((*t, f(v)))
            } else {
                None
            }
        })
        .collect()
}

/// Sum of the per-hit damage values, starting from 0.
pub fn dsum(d: &[(&str, f64)]) -> f64 {
    d.iter().fold(0.0, |a, (_, v)| a + v)
}

/// `a or b or ...`: the first truthy value (the last one when none is).
pub fn first_truthy<'a>(c: &[&'a Value]) -> &'a Value {
    c.iter().find(|v| truthy(v)).copied().unwrap_or(c[c.len() - 1])
}

/// A `("ref", n, guid)` reference with a real target (`n != 0xFFFFFFFF`) and the three-element shape.
pub fn is_ref3(v: &Value) -> bool {
    matches!(v, Value::Array(a) if a.len() == 3 && a[1].as_u64() != Some(0xFFFF_FFFF))
}

pub fn pretty(cls: &str) -> String {
    let mut out = String::new();
    let mut prev = false;
    for c in cls.chars() {
        if c == '_' {
            if !prev {
                out.push(' ');
            }
            prev = true;
        } else {
            out.push(c);
            prev = false;
        }
    }
    out
}

/// Reads item records through an `ItemMaterials` (its Game2.dcb) and the English localisation table.
pub struct ItemStats<'a> {
    pub items: &'a ItemMaterials,
    pub p4k: &'a P4k,
    loc: OnceLock<HashMap<String, String>>,
    cache: Mutex<HashMap<String, Value>>,
}

/// Splits at the line breaks of Python's `str.splitlines` (the localisation text uses that rule).
fn splitlines(s: &str) -> Vec<&str> {
    // streaming over the chars (the earlier version collected every char with its offset first: 16 bytes per input char, ~1 s for the
    // 10 MB localisation table); same line breaks and the same "\r\n is one break" rule
    let mut out = Vec::with_capacity(s.len() / 40 + 1);
    let mut start = 0;
    let mut it = s.char_indices().peekable();
    while let Some((pos, c)) = it.next() {
        let brk = matches!(c, '\n' | '\r' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}');
        if brk {
            out.push(&s[start..pos]);
            let mut end = pos + c.len_utf8();
            if c == '\r' && it.peek().is_some_and(|&(_, n)| n == '\n') {
                it.next();
                end += 1;
            }
            start = end;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

impl<'a> ItemStats<'a> {
    pub fn new(items: &'a ItemMaterials, p4k: &'a P4k) -> Self {
        ItemStats { items, p4k, loc: OnceLock::new(), cache: Mutex::new(HashMap::new()) }
    }

    // ---- plumbing
    /// Entity class record decoded to `maxdepth` levels as a JSON dict, or None.
    pub fn record(&self, cls: &str, depth: usize) -> Option<Value> {
        let d = self.items.datacore()?;
        let ids = d.record_index().get(&format!("{}{}", RECORD, cls))?;
        let rec = d.record(*ids.first()?).ok()?;
        d.record_data(&rec, depth).ok().map(|v| v.to_json())
    }

    fn table(&self) -> &HashMap<String, String> {
        self.loc.get_or_init(|| {
            let mut m = HashMap::new();
            if let Some(ent) = self.p4k.get("Data/Localization/english/global.ini") {
                if let Ok(bytes) = self.p4k.read(ent) {
                    let b = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(&bytes);
                    let text = String::from_utf8_lossy(b);
                    for line in splitlines(&text) {
                        if let Some((k, v)) = line.split_once('=') {
                            m.insert(k.trim().to_string(), v.to_string());
                        }
                    }
                }
            }
            m
        })
    }

    /// Text of a '@key' localisation reference (English), or None when the table is unavailable or lacks it.
    pub fn localise_str(&self, key: &str) -> Option<String> {
        if key.is_empty() || !key.starts_with('@') || key == "@LOC_EMPTY" {
            return None;
        }
        let v = self.table().get(&key[1..])?;
        if v.is_empty() {
            None
        } else {
            Some(v.replace("\\n", "\n"))
        }
    }

    pub fn localise(&self, key: &Value) -> Option<String> {
        self.localise_str(key.as_str()?)
    }

    /// The first component of struct `name`.
    pub fn comp<'v>(&self, data: &'v Value, name: &str) -> &'v Value {
        comp(data, name)
    }

    /// Record data behind a `["ref", n, guid]` reference, or None.
    pub fn rref(&self, r: &Value, depth: usize) -> Option<Value> {
        if !is_ref3(r) {
            return None;
        }
        let i = self.items.guid_record(r[2].as_str()?)?;
        let d = self.items.datacore()?;
        d.record_data(&d.record(i).ok()?, depth).ok().map(|v| v.to_json())
    }

    /// Record name ('Type.name') behind a reference.
    pub fn rname(&self, r: &Value) -> Option<String> {
        if !is_ref3(r) {
            return None;
        }
        let i = self.items.guid_record(r[2].as_str()?)?;
        Some(self.items.datacore()?.record_name(i).ok()?.into_owned())
    }

    // ---- identity
    pub fn blank_identity(cls: &str) -> Value {
        json!({"cls": cls, "name": cls, "type": "", "subtype": "", "size": null, "grade": null, "manufacturer": "", "category": "other"})
    }

    /// `{cls, name, type, subtype, size, grade, manufacturer, category}` for a class; light (shallow record read).
    pub fn identity(&self, cls: &str) -> Value {
        if let Some(v) = self.cache.lock().unwrap().get(cls) {
            return v.clone();
        }
        let mut out = Self::blank_identity(cls);
        let data = self.record(cls, 7);
        let att = data.as_ref().map(|d| g(comp(d, "SAttachableComponentParams"), "AttachDef").clone()).filter(truthy);
        if let Some(att) = att {
            out["type"] = json!(s(g(&att, "Type")));
            out["subtype"] = json!(s(g(&att, "SubType")));
            out["size"] = g(&att, "Size").clone();
            out["grade"] = g(&att, "Grade").clone();
            out["category"] = json!(category(s(g(&att, "Type"))));
            let loc = g(&att, "Localization");
            let mut text = self.localise(g(loc, "Name"));
            let bad = |t: &Option<String>| t.as_ref().map_or(true, |t| t.is_empty() || t.contains("PLACEHOLDER"));
            if bad(&text) && cls.ends_with("_SCItem") {
                text = self.localise_str(&format!("@item_Name{}", &cls[..cls.len() - 7]));
            }
            out["name"] = json!(if bad(&text) { pretty(cls) } else { text.unwrap() });
            if let Some(mrec) = self.rref(g(&att, "Manufacturer"), 4) {
                if truthy(&mrec) {
                    out["manufacturer"] = json!(self.localise(g(g(&mrec, "Localization"), "Name")).unwrap_or_default());
                }
            }
        } else {
            out["name"] = json!(pretty(cls));
        }
        self.cache.lock().unwrap().insert(cls.to_string(), out.clone());
        out
    }

    /// Loadout tree (scene.ships.loadout_tree) with identities and categories attached; empty-class nodes keep their children.
    pub fn tree(&self, nodes: &Value) -> Value {
        let mut out = vec![];
        for n in arr(nodes) {
            let cls = s(g(n, "cls"));
            let ident = if !cls.is_empty() { self.identity(cls) } else { Self::blank_identity("") };
            let mut m = Map::new();
            m.insert("port".into(), g(n, "port").clone());
            if let Value::Object(o) = ident {
                for (k, v) in o {
                    m.insert(k, v);
                }
            }
            for k in ["geo", "m", "helper", "sel"] {
                m.insert(k.into(), g(n, k).clone());
            }
            m.insert("children".into(), self.tree(g(n, "children")));
            out.push(Value::Object(m));
        }
        Value::Array(out)
    }

    // ---- characteristics
    /// `{identity..., description, sections}`; flag 'derived' marks numbers computed here, 'uncertain' ones whose meaning is inferred.
    pub fn item(&self, cls: &str) -> Option<Value> {
        let data = self.record(cls, 14)?;
        let mut out = self.identity(cls);
        let att = g(g(comp(&data, "SAttachableComponentParams"), "AttachDef"), "Localization");
        out["description"] = self.localise(g(att, "Description")).map_or(Value::Null, |t| json!(t));
        let mut secs = vec![];
        secs.extend(self.weapon(&data).into_iter().flatten());
        secs.extend(self.missile(&data).into_iter().flatten());
        secs.extend(self.shield(&data).into_iter().flatten());
        secs.extend(self.resources(&data).into_iter().flatten());
        secs.extend(self.qd(&data).into_iter().flatten());
        secs.extend(self.thruster(&data).into_iter().flatten());
        secs.extend(self.health(&data).into_iter().flatten());
        secs.extend(self.distortion(&data).into_iter().flatten());
        out["sections"] = Value::Array(secs);
        Some(out)
    }

    pub const KEY_ROWS: [&'static str; 11] = [
        "Generates",
        "Max shield health",
        "Max regeneration",
        "Drive speed",
        "DPS (sustained)",
        "Damage per second",
        "Alpha (damage per shot)",
        "Damage (explosion)",
        "Range",
        "Thrust capacity",
        "Health",
    ];

    /// `[[label, value, unit]]` (at most 4) of the numbers that tell candidate items apart.
    pub fn summary(&self, cls: &str) -> Value {
        let card = self.item(cls);
        let mut out = vec![];
        let mut seen: HashSet<String> = HashSet::new();
        for pref in Self::KEY_ROWS {
            'sec: for sec in arr(card.as_ref().map_or(&Value::Null, |c| g(c, "sections"))) {
                for r in arr(g(sec, "rows")) {
                    let label = s(g(r, "label"));
                    let isnum = matches!(g(r, "value"), Value::Number(_) | Value::Bool(_));
                    if label.starts_with(pref) && !seen.contains(label) && isnum {
                        seen.insert(label.to_string());
                        out.push(json!([label, g(r, "value"), s(g(r, "unit"))]));
                        break 'sec;
                    }
                }
            }
            if out.len() >= 4 {
                break;
            }
        }
        Value::Array(out)
    }

    /// (ammo container component, AmmoParams) of an item record.
    fn ammo(&self, data: &Value) -> (Value, Option<Value>) {
        let cont = comp(data, "SAmmoContainerComponentParams");
        if !truthy(cont) {
            return (Value::Null, None);
        }
        (cont.clone(), self.rref(g(cont, "ammoParamsRecord"), 14))
    }

    /// `[(outer action, inner action)]` of a weapon component's fire actions (sequences and charged actions unfolded).
    pub fn modes<'v>(wc: &'v Value) -> Vec<(&'v Value, &'v Value)> {
        fn collect<'v>(fa: &'v Value, modes: &mut Vec<(&'v Value, &'v Value)>) {
            match struct_of(fa) {
                "SWeaponActionSequenceParams" => {
                    for e in arr(g(fa, "sequenceEntries")) {
                        if truthy(e) && truthy(g(e, "weaponAction")) {
                            collect(g(e, "weaponAction"), modes);
                        }
                    }
                }
                // every child of a Parallel action fires on one pull (Tripledown: three barrels, `weaponActions`); each child is listed (CONFIRMED structure in Game2.dcb)
                "SWeaponActionParallelParams" => {
                    for c in arr(g(fa, "weaponActions")) {
                        if truthy(c) {
                            collect(c, modes);
                        }
                    }
                }
                "SWeaponActionFireChargedParams" => modes.push((fa, g(fa, "weaponAction"))),
                "SWeaponActionDynamicConditionParams" => {
                    // default action first, then the heat-triggered ones (volt weapons)
                    let mut subs = vec![g(fa, "defaultWeaponAction")];
                    subs.extend(arr(g(fa, "conditionalWeaponActions")).iter().filter(|c| truthy(c)).map(|c| g(c, "weaponAction")));
                    for sub in subs {
                        if truthy(sub) {
                            collect(sub, modes);
                        }
                    }
                }
                _ => modes.push((fa, fa)),
            }
        }
        let mut modes = vec![];
        for fa in arr(g(wc, "fireActions")) {
            if truthy(fa) {
                collect(fa, &mut modes);
            }
        }
        modes
    }

    fn weapon(&self, data: &Value) -> Vec<Option<Value>> {
        let wc = comp(data, "SCItemWeaponComponentParams");
        if !truthy(wc) {
            return vec![None];
        }
        let (cont, ammo) = self.ammo(data);
        let ammo = ammo.unwrap_or(Value::Null);
        let proj = g(&ammo, "projectileParams");
        let per_hit = damage(g(proj, "damage"));
        let mut sections = vec![];
        let mut shown: HashSet<(String, String, String)> = HashSet::new();
        for (outer, fa) in Self::modes(wc) {
            let kind = struct_of(fa).replace("SWeaponAction", "").replace("Params", "");
            let lname = self.localise(g(outer, "localisedName"));
            let name: String = match lname {
                Some(t) if !t.is_empty() => t,
                _ => {
                    let c = first_truthy(&[g(outer, "name"), g(fa, "name")]);
                    if truthy(c) {
                        pystr(c)
                    } else {
                        kind.clone()
                    }
                }
            };
            let key = (name.clone(), kind.clone(), g(fa, "fireRate").to_string());
            if !shown.insert(key) {
                continue;
            }
            let mut rows: Vec<Option<Value>> = vec![Some(row1("Mode", json!(format!("{} ({})", name, kind))))];
            let launch = g(fa, "launchParams");
            let pellets_v = if truthy(g(launch, "pelletCount")) { g(launch, "pelletCount").clone() } else { json!(1) };
            let pellets = f(&pellets_v);
            let mult = if g(launch, "damageMultiplier").is_null() { 1.0 } else { f(g(launch, "damageMultiplier")) };
            if kind == "FireBeam" {
                let dps = damage(g(fa, "damagePerSecond"));
                rows.push(Some(row2("Damage per second", json!(dsum(&dps)), "dps")));
                for (t, v) in &dps {
                    rows.push(Some(row2(&format!("  {}", t), json!(v), "dps")));
                }
                rows.push(Some(row2("Full damage range", g(fa, "fullDamageRange").clone(), "m")));
                rows.push(Some(row2("Zero damage range", g(fa, "zeroDamageRange").clone(), "m")));
                rows.push(Some(row1("Heat per second", g(fa, "heatPerSecond").clone())));
                rows.push(Some(row1("Energy draw", g(fa, "maxEnergyDraw").clone())));
            } else {
                let rate = num(g(fa, "fireRate"));
                let alpha = if per_hit.is_empty() { None } else { Some(dsum(&per_hit) * pellets * mult) };
                rows.push(Some(row2("Fire rate", jopt(rate), "rounds/min")));
                let is_charged = !std::ptr::eq(outer, fa);
                if is_charged {
                    rows.push(Some(row2("Charge time", g(outer, "chargeTime").clone(), "s")));
                    rows.push(Some(row2("Cooldown", g(outer, "cooldownTime").clone(), "s")));
                    let cm = g(g(outer, "maxChargeModifier"), "damageMultiplier");
                    rows.push(if truthy(cm) {
                        Some(row("Full-charge damage multiplier", cm.clone(), "x", "uncertain", "from maxChargeModifier; how it applies is not verified"))
                    } else {
                        None
                    });
                }
                if let Some(alpha) = alpha {
                    rows.push(Some(row("Alpha (damage per shot)", json!(alpha), "", "derived", "ammo damage x pellets x damage multiplier")));
                    for (t, v) in &per_hit {
                        rows.push(Some(row(&format!("  {}", t), json!(v * pellets * mult), "", "derived", "")));
                    }
                    if let Some(r) = rate {
                        if r != 0.0 && !is_charged {
                            rows.push(Some(row("DPS (sustained)", json!(alpha * r / 60.0), "dps", "derived", "alpha x rounds/min / 60, ignoring heat, spin-up and reload")));
                        }
                    }
                }
                rows.push(Some(row1("Pellets per shot", pellets_v.clone())));
                rows.push(Some(row1("Ammo cost per shot", g(launch, "ammoCost").clone())));
                rows.push(Some(row1("Heat per shot", g(fa, "heatPerShot").clone())));
                rows.push(Some(row2("Spin-up", g(fa, "spinUpTime").clone(), "s")));
                let sp = g(launch, "spreadParams");
                if truthy(sp) {
                    rows.push(Some(row(
                        "Spread",
                        json!(format!("{} .. {}", fmt_g(f(g(sp, "min")), 3), fmt_g(f(g(sp, "max")), 3))),
                        "deg",
                        "uncertain",
                        "unit of SSpreadParams.min/max not verified",
                    )));
                }
            }
            sections.push(section(&format!("Fire mode: {}", name), rows));
        }
        // ammunition
        let mut arows: Vec<Option<Value>> = vec![];
        if truthy(&ammo) {
            let speed = num(g(&ammo, "speed"));
            let life = num(g(&ammo, "lifetime"));
            arows.push(Some(row2("Projectile speed", jopt(speed), "m/s")));
            arows.push(Some(row2("Lifetime", jopt(life), "s")));
            if let (Some(sp), Some(li)) = (speed, life) {
                if sp != 0.0 && li != 0.0 {
                    arows.push(Some(row("Range", json!(sp * li), "m", "derived", "speed x lifetime")));
                }
            }
            let pen = g(proj, "penetrationParams");
            let pz = g(proj, "pierceabilityParams");
            arows.push(Some(row2("Penetration distance", g(pen, "basePenetrationDistance").clone(), "m")));
            arows.push(if truthy(pen) {
                Some(row2("Penetration near / far radius", json!(format!("{} / {}", fmt_g(f(g(pen, "nearRadius")), 3), fmt_g(f(g(pen, "farRadius")), 3))), "m"))
            } else {
                None
            });
            arows.push(Some(row2("Max penetration thickness", g(pz, "maxPenetrationThickness").clone(), "m")));
            arows.push(Some(row2("Impact radius", g(proj, "impactRadius").clone(), "m")));
            let note = per_hit.iter().map(|(t, v)| format!("{} {}", t, fmt_g(*v, 4))).collect::<Vec<_>>().join(", ");
            arows.push(Some(row("Damage per hit", json!(dsum(&per_hit)), "", "", &note)));
        }
        if truthy(&cont) && truthy(g(&cont, "maxAmmoCount")) {
            // energy weapons have no ammo container count (0)
            arows.push(Some(row1("Ammo count", g(&cont, "initialAmmoCount").clone())));
            arows.push(Some(row1("Max ammo", g(&cont, "maxAmmoCount").clone())));
        }
        let heat = g(g(wc, "connectionParams"), "simplifiedHeatParams");
        if truthy(heat) {
            arows.push(Some(row1("Overheat temperature", g(heat, "overheatTemperature").clone())));
            arows.push(Some(row2("Cooling", g(heat, "coolingPerSecond").clone(), "/s")));
            arows.push(Some(row2("Overheat repair time", g(heat, "overheatFixTime").clone(), "s")));
        }
        sections.push(section("Ammunition and heat", arows));
        sections
    }

    pub fn missile(&self, data: &Value) -> Vec<Option<Value>> {
        let mc = comp(data, "SCItemMissileParams");
        if !truthy(mc) {
            return vec![None];
        }
        let ex = g(mc, "explosionParams");
        let dmg = damage(g(ex, "damage"));
        let gcs = g(mc, "GCSParams");
        let tg = g(mc, "targetingParams");
        let speed = num(g(gcs, "linearSpeed"));
        let life = num(g(mc, "maxLifetime"));
        let note = dmg.iter().map(|(t, v)| format!("{} {}", t, fmt_g(*v, 5))).collect::<Vec<_>>().join(", ");
        let range = match (speed, life) {
            (Some(a), Some(b)) if a != 0.0 && b != 0.0 => Some(row("Range", json!(a * b), "m", "derived", "linearSpeed x maxLifetime; the boost phase and fuel are ignored")),
            _ => None,
        };
        let rows = vec![
            Some(row("Damage (explosion)", json!(dsum(&dmg)), "", "", &note)),
            Some(row2("Explosion radius (min / max)", json!(format!("{} / {}", fmt_g(f(g(ex, "minRadius")), 3), fmt_g(f(g(ex, "maxRadius")), 3))), "m")),
            Some(row2("Speed (GCS linearSpeed)", jopt(speed), "m/s")),
            Some(row2("Max lifetime", jopt(life), "s")),
            range,
            Some(row2("Arm time", g(mc, "armTime").clone(), "s")),
            Some(row1("Dumb-fire allowed", g(tg, "allowDumbFiring").clone())),
        ];
        let lock = vec![
            Some(row1("Tracking signal", g(tg, "trackingSignalType").clone())),
            Some(row1("Minimum signal", g(tg, "trackingSignalMin").clone())),
            Some(row2("Lock range (min / max)", json!(format!("{} / {}", fmt_g(f(g(tg, "lockRangeMin")), 5), fmt_g(f(g(tg, "lockRangeMax")), 5))), "m")),
            Some(row2("Lock time", g(tg, "lockTime").clone(), "s")),
            Some(row2("Locking angle", g(tg, "lockingAngle").clone(), "deg")),
            Some(row1("Signal amplifier", g(tg, "lockSignalAmplifier").clone())),
            Some(row2("Lock resolution radius", g(tg, "lockResolutionRadius").clone(), "m")),
        ];
        vec![section("Missile", rows), section("Targeting", lock)]
    }

    pub fn shield(&self, data: &Value) -> Vec<Option<Value>> {
        let sc = comp(data, "SCItemShieldGeneratorParams");
        if !truthy(sc) {
            return vec![None];
        }
        let mut rows = vec![
            Some(row1("Max shield health", g(sc, "MaxShieldHealth").clone())),
            Some(row2("Max regeneration", g(sc, "MaxShieldRegen").clone(), "/s")),
            Some(row2("Regen delay after damage", g(sc, "DamagedRegenDelay").clone(), "s")),
            Some(row2("Regen delay after going down", g(sc, "DownedRegenDelay").clone(), "s")),
            Some(row1("Decay ratio", g(sc, "DecayRatio").clone())),
        ];
        for (key, label) in [("ShieldResistance", "Resistance"), ("ShieldAbsorption", "Absorption")] {
            for (t, v) in DAMAGE_TYPES.iter().zip(arr(g(sc, key))) {
                if truthy(v) && (truthy(g(v, "Max")) || truthy(g(v, "Min"))) {
                    rows.push(Some(row(
                        &format!("{} {}", label, t.to_lowercase()),
                        json!(format!("{} .. {}", fmt_g(f(g(v, "Min")), 3), fmt_g(f(g(v, "Max")), 3))),
                        "",
                        "uncertain",
                        "entry order = DamageInfo order is inferred",
                    )));
                }
            }
        }
        vec![section("Shield generator", rows)]
    }

    pub fn resources(&self, data: &Value) -> Vec<Option<Value>> {
        let rc = comp(data, "ItemResourceComponentParams");
        if !truthy(rc) {
            return vec![None];
        }
        let mut rows = vec![];
        if let Some(st) = arr(g(rc, "states")).first() {
            for dl in items(g(st, "deltas")) {
                for kind in ["generation", "consumption"] {
                    let x = g(dl, kind);
                    if !truthy(x) {
                        continue;
                    }
                    let amt = g(x, "resourceAmountPerSecond");
                    let v = if amt.get("standardResourceUnits").is_some() { g(amt, "standardResourceUnits") } else { g(amt, "units") };
                    if truthy(v) {
                        rows.push(Some(row2(
                            &format!("{} {} ({})", if kind == "generation" { "Generates" } else { "Consumes" }, pystr(g(x, "resource")), pystr(g(st, "name"))),
                            v.clone(),
                            if amt.get("standardResourceUnits").is_some() { "units/s" } else { "power segments" },
                        )));
                    }
                }
            }
            let sig = g(st, "signatureParams");
            for (k, label) in [("EMSignature", "EM signature"), ("IRSignature", "IR signature")] {
                let ns = g(g(sig, k), "nominalSignature");
                if truthy(ns) {
                    rows.push(Some(row1(&format!("{} ({})", label, pystr(g(st, "name"))), ns.clone())));
                }
            }
        }
        vec![section("Power and resources", rows)]
    }

    pub fn qd(&self, data: &Value) -> Vec<Option<Value>> {
        let q = comp(data, "SCItemQuantumDriveParams");
        if !truthy(q) {
            return vec![None];
        }
        let p = g(q, "params");
        let speed = num(g(p, "driveSpeed"));
        let mut rows = vec![
            Some(row2("Drive speed", jopt(speed), "m/s")),
            match speed {
                Some(sp) if sp != 0.0 => Some(row("  = fraction of c", json!(sp / 299792458.0), "", "derived", "")),
                _ => None,
            },
            Some(row2("Spool-up time", g(p, "spoolUpTime").clone(), "s")),
            Some(row2("Cooldown", g(p, "cooldownTime").clone(), "s")),
            Some(row2("Stage 1 acceleration", g(p, "stageOneAccelRate").clone(), "m/s\u{b2}")),
            Some(row2("Stage 2 acceleration", g(p, "stageTwoAccelRate").clone(), "m/s\u{b2}")),
            Some(row2("Engage speed", g(p, "engageSpeed").clone(), "m/s")),
        ];
        let jp = g(q, "jumpParams");
        if truthy(jp) {
            rows.push(Some(row1("Jump calibration rate", g(jp, "calibrationRate").clone())));
        }
        vec![section("Quantum drive", rows)]
    }

    pub fn thruster(&self, data: &Value) -> Vec<Option<Value>> {
        let t = comp(data, "SCItemThrusterParams");
        if !truthy(t) {
            return vec![None];
        }
        vec![section(
            "Thruster",
            vec![
                Some(row1("Type", g(t, "thrusterType").clone())),
                Some(row2("Thrust capacity", g(t, "thrustCapacity").clone(), "N")),
                Some(row1("Fuel burn per 10 kN", g(t, "fuelBurnRatePer10KNewton").clone())),
                Some(row1("Min health thrust multiplier", g(t, "minHealthThrustMultiplier").clone())),
            ],
        )]
    }

    pub fn health(&self, data: &Value) -> Vec<Option<Value>> {
        let h = comp(data, "SHealthComponentParams");
        if !truthy(h) || !truthy(g(h, "Health")) {
            return vec![None];
        }
        vec![section("Durability", vec![Some(row2("Health", g(h, "Health").clone(), "hp"))])]
    }

    /// Section "Distortion": `SDistortionParams` (maximum, decay, warning ratio, power behaviour) and the distortion resistance of the health
    /// component's `DamageResistances`.  Field names and values are EMPIRICAL (Game2.dcb, e.g. POWR_AEGS_S01_Charger_SCItem: Maximum 3550,
    /// DecayDelay 1.5, DecayRate 236.6667); the unit of Maximum / DecayRate is the distortion damage unit (INFERRED: same scale as
    /// `DamageDistortion`) and the meaning of the ratios is INFERRED from their names, not read from the engine.
    pub fn distortion(&self, data: &Value) -> Vec<Option<Value>> {
        distortion_section(data)
    }

    /// Public wrappers for the sections weaponstats re-uses.
    pub fn weapon_sections(&self, data: &Value) -> Vec<Option<Value>> {
        self.weapon(data)
    }
}

/// The first component of struct `name` of a decoded entity record; Null when there is none.
pub fn comp<'v>(data: &'v Value, name: &str) -> &'v Value {
    arr(g(data, "Components")).iter().find(|c| truthy(c) && struct_of(c) == name).unwrap_or(&Value::Null)
}

/// The "Distortion" section of a decoded entity record (see [`ItemStats::distortion`]).
pub fn distortion_section(data: &Value) -> Vec<Option<Value>> {
    let d = comp(data, "SDistortionParams");
    let res = g(g(g(comp(data, "SHealthComponentParams"), "DamageResistances"), "DistortionResistance"), "Multiplier");
    if !truthy(d) && !truthy(res) {
        return vec![None];
    }
    let (max, rate, delay) = (num(g(d, "Maximum")), num(g(d, "DecayRate")), num(g(d, "DecayDelay")));
    let mut rows = vec![];
    if truthy(d) {
        let note = if max.is_some_and(|m| m >= 100_000.0) {
            "INFERRED: distortion damage that disables the component; this value is so large that the part is effectively immune"
        } else {
            "distortion damage that disables the component (INFERRED from the name Maximum)"
        };
        rows.push(Some(row("Distortion limit", jopt(max), "", "uncertain", note)));
        rows.push(Some(row2("Decay delay", jopt(delay), "s")));
        rows.push(Some(row("Decay rate", jopt(rate), "per s", "uncertain", "INFERRED unit: distortion damage removed per second")));
        if let (Some(m), Some(r)) = (max, rate) {
            if r > 0.0 {
                rows.push(Some(row("Time to recover from the limit", json!(pyround(m / r + delay.unwrap_or(0.0), 2)), "s", "derived", "delay + limit / decay rate")));
            }
        }
        rows.push(Some(row1("Warning ratio", g(d, "WarningRatio").clone())));
        rows.push(Some(row1("Recovery ratio", g(d, "RecoveryRatio").clone())));
        rows.push(Some(row1("Power ratio at the limit", g(d, "PowerRatioAtMaxDistortion").clone())));
        rows.push(Some(row1("Power changes only at the limit", g(d, "PowerChangeOnlyAtMaxDistortion").clone())));
    }
    if truthy(res) {
        rows.push(Some(row("Distortion damage multiplier", res.clone(), "x", "uncertain", "INFERRED: DamageResistances.DistortionResistance (1 = takes it all, lower = resists)")));
    }
    vec![section("Distortion", rows)]
}

#[cfg(test)]
mod distortion_tests {
    use super::*;

    #[test]
    fn distortion_section_reads_params_and_resistance() {
        let data = json!({"Components": [
            {"__struct__": "SDistortionParams", "Maximum": 3550.0, "DecayDelay": 1.5, "DecayRate": 236.6667, "WarningRatio": 0.75, "RecoveryRatio": 0.0,
             "PowerRatioAtMaxDistortion": 0.0, "PowerChangeOnlyAtMaxDistortion": true},
            {"__struct__": "SHealthComponentParams", "Health": 100.0, "DamageResistances": {"DistortionResistance": {"Multiplier": 0.5}}}]});
        let s = distortion_section(&data).into_iter().flatten().next().expect("section");
        let text = s.to_string();
        assert!(text.contains("Distortion limit") && text.contains("3550"), "{}", text);
        assert!(text.contains("Time to recover from the limit") && text.contains("16.5"), "{}", text);
        assert!(text.contains("Distortion damage multiplier") && text.contains("0.5"), "{}", text);
    }

    #[test]
    fn distortion_section_absent_without_data() {
        assert!(distortion_section(&json!({"Components": [{"__struct__": "SHealthComponentParams", "Health": 5.0}]})).into_iter().flatten().next().is_none());
    }
}
